//! Parallel, pipelined merkle trie updates.
//!
//! This splits the work of warming-up and performing the trie update across worker threads.

use crossbeam::channel::{self, Receiver, Sender};
use page_set::{FrozenSharedPageSet, PageSet};
use parking_lot::Mutex;

use nomt_core::{
    page_id::{ChildPageIndex, PageId},
    proof::{PathProof, PathProofTerminal},
    trie::{self, KeyPath, Node, ValueHash},
    trie_pos::TriePosition,
};
use seek::{Seek, Seeker};

use std::{collections::HashMap, sync::Arc};

use crate::{
    beatree::ReadTransaction as BeatreeReadTx,
    io::PagePool,
    overlay::LiveOverlay,
    page_cache::{Page, PageCache, ShardIndex},
    rw_pass_cell::WritePassEnvelope,
    store::{BucketIndex, DirtyPage, SharedMaybeBucketIndex, Store},
    task::{join_task, spawn_task, TaskResult},
    HashAlgorithm, Witness, WitnessedOperations, WitnessedPath, WitnessedRead, WitnessedWrite,
};
use threadpool::ThreadPool;

mod cache_prepopulate;
mod page_set;
mod page_walker;
mod seek;
mod worker;

pub use cache_prepopulate::prepopulate as prepopulate_cache;
pub use page_walker::UpdatedPage;

#[cfg(doc)]
use nomt_core::page_id::MAX_CHILD_INDEX;

/// Threshold representing the number of leaves required to be present in the two
/// subtrees contained in a page to be stored on disk.
/// If this threshold is not reached, the page will not be stored on disk
/// and will be constructed on the fly when needed.
pub const PAGE_ELISION_THRESHOLD: u64 = 20;

/// Bitfield used to note which child pages are elided and thus require on-the-fly reconstruction.
#[derive(Debug, PartialEq, Eq)]
pub struct ElidedChildren {
    elided: u64,
}

impl ElidedChildren {
    /// Create a new empty ElidedChildren from its raw version.
    pub fn new() -> Self {
        Self { elided: 0 }
    }

    /// Create a new ElidedChildren from its encoded version.
    pub fn from_bytes(raw: [u8; 8]) -> ElidedChildren {
        ElidedChildren {
            elided: u64::from_le_bytes(raw),
        }
    }

    /// Get raw bytes representing the `ElidedChildren`.
    pub fn to_bytes(&self) -> [u8; 8] {
        self.elided.to_le_bytes()
    }

    /// Toggle as elided or not elided a child of the page.
    pub fn set_elide(&mut self, child_page_index: ChildPageIndex, elide: bool) {
        let shift = child_page_index.to_u8() as u64;
        if elide {
            self.elided |= 1 << shift;
        } else {
            self.elided &= !(1 << shift);
        }
    }

    /// Checks if the child at `child_index` is elided.
    pub fn is_elided(&self, child_page_index: ChildPageIndex) -> bool {
        (self.elided >> child_page_index.to_u8() as u64) & 1 == 1
    }
}

/// Updated pages produced by update workers.
pub struct UpdatedPages(Vec<Vec<UpdatedPage>>);

impl UpdatedPages {
    /// Freeze, label, and iterate all the pages.
    ///
    /// Pages are 'labeled' by placing the page ID into the page data itself prior to freezing.
    ///
    /// `alloc_dependent` tells this function to allocate placeholder shared memory for storing
    /// bucket indices of fresh pages. This is meant to be used when committing into an overlay.
    pub fn into_frozen_iter(
        self,
        alloc_dependent: bool,
    ) -> impl Iterator<Item = (PageId, DirtyPage)> {
        self.0.into_iter().flatten().map(move |updated_page| {
            let page = updated_page.page.freeze();
            let dirty = DirtyPage {
                page,
                diff: updated_page.diff,
                bucket: match updated_page.bucket_info {
                    BucketInfo::Known(b) => crate::store::BucketInfo::Known(b),
                    BucketInfo::Dependent(b) => crate::store::BucketInfo::FreshOrDependent(b),
                    BucketInfo::Fresh => {
                        if alloc_dependent {
                            crate::store::BucketInfo::FreshOrDependent(SharedMaybeBucketIndex::new(
                                None,
                            ))
                        } else {
                            crate::store::BucketInfo::FreshWithNoDependents
                        }
                    }
                },
            };
            (updated_page.page_id, dirty)
        })
    }
}

/// This is akin to the store::BucketInfo, except it has a single variant for fresh pages.
///
/// Determining whether to allocate a `SharedMaybeBucketIndex` for fresh pages requires some
/// context, namely, whether this is being committed into an overlay or not. This type allows us
/// to defer that decision to later on in the pipeline.
#[derive(Clone)]
pub enum BucketInfo {
    /// The bucket index is known.
    Known(BucketIndex),
    /// The bucket index is dependent on some prior overlay.
    Dependent(SharedMaybeBucketIndex),
    /// The bucket index is fresh and to-be-allocated later.
    Fresh,
}

/// Whether a key was read or written.
#[derive(Debug, Clone)]
pub enum KeyReadWrite {
    /// The key was read.
    Read,
    /// The key was written. Contains the written value or `None` if deleted.
    Write(Option<ValueHash>),
    /// The key was both read and written. Contains the written value of `None` if deleted.
    ReadThenWrite(Option<ValueHash>),
}

impl KeyReadWrite {
    fn is_write(&self) -> bool {
        match self {
            KeyReadWrite::Read => false,
            KeyReadWrite::Write(_) | KeyReadWrite::ReadThenWrite(_) => true,
        }
    }

    fn is_read(&self) -> bool {
        match self {
            KeyReadWrite::Read | KeyReadWrite::ReadThenWrite(_) => true,
            KeyReadWrite::Write(_) => false,
        }
    }

    fn written_value(&self) -> Option<Option<ValueHash>> {
        match self {
            KeyReadWrite::Read => None,
            KeyReadWrite::Write(v) | KeyReadWrite::ReadThenWrite(v) => {
                Some(v.as_ref().map(|vh| *vh))
            }
        }
    }
}

/// The update worker pool.
pub struct UpdatePool {
    worker_tp: ThreadPool,
    do_warm_up: bool,
}

impl UpdatePool {
    /// Create a new `UpdatePool`.
    ///
    /// # Panics
    ///
    /// Panics if `num_workers` is zero.
    pub fn new(num_workers: usize, do_warm_up: bool) -> Self {
        UpdatePool {
            worker_tp: threadpool::Builder::new()
                .num_threads(num_workers)
                .thread_name("nomt-commit".to_string())
                .build(),
            do_warm_up,
        }
    }

    /// Create a `Updater` that uses the underlying pool.
    ///
    /// # Deadlocks
    ///
    /// The Updater expects to have exclusive access to the page cache, so if there
    /// are outstanding read passes, write passes, or threads waiting on write passes,
    /// deadlocks are practically guaranteed at some point during the lifecycle of the Updater.
    pub fn begin<H: HashAlgorithm>(
        &self,
        page_cache: PageCache,
        page_pool: PagePool,
        store: Store,
        overlay: LiveOverlay,
        root: Node,
    ) -> Updater {
        let params = worker::WarmUpParams {
            page_cache: page_cache.clone(),
            overlay: overlay.clone(),
            store: store.clone(),
            root,
        };

        let warm_up = if self.do_warm_up {
            Some(spawn_warm_up::<H>(&self.worker_tp, params))
        } else {
            None
        };

        let beatree_read_tx = store.read_transaction();

        Updater {
            worker_tp: self.worker_tp.clone(),
            warm_up,
            page_cache,
            root,
            store,
            page_pool,
            overlay,
            beatree_read_tx,
        }
    }
}

/// Parallel commit handler.
///
/// The expected usage is to call `warm_up` repeatedly and conclude with `commit`.
pub struct Updater {
    worker_tp: ThreadPool,
    page_cache: PageCache,
    warm_up: Option<WarmUpHandle>,
    root: Node,
    store: Store,
    page_pool: PagePool,
    overlay: LiveOverlay,
    beatree_read_tx: BeatreeReadTx,
}

impl Updater {
    /// Warm up the given key-path by pre-fetching the relevant pages.
    pub fn warm_up(&self, key_path: KeyPath) {
        if let Some(ref warm_up) = self.warm_up {
            let _ = warm_up.warmup_tx.send(WarmUpCommand { key_path });
        }
    }

    /// Update the trie with the given key-value read/write operations.
    ///
    /// Key-paths should be in sorted order
    /// and should appear at most once within the vector. Witness specifies whether or not
    /// to collect the witness of the operation.
    pub fn update_and_prove<H: HashAlgorithm>(
        self,
        read_write: Vec<(KeyPath, KeyReadWrite)>,
        witness: bool,
    ) -> std::io::Result<UpdateHandle> {
        if let Some(ref warm_up) = self.warm_up {
            let _ = warm_up.finish_tx.send(());
        }
        let shared = Arc::new(UpdateShared {
            witness,
            overlay: self.overlay.clone(),
            read_write,
            root_page_pending: Mutex::new(Vec::with_capacity(64)),
        });

        let num_workers = self.page_cache.shard_count();
        let shard_regions = (0..num_workers).map(ShardIndex::Shard).collect::<Vec<_>>();

        // receive warm-ups from worker.
        let (warm_ups, warm_page_set) = if let Some(ref warm_up) = self.warm_up {
            let output = join_task(&warm_up.output_rx)?;
            (output.paths, Some(output.pages))
        } else {
            (HashMap::new(), None)
        };
        let warm_ups = Arc::new(warm_ups);

        let write_pass = self.page_cache.new_write_pass();
        let worker_passes = write_pass.split_n(shard_regions);

        let (worker_tx, worker_rx) = crossbeam_channel::bounded(num_workers);

        for write_pass in worker_passes.into_iter() {
            let command = UpdateCommand {
                shared: shared.clone(),
                write_pass: write_pass.into_envelope(),
            };

            let params = worker::UpdateParams {
                page_cache: self.page_cache.clone(),
                page_pool: self.page_pool.clone(),
                store: self.store.clone(),
                root: self.root,
                warm_ups: warm_ups.clone(),
                warm_page_set: warm_page_set.clone(),
                command,
            };
            spawn_updater::<H>(&self.worker_tp, params, worker_tx.clone());
        }

        Ok(UpdateHandle {
            shared,
            worker_rx,
            num_workers,
        })
    }

    pub fn prove<H: HashAlgorithm>(&self, key_path: KeyPath) -> std::io::Result<PathProof> {
        let io_handle = self.store.io_pool().make_handle();

        // It's a little wasteful to create a seeker just for this, but
        // this is the simplest way to get the path proof
        let mut seeker = Seeker::<H>::new(
            self.root,
            self.beatree_read_tx.clone(),
            self.page_cache.clone(),
            self.overlay.clone(),
            io_handle,
            self.store.page_loader(),
            true, // collect witness
        );

        seeker.push(key_path);
        let mut page_set = PageSet::new(self.page_pool.clone(), None);

        // Blocking I/O loop.
        let found = loop {
            seeker.submit_all(&mut page_set);

            // Submitting all requests may have yielded a completion.
            if let Some(completion) = seeker.take_completion() {
                break completion;
            }

            // Block only if no completion was received.
            seeker.recv_page(&mut page_set)?;
        };

        let terminal = found
            .terminal
            .map(PathProofTerminal::Leaf)
            .unwrap_or_else(|| PathProofTerminal::Terminator(found.position.clone()));

        Ok(PathProof {
            terminal,
            siblings: found.siblings,
        })
    }
}

/// A handle for waiting on the results of a commit operation.
pub struct UpdateHandle {
    shared: Arc<UpdateShared>,
    worker_rx: Receiver<TaskResult<std::io::Result<WorkerOutput>>>,
    num_workers: usize,
}

impl UpdateHandle {
    /// Wait on the results of the commit operation.
    pub fn join(self) -> std::io::Result<Output> {
        let mut new_root = None;

        let mut maybe_witness = self.shared.witness.then_some(Witness {
            path_proofs: Vec::new(),
            operations: WitnessedOperations {
                reads: Vec::new(),
                writes: Vec::new(),
            },
        });

        let mut updated_pages = Vec::new();

        let mut path_proof_offset = 0;
        let mut witnessed_start = 0;

        for _ in 0..self.num_workers {
            let output = join_task(&self.worker_rx)?;

            if let Some(root) = output.root {
                assert!(new_root.is_none());
                new_root = Some(root);
            }

            updated_pages.push(output.updated_pages);

            // if the Commit worker collected the witnessed paths
            // then we need to aggregate them
            if let Some(witnessed_paths) = output.witnessed_paths {
                // UNWRAP: the same `UpdateShared` object is used to decide whether
                // to collect witnesses or not. If the commit worker did so,
                // `maybe_witness` and `maybe_witnessed_ops` must be initialized to contain
                // all witnesses from all workers.
                let witness = maybe_witness.as_mut().unwrap();

                let path_proof_count = witnessed_paths.len();
                witness.path_proofs.reserve(witnessed_paths.len());
                for (path_index, (path, leaf_data, batch_size)) in
                    witnessed_paths.into_iter().enumerate()
                {
                    witness.path_proofs.push(path);
                    let witnessed_end = witnessed_start + batch_size;
                    for (k, v) in &self.shared.read_write[witnessed_start..witnessed_end] {
                        if v.is_read() {
                            let value_hash = leaf_data.as_ref().and_then(|leaf_data| {
                                if &leaf_data.key_path == k {
                                    Some(leaf_data.value_hash)
                                } else {
                                    None
                                }
                            });

                            witness.operations.reads.push(WitnessedRead {
                                key: *k,
                                value: value_hash,
                                path_index: path_index + path_proof_offset,
                            });
                        }
                        if let Some(written) = v.written_value() {
                            witness.operations.writes.push(WitnessedWrite {
                                key: *k,
                                value: written,
                                path_index: path_index + path_proof_offset,
                            });
                        }
                    }
                    witnessed_start = witnessed_end;
                }

                path_proof_offset += path_proof_count;
            }
        }

        // UNWRAP: one thread always produces the root.
        Ok(Output {
            root: new_root.unwrap(),
            updated_pages: UpdatedPages(updated_pages),
            witness: maybe_witness,
        })
    }
}

/// The output of a commit operation.
pub struct Output {
    /// The new root.
    pub root: Node,
    /// All updated pages from all worker threads. The covered sets of pages are disjoint.
    pub updated_pages: UpdatedPages,
    /// Optional witness
    pub witness: Option<Witness>,
}

struct UpdateCommand {
    shared: Arc<UpdateShared>,
    write_pass: WritePassEnvelope<ShardIndex>,
}

struct WarmUpCommand {
    key_path: KeyPath,
}

struct WarmUpOutput {
    pages: FrozenSharedPageSet,
    paths: HashMap<KeyPath, Seek>,
}

enum RootPagePending {
    SubTrie {
        range_start: usize,
        range_end: usize,
        prev_terminal: Option<trie::LeafData>,
    },
    Node(Node),
}

struct WorkerOutput {
    root: Option<Node>,
    witnessed_paths: Option<Vec<(WitnessedPath, Option<trie::LeafData>, usize)>>,
    updated_pages: Vec<UpdatedPage>,
}

impl WorkerOutput {
    fn new(witness: bool) -> Self {
        WorkerOutput {
            root: None,
            witnessed_paths: if witness { Some(Vec::new()) } else { None },
            updated_pages: Vec::new(),
        }
    }
}

// Shared data used in committing.
struct UpdateShared {
    read_write: Vec<(KeyPath, KeyReadWrite)>,
    // nodes needing to be written to pages above a shard.
    root_page_pending: Mutex<Vec<(TriePosition, RootPagePending)>>,
    overlay: LiveOverlay,
    witness: bool,
}

impl UpdateShared {
    fn push_pending_root_nodes(&self, nodes: Vec<(TriePosition, Node)>) {
        let mut pending = self.root_page_pending.lock();
        for (trie_pos, node) in nodes {
            pending.push((trie_pos, RootPagePending::Node(node)));
        }
    }

    fn push_pending_subtrie(
        &self,
        trie_pos: TriePosition,
        range_start: usize,
        range_end: usize,
        prev_terminal: Option<trie::LeafData>,
    ) {
        self.root_page_pending.lock().push((
            trie_pos,
            RootPagePending::SubTrie {
                range_start,
                range_end,
                prev_terminal,
            },
        ));
    }

    // Takes all pending root page operations, sorted and deduplicated. Note that duplicates are
    // only expected when a pending range was encountered by multiple workers.
    fn take_root_pending(&self) -> Vec<(TriePosition, RootPagePending)> {
        let mut ops = std::mem::take(&mut *self.root_page_pending.lock());
        ops.sort_unstable_by(|a, b| a.0.path().cmp(b.0.path()));
        ops
    }
}

struct WarmUpHandle {
    finish_tx: Sender<()>,
    warmup_tx: Sender<WarmUpCommand>,
    output_rx: Receiver<TaskResult<std::io::Result<WarmUpOutput>>>,
}

fn spawn_warm_up<H: HashAlgorithm>(
    worker_tp: &ThreadPool,
    params: worker::WarmUpParams,
) -> WarmUpHandle {
    let (warmup_tx, warmup_rx) = channel::unbounded();
    let (output_tx, output_rx) = channel::bounded(1);
    let (finish_tx, finish_rx) = channel::bounded(1);

    spawn_task(
        &worker_tp,
        move || worker::run_warm_up::<H>(params, warmup_rx, finish_rx),
        output_tx,
    );

    WarmUpHandle {
        warmup_tx,
        finish_tx,
        output_rx,
    }
}

fn spawn_updater<H: HashAlgorithm>(
    worker_tp: &ThreadPool,
    params: worker::UpdateParams,
    output_tx: Sender<TaskResult<std::io::Result<WorkerOutput>>>,
) {
    spawn_task(&worker_tp, || worker::run_update::<H>(params), output_tx);
}

fn get_in_memory_page(
    overlay: &LiveOverlay,
    page_cache: &PageCache,
    page_id: &PageId,
) -> Option<(Page, BucketInfo)> {
    overlay
        .page(page_id)
        .map(|page| {
            let bucket_info = match page.bucket {
                crate::store::BucketInfo::Known(ref b) => BucketInfo::Known(*b),
                crate::store::BucketInfo::FreshOrDependent(ref maybe) => {
                    if let Some(bucket) = maybe.get() {
                        BucketInfo::Known(bucket)
                    } else {
                        BucketInfo::Dependent(maybe.clone())
                    }
                }
                // PANIC: overlays are never created with `FreshWithNoDependents`.
                crate::store::BucketInfo::FreshWithNoDependents => panic!(),
            };

            (page.page.clone(), bucket_info)
        })
        .or_else(|| {
            page_cache
                .get(page_id.clone())
                .map(|(page, bucket)| (page, BucketInfo::Known(bucket)))
        })
}
