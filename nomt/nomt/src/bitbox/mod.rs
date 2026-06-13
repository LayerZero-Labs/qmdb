use crossbeam_channel::{Receiver, Sender};
use nomt_core::page_id::PageId;
use parking_lot::{ArcRwLockReadGuard, Mutex, RwLock};
use std::{
    collections::HashSet,
    fmt,
    fs::File,
    os::{fd::AsRawFd, unix::fs::FileExt},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
};
use threadpool::ThreadPool;

use crate::{
    io::{self, page_pool::FatPage, IoCommand, IoHandle, IoKind, PagePool, PAGE_SIZE},
    page_cache::{Page, PageCache},
    store::{BucketInfo, DirtyPage},
    task::{join_task, spawn_task, TaskResult},
};

use self::{ht_file::HTOffsets, meta_map::MetaMap};

pub use self::ht_file::create;
pub use wal::WalBlobBuilder;

mod ht_file;
mod meta_map;
mod wal;
pub(crate) mod writeout;

/// During assigning a bucket to a page, the allocator gave up, meaning that the occupancy rate
/// is too high.
#[derive(fmt::Debug)]
pub struct BucketExhaustion;

impl fmt::Display for BucketExhaustion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bucket exhaustion")
    }
}

impl std::error::Error for BucketExhaustion {}

/// The index of a bucket within the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketIndex(u64);

#[cfg(test)]
impl BucketIndex {
    pub fn new(index: u64) -> Self {
        BucketIndex(index)
    }
}

/// Essentially an `Arc<Option<BucketIndex>>` that can be mutated atomically.
///
/// This is used as a shared placeholder for a bucket that _will_ be allocated in the future
/// but hasn't yet.
///
/// Typically, this will be instantiated with `None` and then `set`.
#[derive(Clone)]
pub struct SharedMaybeBucketIndex(Arc<AtomicU64>);

impl SharedMaybeBucketIndex {
    // we assume that no bucket indices reach the max value of a u64, as this would require 2^76
    // bytes of physical storage to reach, or 64 billion terabytes of page data.
    const NONE_PATTERN: u64 = u64::MAX;

    /// Create a new shared, optional bucket index.
    pub fn new(bucket: Option<BucketIndex>) -> Self {
        SharedMaybeBucketIndex(Arc::new(AtomicU64::new(match bucket {
            None => Self::NONE_PATTERN,
            Some(BucketIndex(x)) => x,
        })))
    }

    /// Set the bucket index. This uses atomic `Relaxed` ordering.
    pub fn set(&self, bucket: BucketIndex) {
        self.0.store(bucket.0, Ordering::Relaxed)
    }

    /// Get the bucket index. This uses atomic `Relaxed` ordering.
    pub fn get(&self) -> Option<BucketIndex> {
        let val = self.0.load(Ordering::Relaxed);
        if val == Self::NONE_PATTERN {
            None
        } else {
            Some(BucketIndex(val))
        }
    }
}

#[derive(Clone)]
pub struct DB {
    shared: Arc<Shared>,
}

pub struct Shared {
    page_pool: PagePool,
    store: HTOffsets,
    seed: [u8; 16],
    meta_map: Arc<RwLock<MetaMap>>,
    wal_blob_builder: Arc<Mutex<WalBlobBuilder>>,
    occupied_buckets: AtomicUsize,
    wal_fd: File,
    ht_fd: File,
    sync_tp: ThreadPool,
    capacity: usize,
}

impl DB {
    /// Opens an existing bitbox database.
    pub fn open(
        sync_seqn: u32,
        num_pages: u32,
        seed: [u8; 16],
        page_pool: PagePool,
        ht_fd: File,
        wal_fd: File,
    ) -> anyhow::Result<Self> {
        let (store, mut meta_map) = match ht_file::open(num_pages, &page_pool, &ht_fd) {
            Ok(x) => x,
            Err(e) => {
                anyhow::bail!("encountered error in opening store: {e:?}");
            }
        };

        if wal_fd.metadata()?.len() > 0 {
            recover(
                sync_seqn,
                &ht_fd,
                &wal_fd,
                &page_pool,
                &store,
                &mut meta_map,
                seed,
            )?;
        }

        let occupied_buckets = meta_map.full_count();

        let wal_blob_builder = WalBlobBuilder::new()?;
        let capacity = meta_map.len();
        Ok(Self {
            shared: Arc::new(Shared {
                page_pool,
                store,
                seed,
                meta_map: Arc::new(RwLock::new(meta_map)),
                wal_blob_builder: Arc::new(Mutex::new(wal_blob_builder)),
                occupied_buckets: AtomicUsize::new(occupied_buckets),
                wal_fd,
                ht_fd,
                sync_tp: ThreadPool::with_name("bitbox-sync".into(), 2),
                capacity,
            }),
        })
    }

    /// Return space utilization counts.
    pub fn utilization(&self) -> HashTableUtilization {
        HashTableUtilization {
            capacity: self.shared.capacity,
            occupied: self.shared.occupied_buckets.load(Ordering::Relaxed),
        }
    }

    pub fn sync(&self) -> SyncController {
        SyncController::new(self.clone())
    }

    fn prepare_sync(
        &self,
        sync_seqn: u32,
        page_pool: &PagePool,
        changes: impl IntoIterator<Item = (PageId, DirtyPage)>,
        wal_blob_builder: &mut WalBlobBuilder,
    ) -> Result<
        (
            Vec<(u64, Arc<FatPage>)>,
            Vec<(PageId, Option<(Page, BucketIndex)>)>,
        ),
        BucketExhaustion,
    > {
        wal_blob_builder.reset(sync_seqn);

        let mut meta_map = self.shared.meta_map.write();

        let mut changed_meta_pages = HashSet::new();
        let mut ht_pages = Vec::new();
        let mut cache_updates = Vec::new();

        let mut occupied_buckets_delta = 0isize;

        // Allocate relevant buckets and update the meta-map.
        for (page_id, dirty_page) in changes {
            if dirty_page.diff.cleared() {
                occupied_buckets_delta -= 1;

                // UNWRAP/PANIC: any cleared pages should have already existed on disk.
                let bucket = match dirty_page.bucket {
                    BucketInfo::Known(bucket) => bucket,
                    BucketInfo::FreshOrDependent(maybe_bucket) => maybe_bucket.get().unwrap(),
                    _ => unreachable!(),
                }
                .0;
                meta_map.set_tombstone(bucket as usize);
                changed_meta_pages.insert(meta_map.page_index(bucket as usize));
                cache_updates.push((page_id.clone(), None));

                wal_blob_builder.write_clear(bucket);
            } else {
                // Allocate the bucket, if one is necessary.
                let (meta_map_changed, bucket) = match dirty_page.bucket {
                    BucketInfo::Known(bucket) => (false, bucket),
                    BucketInfo::FreshWithNoDependents => {
                        let bucket = allocate_bucket(&page_id, &mut meta_map, &self.shared.seed)
                            .ok_or(BucketExhaustion)?;
                        (true, bucket)
                    }
                    BucketInfo::FreshOrDependent(maybe_bucket) => match maybe_bucket.get() {
                        Some(bucket) => (false, bucket),
                        None => {
                            let bucket =
                                allocate_bucket(&page_id, &mut meta_map, &self.shared.seed)
                                    .ok_or(BucketExhaustion)?;
                            // Propagate changes to dependents.
                            maybe_bucket.set(bucket);
                            (true, bucket)
                        }
                    },
                };

                let BucketIndex(bucket) = bucket;

                // update meta map with new info
                let hash = hash_page_id(&page_id, &self.shared.seed);
                if meta_map_changed {
                    occupied_buckets_delta += 1;
                    meta_map.set_full(bucket as usize, hash);
                    changed_meta_pages.insert(meta_map.page_index(bucket as usize));
                }

                wal_blob_builder.write_update(
                    page_id.encode(),
                    &dirty_page.diff,
                    dirty_page
                        .diff
                        .pack_changed_nodes(dirty_page.page.page_data()),
                    dirty_page.page.elided_children(),
                    bucket,
                );

                let pn = self.shared.store.data_page_index(bucket);
                cache_updates.push((
                    page_id.clone(),
                    Some((dirty_page.page.clone(), BucketIndex(bucket))),
                ));
                ht_pages.push((pn, dirty_page.page.into_inner()));
            }
        }

        for changed_meta_page in changed_meta_pages {
            let mut buf = page_pool.alloc_fat_page();
            buf[..].copy_from_slice(meta_map.page_slice(changed_meta_page));
            let pn = self.shared.store.meta_bytes_index(changed_meta_page as u64);
            ht_pages.push((pn, Arc::new(buf)));
        }

        if cfg!(debug_assertions) {
            // Make sure that there are no duplicate pages.
            let orig_len = ht_pages.len();
            ht_pages.sort_unstable_by_key(|(pn, _)| *pn);
            ht_pages.dedup_by_key(|(pn, _)| *pn);
            assert_eq!(orig_len, ht_pages.len());
        }

        if occupied_buckets_delta < 0 {
            self.shared
                .occupied_buckets
                .fetch_sub(occupied_buckets_delta.abs() as usize, Ordering::Relaxed);
        } else if occupied_buckets_delta > 0 {
            self.shared
                .occupied_buckets
                .fetch_add(occupied_buckets_delta as usize, Ordering::Relaxed);
        }

        wal_blob_builder.finalize();

        Ok((ht_pages, cache_updates))
    }
}

pub struct SyncController {
    db: DB,
    /// The channel to send the result of the pre-meta sync errors. Option is to allow `take`.
    pre_meta_result_tx: Option<Sender<TaskResult<std::io::Result<()>>>>,
    /// he channel to receive the result of the pre-meta sync errors.
    pre_meta_result_rx: Receiver<TaskResult<std::io::Result<()>>>,
    /// The channel to send the result of the begin_sync task. Option is to allow `take`.
    begin_sync_result_tx: Option<Sender<TaskResult<Result<(), BucketExhaustion>>>>,
    /// The channel to receive the result of the the begin_sync task.
    begin_sync_result_rx: Receiver<TaskResult<Result<(), BucketExhaustion>>>,
    /// The pages along with their page numbers to write out to the HT file.
    ht_to_write: Arc<Mutex<Option<Vec<(u64, Arc<FatPage>)>>>>,
}

impl SyncController {
    fn new(db: DB) -> Self {
        let (pre_meta_result_tx, pre_meta_result_rx) = crossbeam_channel::bounded(1);
        let (begin_sync_result_tx, begin_sync_result_rx) = crossbeam_channel::bounded(1);
        Self {
            db,
            pre_meta_result_tx: Some(pre_meta_result_tx),
            pre_meta_result_rx,
            begin_sync_result_tx: Some(begin_sync_result_tx),
            begin_sync_result_rx,
            ht_to_write: Arc::new(Mutex::new(None)),
        }
    }

    /// Begins the sync process.
    ///
    /// Non-blocking.
    pub fn begin_sync(
        &mut self,
        sync_seqn: u32,
        page_cache: PageCache,
        updated_pages: impl IntoIterator<Item = (PageId, DirtyPage)> + Send + 'static,
    ) {
        let page_pool = self.db.shared.page_pool.clone();
        let bitbox = self.db.clone();
        let ht_to_write = self.ht_to_write.clone();
        let wal_blob_builder = self.db.shared.wal_blob_builder.clone();
        // UNWRAP: safe because begin_sync is called only once.
        let pre_meta_result_tx = self.pre_meta_result_tx.take().unwrap();
        let begin_sync_task = move || {
            let mut wal_blob_builder = wal_blob_builder.lock();

            // if fails The sync coordinator will poison the database and all further commits will
            // be rejected. Therefore, there is no need to perform cleanup.
            let (ht_pages, cache_updates) = bitbox.prepare_sync(
                sync_seqn,
                &page_pool,
                updated_pages,
                &mut *wal_blob_builder,
            )?;
            drop(wal_blob_builder);

            // Set the hash-table pages before spawning WAL writeout so they don't race with it.
            *ht_to_write.lock() = Some(ht_pages);
            Self::spawn_wal_writeout(pre_meta_result_tx, bitbox);

            // perform cache updates: insert changes and evict old pages.
            // evict and drop old pages outside of the critical path.
            page_cache.batch_update(cache_updates);
            page_cache.evict();
            Ok(())
        };
        // UNWRAP: safe because begin_sync is called only once.
        let begin_sync_result_tx = self.begin_sync_result_tx.take().unwrap();
        spawn_task(
            &self.db.shared.sync_tp,
            begin_sync_task,
            begin_sync_result_tx,
        );
    }

    fn spawn_wal_writeout(pre_meta_result_tx: Sender<TaskResult<std::io::Result<()>>>, bitbox: DB) {
        let bitbox = bitbox.clone();
        let tp = bitbox.shared.sync_tp.clone();
        let wal_writeout_task = move || {
            let wal_blob_builder = bitbox.shared.wal_blob_builder.lock();
            let wal_slice = wal_blob_builder.as_slice();
            writeout::write_wal(&bitbox.shared.wal_fd, wal_slice)
        };

        spawn_task(&tp, wal_writeout_task, pre_meta_result_tx);
    }

    /// Wait for the pre-meta operations to complete.
    ///
    /// This includes WAL file to be written out.
    ///
    /// Must be invoked by the sync thread. Blocking.
    pub fn wait_pre_meta(&self) -> anyhow::Result<()> {
        join_task(&self.begin_sync_result_rx)?;
        join_task(&self.pre_meta_result_rx)?;
        Ok(())
    }

    /// Write out the HT pages and truncate the WAL file.
    ///
    /// Has to be called after the manifest is updated. Must be invoked by the sync
    /// thread. Blocking.
    pub fn post_meta(&self, io_handle: IoHandle) -> std::io::Result<()> {
        let ht_pages = self.ht_to_write.lock().take().unwrap();
        // Writeout the HT pages and truncate the WAL file.
        //
        // Why don't we fsync the truncation of the WAL file? Because it should not be necessary.
        // To see why, recall that we write WAL at pre-meta stage, truncate it here and we only
        // read from it at recovery. Leaving the truncation unfsynced introduces some uncertainty
        // regarding the length of the file.
        //
        // However,
        //
        // 1. if we reach the commit after this one we are going to fsync the WAL with the
        //    new contents.
        // 2. if we crash before the next commit and if the WAL ended up not truncated, we just
        //    reapply the changes from the WAL which must be a noop.
        //
        // Therefore, we can safely avoid blocking on the truncation here.
        writeout::write_ht(io_handle, &self.db.shared.ht_fd, ht_pages)?;
        writeout::truncate_wal(&self.db.shared.wal_fd, false)?;
        Ok(())
    }
}

/// Perform recovery by applying the WAL to the HT file.
fn recover(
    sync_seqn: u32,
    ht_fd: &File,
    mut wal_fd: &File,
    page_pool: &PagePool,
    ht_offsets: &HTOffsets,
    meta_map: &mut MetaMap,
    seed: [u8; 16],
) -> anyhow::Result<()> {
    use crate::bitbox::wal::WalBlobReader;
    use std::io::{Seek, SeekFrom};

    wal_fd.seek(SeekFrom::Start(0))?;

    let mut wal_reader = WalBlobReader::new(page_pool, wal_fd)?;

    // This condition triggers either if:
    //   1. the WAL holds data for a sync that never concluded. Safe to discard.
    //   2. the WAL holds data for a sync that fully concluded. (somehow). Safe to discard.
    if wal_reader.sync_seqn() != sync_seqn {
        // fsync generously here since it's a one-time operation.
        writeout::truncate_wal(wal_fd, true)?;
        return Ok(());
    }

    // The indices of pages (in the metabits page space) that were changed and require updates.
    // Note those are not ht page numbers yet and still require additional conversion.
    let mut changed_meta_page_ixs = HashSet::new();

    while let Some(entry) = wal_reader.read_entry()? {
        match entry {
            wal::WalEntry::Clear { bucket } => {
                meta_map.set_tombstone(bucket as usize);

                // Note that the meta page requires update.
                changed_meta_page_ixs.insert(meta_map.page_index(bucket as usize));
            }
            wal::WalEntry::Update {
                page_id,
                page_diff,
                changed_nodes,
                elided_children,
                bucket,
            } => {
                let hash = hash_raw_page_id(page_id, &seed);
                let meta_map_changed = meta_map.hint_not_match(bucket as usize, hash);
                if meta_map_changed {
                    meta_map.set_full(bucket as usize, hash);
                    // Note that the meta page requires update.
                    changed_meta_page_ixs.insert(meta_map.page_index(bucket as usize));
                }

                // Apply the diff to the page in the ht file.
                //
                // The algorithm is:
                // - read the bucket page from the ht file.
                // - for each index of a bit in a diff that equals to 1, copy the changed node into
                //   the page.
                // - store the changed page.
                let pn = ht_offsets.data_page_index(bucket);

                let mut page = io::read_page(page_pool, ht_fd, pn)?;
                if page_diff.count() != changed_nodes.len() {
                    anyhow::bail!(
                        "mismatched number of changed nodes: {} != {}",
                        page_diff.count(),
                        changed_nodes.len()
                    );
                }
                page_diff.unpack_changed_nodes(&changed_nodes, &mut page);

                // Label the page.
                page[PAGE_SIZE - 32..].copy_from_slice(&page_id);
                // Write elided children bitfield.
                page[PAGE_SIZE - 32 - 8..PAGE_SIZE - 32]
                    .copy_from_slice(&elided_children.to_bytes());

                ht_fd.write_all_at(&page, pn * PAGE_SIZE as u64)?;
            }
        }
    }

    // Now that we have applied all the updates, we know precisely which meta pages have been
    // updated.
    //
    // We now write those pages out to the HT file.
    for changed_meta_page_ix in changed_meta_page_ixs {
        unsafe {
            let page = page_pool.alloc();
            // SAFETY: page is a fresh allocation from page pool and it's not aliased.
            let page_data = page.as_mut_slice();
            page_data[..].copy_from_slice(meta_map.page_slice(changed_meta_page_ix));

            let pn = ht_offsets.meta_bytes_index(changed_meta_page_ix as u64);
            ht_fd.write_all_at(page_data, pn * PAGE_SIZE as u64)?;

            page_pool.dealloc(page);
        }
    }

    // Finally, we collapse the WAL file and fsync.
    writeout::truncate_wal(wal_fd, true)?;

    Ok(())
}

/// A utility for loading pages from bitbox.
pub struct PageLoader {
    shared: Arc<Shared>,
    meta_map: ArcRwLockReadGuard<parking_lot::RawRwLock, MetaMap>,
}

impl PageLoader {
    /// Create a new page loader.
    pub fn new(db: &DB) -> Self {
        PageLoader {
            shared: db.shared.clone(),
            meta_map: RwLock::read_arc(&db.shared.meta_map),
        }
    }

    /// Create a new page load.
    pub fn start_load(&self, page_id: PageId) -> PageLoad {
        PageLoad {
            probe_sequence: ProbeSequence::new(&page_id, &self.meta_map, &self.shared.seed),
            page_id,
            state: PageLoadState::Pending,
        }
    }

    /// Advance the state of the given page load, blocking the current thread.
    ///
    /// Panics if the page load needs a completion or if the I/O pool is down.
    ///
    /// This returns `true` if the page request has been submitted and a completion will be
    /// coming. `false` means that the page is guaranteed to be fresh.
    ///
    /// An `IoCommand` of kind `Read` will be submitted along the I/O handle with the provided
    /// user-data.
    ///
    /// Note that the page loaded by the I/O pool may be a misprobe. You must use
    /// [`PageLoad::try_complete`] to verify whether the hash-table probe has completed or must be
    /// tried again.
    pub fn probe(&self, load: &mut PageLoad, io_handle: &IoHandle, user_data: u64) -> bool {
        let bucket = loop {
            match load.probe_sequence.next(&self.meta_map) {
                ProbeResult::Tombstone(_) => continue,
                ProbeResult::Empty(_) => return false,
                ProbeResult::PossibleHit(bucket) => break BucketIndex(bucket),
            }
        };

        let data_page_index = self.shared.store.data_page_index(bucket.0);

        let page = self.shared.page_pool.alloc_fat_page();
        let command = IoCommand {
            kind: IoKind::Read(self.shared.ht_fd.as_raw_fd(), data_page_index, page),
            user_data,
        };

        // UNWRAP: I/O pool is not expected to hangup.
        io_handle.send(command).unwrap();
        load.state = PageLoadState::Submitted;
        true
    }
}

pub struct PageLoad {
    page_id: PageId,
    probe_sequence: ProbeSequence,
    state: PageLoadState,
}

impl PageLoad {
    pub fn needs_completion(&self) -> bool {
        self.state == PageLoadState::Submitted
    }

    pub fn page_id(&self) -> &PageId {
        &self.page_id
    }

    /// Try to complete the page load.
    ///
    /// If this returns `Some`, then the load has completed and this struct may be discarded.
    /// Otherwise, you must continue with [`PageLoader::probe`].
    pub fn try_complete(&mut self, page: FatPage) -> Option<(FatPage, BucketIndex)> {
        assert!(self.needs_completion());
        if page[PAGE_SIZE - 32..] == self.page_id.encode() {
            Some((page, BucketIndex(self.probe_sequence.bucket())))
        } else {
            self.state = PageLoadState::Pending;
            None
        }
    }
}

/// Describes the utilization of buckets in the hash-table at a point in time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HashTableUtilization {
    /// The maximum number of buckets in the hash-table.
    pub capacity: usize,
    /// The number of occupied buckets in the hash-table.
    pub occupied: usize,
}

impl HashTableUtilization {
    /// Compute the occupancy rate of the hash-table. This ranges between 0.0 and 1.0.
    ///
    /// Performance likely degrades beyond 0.9 and commits may fail as the occupancy approaches 1.0.
    pub fn occupancy_rate(&self) -> f64 {
        self.occupied as f64 / self.capacity as f64
    }
}

#[derive(Clone, Copy, PartialEq)]
enum PageLoadState {
    Pending,
    Submitted,
}

/// Allocates a bucket in the meta map for the given page ID.
///
/// Returns the index of the allocated bucket.
///
/// This performs a limited number of attempts. In case too many attempts have been made and the
/// allocator gave up returns `None`.
fn allocate_bucket(
    page_id: &PageId,
    meta_map: &mut MetaMap,
    seed: &[u8; 16],
) -> Option<BucketIndex> {
    let mut probe_seq = ProbeSequence::new(page_id, &meta_map, seed);

    let mut i = 0;
    loop {
        i += 1;
        if i >= 10000 {
            // Give up.
            return None;
        }
        match probe_seq.next(&meta_map) {
            ProbeResult::PossibleHit(_) => continue,
            ProbeResult::Tombstone(bucket) | ProbeResult::Empty(bucket) => {
                meta_map.set_full(bucket as usize, probe_seq.hash);
                return Some(BucketIndex(bucket));
            }
        }
    }
}

fn hash_page_id(page_id: &PageId, seed: &[u8; 16]) -> u64 {
    hash_raw_page_id(page_id.encode(), seed)
}

fn hash_raw_page_id(page_id: [u8; 32], seed: &[u8; 16]) -> u64 {
    // UNWRAP: 8 byte slice can always be transformed into 8 byte array.
    let seed_u64 = u64::from_be_bytes(seed[..8].try_into().unwrap());
    twox_hash::xxhash3_64::Hasher::oneshot_with_seed(seed_u64, &page_id)
}

#[derive(Clone, Copy)]
struct ProbeSequence {
    hash: u64,
    bucket: u64,
    step: u64,
}

enum ProbeResult {
    PossibleHit(u64),
    Empty(u64),
    Tombstone(u64),
}

impl ProbeSequence {
    fn new(page_id: &PageId, meta_map: &MetaMap, seed: &[u8; 16]) -> Self {
        let hash = hash_page_id(page_id, seed);
        Self {
            hash,
            bucket: hash % meta_map.len() as u64,
            step: 0,
        }
    }

    // probe until there is a possible hit or an empty bucket is found
    fn next(&mut self, meta_map: &MetaMap) -> ProbeResult {
        loop {
            // Triangular probing
            self.bucket += self.step;
            self.step += 1;
            self.bucket %= meta_map.len() as u64;

            if meta_map.hint_empty(self.bucket as usize) {
                return ProbeResult::Empty(self.bucket);
            }

            if meta_map.hint_tombstone(self.bucket as usize) {
                return ProbeResult::Tombstone(self.bucket);
            }

            if meta_map.hint_not_match(self.bucket as usize, self.hash) {
                continue;
            }

            return ProbeResult::PossibleHit(self.bucket);
        }
    }

    fn bucket(&self) -> u64 {
        self.bucket
    }
}
