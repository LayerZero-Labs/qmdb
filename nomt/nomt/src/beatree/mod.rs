use allocator::{Store, StoreReader, FREELIST_EMPTY};
use anyhow::{Context, Result};
use branch::BRANCH_NODE_SIZE;
use crossbeam_channel::{Receiver, Sender};
use imbl::OrdMap;

use leaf::node::MAX_LEAF_VALUE_SIZE;
use nomt_core::trie::ValueHash;
use ops::overflow;
use parking_lot::{ArcMutexGuard, Condvar, Mutex, RwLock};
use std::{fs::File, mem, path::Path, sync::Arc};
use threadpool::ThreadPool;

use crate::{
    io::{fsyncer::Fsyncer, FatPage, IoHandle, IoPool, PagePool},
    task::{join_task, spawn_task, TaskResult},
};

pub mod iterator;

mod allocator;
mod branch;
mod index;
mod leaf;
mod leaf_cache;
mod ops;

mod writeout;

pub use allocator::PageNumber;
use index::Index;
pub use iterator::BeatreeIterator;
use leaf_cache::LeafCache;

#[cfg(feature = "benchmarks")]
pub mod benches;

#[cfg(feature = "fuzz")]
pub use ops::bit_ops::*;

pub type Key = [u8; 32];

#[derive(Clone)]
pub struct Tree {
    read_transaction_counter: ReadTransactionCounter,
    shared: Arc<RwLock<Shared>>,
    sync: Arc<Mutex<Sync>>,
}

struct Shared {
    page_pool: PagePool,
    io_handle: IoHandle,
    bbn_index: index::Index,
    leaf_store: Store,
    bbn_store: Store,
    leaf_store_rd: StoreReader,
    /// Primary staging collects changes that are committed but not synced yet. Upon sync, changes
    /// from here are moved to secondary staging.
    primary_staging: OrdMap<Key, ValueChange>,
    /// Secondary staging collects committed changes that are currently being synced. This is None
    /// if there is no sync in progress.
    secondary_staging: Option<OrdMap<Key, ValueChange>>,
    leaf_cache: leaf_cache::LeafCache,
}

struct Sync {
    tp: ThreadPool,
    commit_concurrency: usize,
    bbn_fsync: Arc<Fsyncer>,
    ln_fsync: Arc<Fsyncer>,
}

impl Shared {
    fn take_staged_changeset(&mut self) -> OrdMap<Key, ValueChange> {
        assert!(self.secondary_staging.is_none());
        let staged = mem::take(&mut self.primary_staging);
        self.secondary_staging = Some(staged.clone());
        staged
    }
}

impl Tree {
    pub fn open(
        page_pool: PagePool,
        io_pool: &IoPool,
        ln_freelist_pn: u32,
        bbn_freelist_pn: u32,
        ln_bump: u32,
        bbn_bump: u32,
        bbn_file: Arc<File>,
        ln_file: Arc<File>,
        commit_concurrency: usize,
        leaf_cache_size: usize,
    ) -> Result<Tree> {
        let ln_freelist_pn = Some(ln_freelist_pn)
            .map(PageNumber)
            .filter(|&x| x != FREELIST_EMPTY);
        let bbn_freelist_pn = Some(bbn_freelist_pn)
            .map(PageNumber)
            .filter(|&x| x != FREELIST_EMPTY);

        let ln_bump = PageNumber(ln_bump);
        let bbn_bump = PageNumber(bbn_bump);

        let leaf_store = Store::open(&page_pool, ln_file.clone(), ln_bump, ln_freelist_pn)?;

        let bbn_store = Store::open(&page_pool, bbn_file.clone(), bbn_bump, bbn_freelist_pn)?;

        let bbn_freelist_tracked = bbn_store.all_tracked_freelist_pages();
        let index = ops::reconstruct(
            bbn_file.clone(),
            &page_pool,
            &bbn_freelist_tracked,
            bbn_bump,
        )
        .context("failed to reconstruct btree from bbn store file")?;
        let shared = Shared {
            io_handle: io_pool.make_handle(),
            page_pool: io_pool.page_pool().clone(),
            bbn_index: index,
            leaf_store_rd: StoreReader::new(leaf_store.clone(), io_pool.page_pool().clone()),
            leaf_store,
            bbn_store,
            primary_staging: OrdMap::new(),
            secondary_staging: None,
            leaf_cache: leaf_cache::LeafCache::new(32, leaf_cache_size),
        };

        let sync = Sync {
            // +1 for the begin_sync task.
            tp: ThreadPool::with_name("beatree-sync".into(), commit_concurrency + 1),
            commit_concurrency,
            bbn_fsync: Arc::new(Fsyncer::new("bbn", bbn_file)),
            ln_fsync: Arc::new(Fsyncer::new("ln", ln_file)),
        };

        Ok(Tree {
            shared: Arc::new(RwLock::new(shared)),
            sync: Arc::new(Mutex::new(sync)),
            read_transaction_counter: ReadTransactionCounter::new(),
        })
    }

    /// Lookup a key in the btree. This blocks the current thread.
    pub fn lookup(&self, key: Key) -> Option<Vec<u8>> {
        let shared = self.shared.read();

        // First look up in the primary staging which contains the most recent changes.
        if let Some(val) = shared.primary_staging.get(&key) {
            return val.as_option().map(|v| v.to_vec());
        }

        // Then check the secondary staging which is a bit older, but fresher still than the btree.
        if let Some(val) = shared.secondary_staging.as_ref().and_then(|x| x.get(&key)) {
            return val.as_option().map(|v| v.to_vec());
        }

        // Finally, look up in the btree.
        ops::lookup_blocking(
            key,
            &shared.bbn_index,
            &shared.leaf_cache,
            &shared.leaf_store_rd,
        )
        .unwrap()
    }

    /// Returns a controller for the sync process. This is blocked by other `sync`s running as well
    /// as the existence of any read transactions.
    pub fn sync(&self) -> SyncController {
        // Take the sync lock.
        //
        // That will exclude any other syncs from happening. This is a long running operation.
        let sync = self.sync.lock_arc();
        let (begin_sync_result_tx, begin_sync_result_rx) = crossbeam_channel::bounded(1);
        SyncController {
            inner: Arc::new(SharedSyncController {
                sync,
                shared: self.shared.clone(),
                read_transaction_counter: self.read_transaction_counter.clone(),
                sync_data: Mutex::new(None),
                bbn_index: Mutex::new(None),
                pre_swap_rx: Mutex::new(None),
            }),
            begin_sync_result_tx: Some(begin_sync_result_tx),
            begin_sync_result_rx,
        }
    }

    /// Initiate a new read transaction, as-of the current state of the last commit.
    /// This blocks new sync operations from starting until it is dropped.
    pub fn read_transaction(&self) -> ReadTransaction {
        // Increment the count. This will block any sync from starting between now and the point
        // where the read transaction is dropped.
        self.read_transaction_counter.add_one();
        let shared = self.shared.read();
        let inner = Arc::new(ReadTransactionInner {
            bbn_index: shared.bbn_index.clone(),
            primary_staging: shared.primary_staging.clone(),
            secondary_staging: shared.secondary_staging.clone(),
            leaf_store: StoreReader::new(shared.leaf_store.clone(), shared.page_pool.clone()),
            leaf_cache: shared.leaf_cache.clone(),
            read_counter: self.read_transaction_counter.clone(),
        });

        ReadTransaction { inner }
    }

    /// Commit a set of changes to the btree.
    ///
    /// The changeset is a list of key value pairs to be added or removed from the btree.
    /// The changeset is applied atomically. If the changeset is empty, the btree is not modified.
    // There might be some temptation to unify this with prepare_sync, but this should not be done
    // because in the future sync and commit will be called on different threads at different times.
    fn commit(
        shared: &Arc<RwLock<Shared>>,
        changeset: impl IntoIterator<Item = (Key, ValueChange)>,
    ) {
        let mut inner = shared.write();
        let staging = &mut inner.primary_staging;
        for (key, value) in changeset {
            staging.insert(key, value);
        }
    }

    /// Dump all changes performed by commits to the underlying storage medium.
    /// The returned received indicates that all eviction has completed and it is safe to swap the
    /// index. This receiver must be blocked on before finishing sync.
    ///
    /// Blocks until all outstanding read transactions have concluded, and either blocks or panics
    /// if another sync is inflight.
    fn prepare_sync(
        sync: &Sync,
        shared: &Arc<RwLock<Shared>>,
        read_transaction_counter: &ReadTransactionCounter,
    ) -> std::io::Result<(SyncData, Index, Receiver<TaskResult<()>>)> {
        // Take the shared lock. Briefly.
        let staged_changeset;
        let bbn_index;
        let page_pool;
        let leaf_cache;
        let leaf_store;
        let bbn_store;
        let io_handle;
        {
            // Wait for all outstanding read transactions to conclude. This ensures they won't
            // be invalidated by any destructive changes.
            //
            // The reason we do this is somewhat subtle and depends on internal details of the
            // database. Some elaboration follows.
            //
            // Any outstanding read transaction may have been started before the last sync
            // concluded. That means that it may have a copy of the index referring to leaf pages
            // which have been logically deleted but which are still present on disk or in the
            // cache. This is due to our use of shadow paging and CoW techniques which do not
            // perform destructive changes of on-disk pages until the sync after the one which
            // logically changed them.
            //
            // However, this next sync may overwrite those pages. Therefore we must force those
            // read transactions to conclude.
            //
            // note: there are other possible implementations of this protocol which are more
            // permissive, i.e. only forcing read transactions to end if they were created before
            // the most recent call to `finish_sync`.
            //
            // As a diagram, where 's' marks sync start points and 'f' marks sync finish points.
            //
            // ```
            //        we want to exclude any transactions started before this point
            //       |
            // [s____f____s]
            // time->     |
            //            |
            //            but we exclude all transactions before this point as a simplification.
            // ```
            read_transaction_counter.block_until_zero();

            // It is safe for a read transaction to be created here, since it follows the conclusion
            // of the most recent sync and therefore references no logically free pages.

            let mut shared = shared.write();
            staged_changeset = shared.take_staged_changeset();
            bbn_index = shared.bbn_index.clone();
            page_pool = shared.page_pool.clone();
            leaf_cache = shared.leaf_cache.clone();
            leaf_store = shared.leaf_store.clone();
            bbn_store = shared.bbn_store.clone();
            io_handle = shared.io_handle.clone();
        }

        {
            // Update will modify the index in a CoW manner.
            //
            // Nodes that need to be modified are not modified in place but they are
            // removed from the copy of the index,
            // and new ones (implying the one created as modification of existing nodes) are
            // allocated.
            //
            // Thus during the update:
            // + The index will just be modified, being a copy of the one used in parallel during lookups
            // + Allocation and releases of the leaf_store_wr will be executed normally,
            //   as everything will be in a pending state until commit
            // + All branch page releases will be performed at the end of the sync, when the old
            //   revision of the index is dropped.
            //   This makes it possible to keep the previous state of the tree (before this sync)
            //   available and reachable from the old index
            // + All necessary page writes will be issued to the store and their completion waited
            //   upon. However, these changes are not reflected until `finish_sync`.
            ops::update(
                staged_changeset.clone(),
                bbn_index,
                leaf_cache,
                leaf_store,
                bbn_store,
                page_pool,
                io_handle,
                sync.tp.clone(),
                sync.commit_concurrency,
            )
        }
    }

    fn finish_sync(shared: &Arc<RwLock<Shared>>, bbn_index: Index) {
        // Take the shared lock again to complete the update to the new shared state
        let mut shared = shared.write();
        shared.secondary_staging = None;
        shared.bbn_index = bbn_index;
    }
}

/// A change in the value associated with a key.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueChange {
    /// The key-value pair is deleted.
    Delete,
    /// A new value small enough to fit in a leaf is inserted.
    Insert(Vec<u8>),
    /// A new value which requires an overflow page is inserted.
    InsertOverflow(Vec<u8>, ValueHash),
}

impl ValueChange {
    /// Create a [`ValueChange`] from an option, determining whether to use the normal or overflow
    /// variant based on size.
    pub fn from_option<T: crate::ValueHasher>(maybe_value: Option<Vec<u8>>) -> Self {
        match maybe_value {
            None => ValueChange::Delete,
            Some(v) => Self::insert::<T>(v),
        }
    }

    /// Create an insertion, determining whether to use the normal or overflow variant based on size.
    pub fn insert<T: crate::ValueHasher>(v: Vec<u8>) -> Self {
        if v.len() > MAX_LEAF_VALUE_SIZE {
            let value_hash = T::hash_value(&v);
            ValueChange::InsertOverflow(v, value_hash)
        } else {
            ValueChange::Insert(v)
        }
    }

    /// Get the value bytes, optionally.
    pub fn as_option(&self) -> Option<&[u8]> {
        match self {
            ValueChange::Delete => None,
            ValueChange::Insert(ref v) | ValueChange::InsertOverflow(ref v, _) => Some(&v[..]),
        }
    }
}

/// Data generated during update
pub struct SyncData {
    pub ln_freelist_pn: u32,
    pub ln_bump: u32,
    pub bbn_freelist_pn: u32,
    pub bbn_bump: u32,
}

/// Creates the required files for the beatree.
pub fn create(db_dir: impl AsRef<Path>) -> anyhow::Result<()> {
    // Create the files.
    //
    // Size them to have an empty page at the beginning, this is reserved for the nil page.
    let ln_fd = File::create(db_dir.as_ref().join("ln"))?;
    let bbn_fd = File::create(db_dir.as_ref().join("bbn"))?;
    ln_fd.set_len(BRANCH_NODE_SIZE as u64)?;
    bbn_fd.set_len(BRANCH_NODE_SIZE as u64)?;

    // Sync files and the directory. I am not sure if syncing files is necessary, but it
    // is necessary to make sure that the directory is synced.
    ln_fd.sync_all()?;
    bbn_fd.sync_all()?;
    Ok(())
}

/// A handle that controls the sync process.
///
/// The order of the calls should always be:
///
/// 1. [`Self::begin_sync`] - Initiates an asynchronous sync operation.
/// 2. [`Self::wait_pre_meta`] - Blocks until the sync process completes and returns metadata.
///    The manifest can be updated after this call returns successfully.
/// 3. [`Self::post_meta`] - Finalizes the sync process by updating internal state.
///
/// # Thread Safety
///
/// This controller is designed to be used from a single thread. While the underlying operations
/// are thread-safe, the controller itself maintains state that requires calls to be made in sequence.
///
/// # Error Handling
///
/// If [`Self::wait_pre_meta`] returns an error, the sync process has failed and the controller
/// should be discarded.
pub struct SyncController {
    // The channel to send the result of the begin sync task. Option is to allow `take`.
    begin_sync_result_tx: Option<Sender<TaskResult<std::io::Result<()>>>>,
    // The channel to receive the result of the begin sync task.
    begin_sync_result_rx: Receiver<TaskResult<std::io::Result<()>>>,
    inner: Arc<SharedSyncController>,
}

struct SharedSyncController {
    sync: ArcMutexGuard<parking_lot::RawMutex, Sync>,
    shared: Arc<RwLock<Shared>>,
    read_transaction_counter: ReadTransactionCounter,
    sync_data: Mutex<Option<SyncData>>,
    bbn_index: Mutex<Option<Index>>,
    pre_swap_rx: Mutex<Option<Receiver<TaskResult<()>>>>,
}

impl SyncController {
    /// Begins the sync process.
    ///
    /// Accepts a list of changes to be committed to the btree.
    ///
    /// Non-blocking.
    pub fn begin_sync(
        &mut self,
        changeset: impl IntoIterator<Item = (Key, ValueChange)> + Send + 'static,
    ) {
        let inner = self.inner.clone();
        let begin_sync_task = move || {
            Tree::commit(&inner.shared, changeset);

            let (out_meta, out_bbn_index, out_pre_swap_rx) =
                Tree::prepare_sync(&inner.sync, &inner.shared, &inner.read_transaction_counter)?;

            let mut sync_data = inner.sync_data.lock();
            *sync_data = Some(out_meta);
            drop(sync_data);

            let mut bbn_index = inner.bbn_index.lock();
            *bbn_index = Some(out_bbn_index);
            drop(bbn_index);

            let mut pre_swap_rx = inner.pre_swap_rx.lock();
            *pre_swap_rx = Some(out_pre_swap_rx);
            drop(pre_swap_rx);

            inner.sync.bbn_fsync.fsync();
            inner.sync.ln_fsync.fsync();
            Ok(())
        };

        // UNWRAP: safe because begin_sync is called only once.
        let begin_sync_result_tx = self.begin_sync_result_tx.take().unwrap();
        spawn_task(&self.inner.sync.tp, begin_sync_task, begin_sync_result_tx);
    }

    /// Waits for the writes to the tree to be synced to disk which allows the caller to proceed
    /// with updating the manifest.
    ///
    /// This must be called after [`Self::begin_sync`].
    pub fn wait_pre_meta(&mut self) -> std::io::Result<SyncData> {
        join_task(&self.begin_sync_result_rx)?;
        self.inner.sync.bbn_fsync.wait()?;
        self.inner.sync.ln_fsync.wait()?;

        // UNWRAP: fsync of bbn and ln above ensures that sync_data is Some.
        let sync_data = self.inner.sync_data.lock().take().unwrap();
        Ok(sync_data)
    }

    /// Finishes sync.
    ///
    /// Has to be called after the manifest is updated. Must be invoked by the sync
    /// thread. Blocking.
    pub fn post_meta(&mut self) {
        let pre_swap_rx = self.inner.pre_swap_rx.lock().take().unwrap();
        join_task(&pre_swap_rx);
        let bbn_index = self.inner.bbn_index.lock().take().unwrap();
        Tree::finish_sync(&self.inner.shared, bbn_index);
    }
}

/// A read-transaction freezes a read-only state of the beatree as-of the last commit and enables
/// lookups while it is alive.
///
/// The existence of a read transaction blocks new `Sync`s from starting, but may start when
/// a sync is already ongoing and does not block an ongoing sync from completing.
///
/// This is because sync may perform destructive changes to leaves, invalidating the read
/// transaction.
///
/// Further commits may be performed while the read transaction is live, but they won't be reflected
/// within the transaction.
///
/// This is cheap to clone.
#[derive(Clone)]
pub struct ReadTransaction {
    inner: Arc<ReadTransactionInner>,
}

struct ReadTransactionInner {
    bbn_index: Index,
    primary_staging: OrdMap<Key, ValueChange>,
    secondary_staging: Option<OrdMap<Key, ValueChange>>,
    leaf_store: StoreReader,
    leaf_cache: LeafCache,
    read_counter: ReadTransactionCounter,
}

impl ReadTransaction {
    /// Create a new iterator with the given half-open start and end range.
    pub fn iterator(&self, start: Key, end: Option<Key>) -> BeatreeIterator {
        BeatreeIterator::new(
            self.inner.primary_staging.clone(),
            self.inner.secondary_staging.clone(),
            self.inner.bbn_index.clone(),
            start,
            end,
        )
    }

    /// Initiate an asynchronous leaf page fetch. This may return immediately if the leaf is cached.
    ///
    /// This is an error-prone, low-level API you should not use unless you know what you are doing.
    ///
    /// If `Ok` is returned, then no I/O command has been submitted along the handle.
    /// If `Err` is returned, then an I/O command has been submitted along the handle, and the
    /// user_data is as specified.
    pub fn load_leaf_async(
        &self,
        page_number: PageNumber,
        io_handle: &IoHandle,
        user_data: u64,
    ) -> Result<LeafNodeRef, AsyncLeafLoad> {
        if let Some(leaf) = self.inner.leaf_cache.get(page_number) {
            Ok(LeafNodeRef { inner: leaf })
        } else {
            let command = self.inner.leaf_store.io_command(page_number, user_data);

            let _ = io_handle.send(command);
            Err(AsyncLeafLoad {
                page_number,
                read_tx: self.inner.clone(),
            })
        }
    }

    /// Initiate an asynchronous lookup of a value. This may return immediately if the leaf is cached.
    ///
    /// This is an error-prone, low-level API you should not use unless you know what you are doing.
    ///
    /// If `Ok` is returned, then no I/O command has been submitted along the handle.
    /// If `Err` is returned, then an I/O command has been submitted along the handle, and the
    /// user_data is as specified.
    pub fn lookup_async(
        &self,
        key: Key,
        io_handle: &IoHandle,
        user_data: u64,
    ) -> Result<Option<Vec<u8>>, AsyncLookup> {
        // First look up in the primary staging which contains the most recent changes.
        if let Some(val) = self.inner.primary_staging.get(&key) {
            return Ok(val.as_option().map(|v| v.to_vec()));
        }

        // Then check the secondary staging which is a bit older, but fresher still than the btree.
        if let Some(val) = self
            .inner
            .secondary_staging
            .as_ref()
            .and_then(|x| x.get(&key))
        {
            return Ok(val.as_option().map(|v| v.to_vec()));
        }

        let leaf_pn = match ops::partial_lookup(key, &self.inner.bbn_index) {
            None => return Ok(None),
            Some(pn) => pn,
        };

        match self
            .load_leaf_async(leaf_pn, io_handle, user_data)
            .map(|leaf| ops::finish_lookup_async(key, &leaf.inner, &self.inner.leaf_store))
        {
            Ok(Ok(val)) => Ok(val),
            Ok(Err(mut overflow)) => {
                // UNWRAP: first overflow request always succeeds.
                let meta = overflow.submit(io_handle, user_data).unwrap();
                Err(AsyncLookup {
                    key,
                    state: AsyncLookupState::Overflow(overflow, Some(meta)),
                })
            }
            Err(pending) => Err(AsyncLookup {
                key,
                state: AsyncLookupState::Initial(pending),
            }),
        }
    }
}

impl Drop for ReadTransactionInner {
    fn drop(&mut self) {
        self.read_counter.release_one()
    }
}

/// A type representing a pending leaf load. This keeps the associated read transaction alive
/// throughout its lifetime.
pub struct AsyncLeafLoad {
    read_tx: Arc<ReadTransactionInner>,
    page_number: PageNumber,
}

impl AsyncLeafLoad {
    /// Finish the leaf load.
    ///
    /// Calling this with the wrong page will likely lead to panics or bugs in the future.
    pub fn finish(self, page: FatPage) -> LeafNodeRef {
        LeafNodeRef {
            inner: self.finish_inner(page),
        }
    }

    fn finish_inner(&self, page: FatPage) -> Arc<leaf::node::LeafNode> {
        let leaf_node = Arc::new(leaf::node::LeafNode { inner: page });

        self.read_tx
            .leaf_cache
            .insert(self.page_number, leaf_node.clone());

        leaf_node
    }

    /// Get the page number associated with this leaf load.
    pub fn page_number(&self) -> PageNumber {
        self.page_number
    }
}

/// A type representing a pending lookup. This keeps the associated read transaction alive
/// throughout its lifetime.
pub struct AsyncLookup {
    key: Key,
    state: AsyncLookupState,
}

enum AsyncLookupState {
    Initial(AsyncLeafLoad),
    Overflow(overflow::AsyncReader, Option<usize>),
    Done,
}

impl AsyncLookup {
    /// Attempt to submit a continuation request along the handle.
    ///
    /// This should not be called unless `try_finish` has failed at least once.
    pub fn submit(&mut self, io_handle: &IoHandle, user_data: u64) -> Option<OverflowPageInfo> {
        match self.state {
            AsyncLookupState::Initial(_) => None,
            AsyncLookupState::Overflow(ref mut overflow, _) => {
                overflow.submit(io_handle, user_data).map(OverflowPageInfo)
            }
            AsyncLookupState::Done => None,
        }
    }

    /// Try to finish the lookup.
    /// Calling this with the wrong page will lead to panics or silent errors.
    ///
    /// If calling for the first time, provide no load info. Only provide load info that has been
    /// returned from `submit` from this lookup. Otherwise, this may panic or silently cause errors.
    ///
    /// This returns `None` if not finished, `Some` otherwise. After returning `Some` once, this
    /// will return `None` forever.
    ///
    /// If more lookups are required to finish, this will return an `Err`.
    pub fn try_finish(
        &mut self,
        page: FatPage,
        meta: Option<OverflowPageInfo>,
    ) -> Option<Option<Vec<u8>>> {
        match self.state {
            AsyncLookupState::Done => None,
            AsyncLookupState::Initial(ref inner) => {
                let leaf = inner.finish_inner(page);
                match ops::finish_lookup_async(self.key, &leaf, &inner.read_tx.leaf_store) {
                    Ok(val) => {
                        self.state = AsyncLookupState::Done;
                        Some(val)
                    }
                    Err(overflow) => {
                        self.state = AsyncLookupState::Overflow(overflow, None);
                        None
                    }
                }
            }
            AsyncLookupState::Overflow(ref mut overflow, ref mut initial_meta) => {
                // UNWRAP: part of function contract.
                let index = meta
                    .map(|m| m.0)
                    .unwrap_or_else(|| initial_meta.take().unwrap());
                let res = overflow.complete(index, page).map(Some);

                if res.is_some() {
                    self.state = AsyncLookupState::Done;
                }

                res
            }
        }
    }
}

/// Opaque info related to an overflow page load.
///
/// Used by [`AsyncLookup`] to handle the page appropriately.
pub struct OverflowPageInfo(usize);

/// An opaque reference to a leaf node. These cannot be manipulated directly and instead must be
/// passed to a struct which can make use of them, such as the [`BeatreeIterator`].
///
/// This is cheap to clone.
#[derive(Clone)]
pub struct LeafNodeRef {
    inner: Arc<leaf::node::LeafNode>,
}

/// This keeps track of the count of outstanding read transactions, and enables blocking until
/// the count is zero.
#[derive(Clone)]
struct ReadTransactionCounter {
    inner: Arc<ReadTransactionCounterInner>,
}

impl ReadTransactionCounter {
    fn new() -> Self {
        ReadTransactionCounter {
            inner: Arc::new(ReadTransactionCounterInner {
                read_transactions: Mutex::new(0),
                cvar: Condvar::new(),
            }),
        }
    }

    fn release_one(&self) {
        {
            let mut guard = self.inner.read_transactions.lock();
            // UNWRAP: this is only called when a read transaction is dropped, which always pairs with
            // the `add_one` call when the read transaction was created.
            *guard = guard.checked_sub(1).unwrap();
        }
        self.inner.cvar.notify_one();
    }

    fn add_one(&self) {
        *self.inner.read_transactions.lock() += 1;
    }

    // Block until all outstanding read transactions have been released.
    fn block_until_zero(&self) {
        let mut guard = self.inner.read_transactions.lock();
        self.inner.cvar.wait_while(&mut guard, |count| *count > 0);
    }
}

struct ReadTransactionCounterInner {
    read_transactions: Mutex<usize>,
    cvar: Condvar,
}
