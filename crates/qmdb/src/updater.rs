//! Update and state management module for QMDB.
//!
//! This module implements the core state update functionality for the database,
//! handling all modifications to entries while maintaining consistency and
//! optimizing performance. It manages:
//!
//! # Core Responsibilities
//! - Processing entry updates (create, write, delete)
//! - Managing entry versions and serial numbers
//! - Coordinating with the task hub for ordered execution
//! - Handling compaction of old entries
//! - Maintaining cache consistency
//! - Optimizing I/O operations
//!
//! # Architecture
//! The update system consists of several key components:
//!
//! ## Updater
//! The main coordinator that:
//! - Processes tasks in order
//! - Manages entry lifecycle
//! - Handles buffer management
//! - Coordinates with other components
//!
//! ## Update Buffer
//! In-memory buffer that:
//! - Stores recent updates
//! - Provides fast access to hot data
//! - Manages write batching
//! - Optimizes I/O patterns
//!
//! ## Task Processing
//! Implements ordered execution:
//! - Maintains task ordering
//! - Handles out-of-order arrivals
//! - Ensures consistency
//! - Manages dependencies
//!
//! # Process Flow
//! 1. Task Reception:
//!    - Receive task IDs
//!    - Order tasks appropriately
//!    - Handle out-of-order arrivals
//!
//! 2. Entry Processing:
//!    - Read existing entries
//!    - Apply modifications
//!    - Update indices
//!    - Manage versions
//!
//! 3. State Management:
//!    - Update cache entries
//!    - Write to buffer
//!    - Handle compaction
//!    - Maintain consistency
//!
//! # Performance Considerations
//! - Uses efficient buffer management
//! - Implements caching strategies
//! - Optimizes I/O patterns
//! - Minimizes lock contention
//!
//! # Configuration
//! Key parameters include:
//! - Buffer sizes
//! - Compaction thresholds
//! - Utilization ratios
//! - Cache settings
//!
//! # Example Usage
//! ```ignore
//! use qmdb::updater::Updater;
//! use qmdb::tasks::TaskHub;
//! use qmdb::entryfile::{EntryBufferWriter, EntryFile, EntryCache, entrybuffer};
//! use qmdb::indexer::Indexer;
//! use qmdb::utils::ringchannel::Consumer;
//! use qmdb::compactor::CompactJob;
//! use qmdb::tasks::BlockPairTaskHub;
//! use qmdb::test_helper::SimpleTask;
//! use std::sync::Arc;
//! use parking_lot::Mutex;
//!
//! // Create required components
//! let task_hub = Arc::new(BlockPairTaskHub::<SimpleTask>::new());
//! let (update_buffer_writer, _) = entrybuffer::new(0, 1024);
//! let update_buffer = Arc::new(Mutex::new(update_buffer_writer));
//! let entry_file = Arc::new(EntryFile::new(512, 2048, "/path/to/data".to_string(), None));
//! let indexer = Arc::new(Indexer::new(16));
//! let (_, compact_consumer) = qmdb::utils::ringchannel::new(100, &CompactJob { old_pos: 0, entry_bz: Vec::new() });
//!
//! // Create an updater for a shard
//! let updater = Updater::new(
//!     0,                    // shard_id
//!     task_hub,             // Task coordination
//!     update_buffer,        // Write buffer
//!     entry_file,           // Storage
//!     indexer,              // Entry indexing
//!     0,                    // curr_version
//!     0,                    // sn_start
//!     0,                    // sn_end
//!     compact_consumer,     // Compaction handling
//!     0,                    // compact_done_pos
//!     10,                   // utilization_div
//!     7,                    // utilization_ratio
//!     8,                    // compact_thres
//!     0,                    // next_task_id
//! );
//!
//! // Start the updater thread (requires a task receiver)
//! // let (task_sender, task_receiver) = crossbeam::channel::unbounded();
//! // updater.start_thread(task_receiver);
//! ```
//!
//! # Error Handling
//! The module handles various error conditions:
//! - Missing entries
//! - Version conflicts
//! - Buffer overflow
//! - I/O errors
//!
//! # Best Practices
//! 1. Configure appropriate buffer sizes
//! 2. Monitor cache hit rates
//! 3. Tune compaction parameters
//! 4. Handle errors appropriately
//! 5. Maintain consistent ordering

use crate::{
    compactor::CompactJob,
    entryfile::{EntryBufferWriter, EntryCache, EntryFile},
    hasher::Hash32,
    tasks::TaskHub,
    utils::{intset::IntSet, ringchannel::Consumer},
    Indexer,
};
use crossbeam::channel::{Receiver, TryRecvError};
use parking_lot::{ArcMutexGuard, Mutex, RawMutex};
use qmdb_common::def::{OP_CREATE, OP_DELETE, OP_READ, OP_WRITE};
use qmdb_common::{
    def::is_compactible,
    utils::{byte0_to_shard_id, OpRecord},
};
use qmdb_common::{entryfile::entry::entry_equal, indexer::indexer_trait::IndexerTrait};
use qmdb_common::{
    entryfile::{
        entry::{Entry, EntryBz},
        readbuf::ReadBuf,
    },
    mutator::{lookup_entry_in_pos_list, EntryMutatorBuilder, EntryStorage},
    task::taskid::{
        exclude_end_block, is_end_task_in_block, is_first_task_in_block, join_task_id,
        split_task_id,
    },
};
use smallvec::SmallVec;
use std::sync::Arc;

/// A type alias for the mutex guard used to protect the entry buffer writer.
///
/// This guard ensures thread-safe access to the buffer while minimizing
/// lock contention through efficient locking patterns.
type Guard = ArcMutexGuard<RawMutex, EntryBufferWriter>;

/// The Updater component manages updates to entries in a specific shard.
///
/// # Core Responsibilities
/// - Processing entry updates (create, write, delete)
/// - Managing entry versions and serial numbers
/// - Coordinating with task hub for ordered execution
/// - Handling compaction of old entries
/// - Maintaining cache consistency
///
/// # Architecture
/// The Updater implements a multi-layered approach:
/// 1. In-memory cache for hot entries
/// 2. Update buffer for recent changes
/// 3. Persistent storage for durability
///
/// # Performance
/// - Optimizes I/O through buffering
/// - Implements efficient caching
/// - Minimizes lock contention
/// - Handles compaction automatically
///
/// # Thread Safety
/// - Uses Arc for shared resources
/// - Implements proper locking
/// - Maintains consistency
/// - Handles concurrent access
pub struct Updater {
    /// ID of the shard this updater manages
    shard_id: usize,
    /// Task hub for coordinating task execution
    task_hub: Arc<dyn TaskHub>,
    /// Buffer for storing recent updates
    update_buffer: Arc<Mutex<EntryBufferWriter>>,
    /// Cache for storing frequently accessed entries
    cache: Arc<EntryCache>,
    /// File for persisting entries
    entry_file: Arc<EntryFile>,
    /// Indexer for looking up entry positions
    indexer: Arc<Indexer>,
    /// Buffer for reading entries (only accessed by Updater's functions)
    read_entry_buf: Option<ReadBuf>, // its content is only accessed by Updater's functions
    /// Current version number (contained in new entries)
    curr_version: i64, // will be contained by the new entries
    /// Starting serial number (increased after compacting old entries)
    sn_start: u64, // increased after compacting old entries
    /// Ending serial number (increased after appending new entries)
    sn_end: u64, // increased after appending new entries
    /// Consumer for receiving compact jobs
    compact_consumer: Consumer<CompactJob>,
    /// Position up to which compaction is complete
    compact_done_pos: i64,
    /// Divisor for calculating utilization
    utilization_div: i64,
    /// Target utilization ratio
    utilization_ratio: i64,
    /// Threshold for triggering compaction
    compact_thres: i64,
    /// Set of task IDs that are ready to run
    runnable_task_id_set: IntSet,
    /// ID of the next task to execute
    next_task_id: i64,
    /// Number of cache hits
    hit_count: u64,
    /// Number of cache misses
    miss_count: u64,
    /// Task IDs that mark the end of a block
    end_block_task_ids: SmallVec<[i64; 4]>, // TODO: need optimize this
}

impl Updater {
    /// Creates a new Updater instance for managing shard updates.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard this updater manages
    /// * `task_hub` - Task hub for coordinating task execution
    /// * `update_buffer` - Buffer for storing recent updates
    /// * `entry_file` - File for persisting entries
    /// * `indexer` - Indexer for looking up entry positions
    /// * `curr_version` - Current version number for new entries
    /// * `sn_start` - Starting serial number (post-compaction)
    /// * `sn_end` - Ending serial number (for new entries)
    /// * `compact_consumer` - Consumer for receiving compact jobs
    /// * `compact_done_pos` - Position up to which compaction is complete
    /// * `utilization_div` - Divisor for calculating storage utilization
    /// * `utilization_ratio` - Target storage utilization ratio
    /// * `compact_thres` - Threshold for triggering compaction
    /// * `next_task_id` - ID of the next task to execute
    ///
    /// # Performance
    /// - Constant-time initialization
    /// - Pre-allocates resources
    /// - Sets up efficient channels
    ///
    /// # Thread Safety
    /// - Safe to share across threads
    /// - Proper resource management
    /// - Consistent initialization
    pub fn new(
        shard_id: usize,
        task_hub: Arc<dyn TaskHub>,
        update_buffer: Arc<Mutex<EntryBufferWriter>>,
        entry_file: Arc<EntryFile>,
        indexer: Arc<Indexer>,
        curr_version: i64,
        sn_start: u64,
        sn_end: u64,
        compact_consumer: Consumer<CompactJob>,
        compact_done_pos: i64,
        utilization_div: i64,
        utilization_ratio: i64,
        compact_thres: i64,
        next_task_id: i64,
    ) -> Self {
        Self {
            shard_id,
            task_hub,
            update_buffer,
            cache: Arc::new(EntryCache::new_uninit()),
            entry_file,
            indexer,
            read_entry_buf: Some(ReadBuf::new()),
            curr_version,
            sn_start,
            sn_end,
            compact_consumer,
            compact_done_pos,
            utilization_div,
            utilization_ratio,
            compact_thres,
            runnable_task_id_set: IntSet::default(),
            next_task_id,
            hit_count: 0,
            miss_count: 0,
            end_block_task_ids: SmallVec::new(),
        }
    }

    /// Reads an entry from the cache, update buffer, or entry file.
    ///
    /// # Arguments
    /// * `ebw` - Entry buffer writer guard
    /// * `shard_id` - ID of the shard containing the entry
    /// * `file_pos` - Position of the entry in the file
    /// * `buf` - Buffer to read the entry into
    ///
    /// # Process Flow
    /// 1. Check cache for entry
    /// 2. Check update buffer if not in cache
    /// 3. Read from file if not in buffer
    ///
    /// # Performance
    /// - Optimizes for cache hits
    /// - Minimizes file I/O
    /// - Tracks hit/miss rates
    /// - Uses efficient buffering
    ///
    /// # Error Handling
    /// - Handles missing entries
    /// - Maintains consistency
    /// - Reports access errors
    fn read_entry(&mut self, ebw: &mut Guard, shard_id: usize, file_pos: i64, buf: &mut ReadBuf) {
        buf.clear();
        self.cache.lookup(shard_id, file_pos, buf);
        if !buf.is_empty() {
            self.hit_count += 1;
            return;
        }

        buf.clear();
        let _in_disk = ebw.get_entry_bz_at(file_pos, buf);
        if !buf.is_empty() {
            self.hit_count += 1;
            self.cache.insert(shard_id, file_pos, &buf.as_entry_bz());
            return;
        }
        self.miss_count += 1;
        let ef = &self.entry_file;

        #[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
        if !ef.read_entry_with_uring(file_pos, buf) {
            ef.read_entry(file_pos, buf);
        }
        #[cfg(any(not(target_os = "linux"), feature = "disable_dio"))]
        ef.read_entry(file_pos, buf);

        assert!(!buf.is_empty(), "file_pos={file_pos} is empty");
        self.cache.insert(shard_id, file_pos, &buf.as_entry_bz());
    }

    pub fn append_entry(&mut self, ebw: &mut Guard, entry: &Entry, deactivated_sns: &[u64]) -> i64 {
        ebw.append(entry, deactivated_sns)
    }

    fn lookup_entry<F>(
        &mut self,
        ebw: &mut Guard,
        height: i64,
        key_hash: &Hash32,
        read_buf: &mut ReadBuf,
        predicate: F,
    ) -> Option<i64>
    where
        F: Fn(&EntryBz) -> bool,
    {
        let pos_list = self.indexer.for_each_value(height, &key_hash[..]);
        lookup_entry_in_pos_list(
            pos_list.enumerate(),
            |pos, buf| self.read_entry(ebw, self.shard_id, pos, buf),
            read_buf,
            |_pos, entry| predicate(entry),
        )
    }

    fn lookup_adjacent_entry<F>(
        &mut self,
        ebw: &mut Guard,
        height: i64,
        key_hash: &Hash32,
        read_buf: &mut ReadBuf,
        predicate: F,
    ) -> Option<i64>
    where
        F: Fn(&EntryBz) -> bool,
    {
        let pos_list = self.indexer.for_each_adjacent_value(height, &key_hash[..]);
        lookup_entry_in_pos_list(
            pos_list.enumerate(),
            |pos, buf| self.read_entry(ebw, self.shard_id, pos, buf),
            read_buf,
            |_pos, entry| predicate(entry),
        )
    }

    /// Starts the updater thread for processing tasks.
    ///
    /// # Arguments
    /// * `mid` - Receiver for task IDs to process
    ///
    /// # Process Flow
    /// 1. Acquire update buffer lock
    /// 2. Process incoming task IDs
    /// 3. Release lock when waiting
    ///
    /// # Performance
    /// - Efficient lock management
    /// - Minimizes contention
    /// - Optimizes throughput
    ///
    /// # Thread Safety
    /// - Safe shutdown handling
    /// - Proper lock release
    /// - Consistent state
    pub fn start_thread(mut self, mid: Receiver<i64>) {
        std::thread::spawn(move || {
            let mut ebw = self.update_buffer.lock_arc();
            loop {
                let task_id = match mid.try_recv() {
                    Ok(id) => id,
                    Err(err) => match err {
                        TryRecvError::Empty => {
                            //if no data from receiver, release the lock
                            drop(ebw);
                            // re-acquire the lock when we got data from receiver
                            match mid.recv() {
                                Ok(id) => {
                                    ebw = self.update_buffer.lock_arc();
                                    id
                                }
                                Err(_) => break,
                            }
                        }
                        TryRecvError::Disconnected => break,
                    },
                };

                let new_task_id = exclude_end_block(task_id);
                // ensure task_id is increasing
                assert!(
                    new_task_id >= self.next_task_id,
                    "new_task_id {new_task_id} must >= next_task_id {}",
                    self.next_task_id
                );

                if is_end_task_in_block(task_id) {
                    self.end_block_task_ids.push(new_task_id);
                }
                if new_task_id == self.next_task_id {
                    self.run_task_with_ooo_id(&mut ebw, new_task_id);
                } else {
                    self.runnable_task_id_set.add(new_task_id);
                }
            }

            eprintln!("Updater thread exit");
        });
    }

    /// Handles out-of-order task IDs by maintaining proper execution order.
    ///
    /// # Arguments
    /// * `ebw` - Entry buffer writer guard
    /// * `task_id` - ID of the task to run
    ///
    /// # Process Flow
    /// 1. Add task to runnable set
    /// 2. Process tasks in order
    /// 3. Update next task ID
    ///
    /// # Ordering Guarantees
    /// - Maintains strict task order
    /// - Handles block boundaries
    /// - Ensures consistency
    ///
    /// # Performance
    /// - Efficient task tracking
    /// - Minimal reordering
    /// - Optimized processing
    fn run_task_with_ooo_id(&mut self, ebw: &mut Guard, task_id: i64) {
        let mut next_task_id = self.next_task_id;

        while task_id == next_task_id || self.runnable_task_id_set.remove(next_task_id) {
            let end_block = if let Some(pos) = self
                .end_block_task_ids
                .iter()
                .position(|&x| x == next_task_id)
            {
                self.end_block_task_ids.remove(pos);
                true
            } else {
                false
            };
            self.run_task(ebw, next_task_id, end_block);
            next_task_id = if end_block {
                let (height, _) = split_task_id(next_task_id);
                join_task_id(height + 1, 0, false)
            } else {
                next_task_id + 1
            };
        }
        self.next_task_id = next_task_id;
    }

    /// Runs a task by processing its change sets.
    ///
    /// # Arguments
    /// * `ebw` - Entry buffer writer guard
    /// * `task_id` - ID of the task to run
    /// * `end_block` - Whether this is the last task in a block
    ///
    /// # Process Flow
    /// 1. Retrieve task change sets
    /// 2. Process each operation:
    ///    - Create: Add new entries
    ///    - Write: Update existing entries
    ///    - Delete: Mark entries as deleted
    /// 3. Update indices and cache
    ///
    /// # Performance
    /// - Batches related operations
    /// - Minimizes I/O overhead
    /// - Optimizes cache usage
    /// - Handles compaction efficiently
    ///
    /// # Consistency
    /// - Maintains ACID properties
    /// - Ensures ordered execution
    /// - Preserves referential integrity
    fn run_task(&mut self, ebw: &mut Guard, task_id: i64, end_block: bool) {
        if is_first_task_in_block(task_id) {
            //task_index ==0 and new block start
            let height = split_task_id(task_id).0;
            self.curr_version = join_task_id(height, 0, false);

            let cache = self.task_hub.get_entry_cache(height);
            self.cache = cache;
        }
        for change_set in self.task_hub.get_change_sets(task_id).iter() {
            change_set.run_in_shard(self.shard_id, |op, key_hash, k, v, r| {
                self.compare_active_info(r);
                match op {
                    OP_WRITE => self.write_kv(ebw, key_hash, k, v, r),
                    OP_CREATE => self.create_kv(ebw, key_hash, k, v, r),
                    OP_DELETE => self.delete_kv(ebw, key_hash, k, r),
                    OP_READ => (), //used for debug
                    _ => {
                        panic!("Updater: unsupported operation");
                    }
                }
            });
            self.curr_version += 1;
        }
        if end_block {
            ebw.end_block(self.compact_done_pos, self.sn_start, self.sn_end);
        }
    }

    /// Writes a key-value pair to the database.
    ///
    /// # Arguments
    /// * `ebw` - Entry buffer writer guard
    /// * `key_hash` - Hash of the key to write
    /// * `key` - Key to write
    /// * `value` - Value to write
    /// * `r` - Optional operation record for verification
    ///
    /// # Process Flow
    /// 1. Check for existing entry
    /// 2. Create new entry with:
    ///    - Updated value
    ///    - Incremented version
    ///    - New serial number
    /// 3. Update indices and cache
    ///
    /// # Performance
    /// - Optimizes buffer usage
    /// - Minimizes I/O operations
    /// - Efficient index updates
    ///
    /// # Error Handling
    /// - Handles missing entries
    /// - Maintains consistency
    /// - Reports write failures
    fn write_kv(
        &mut self,
        ebw: &mut Guard,
        key_hash: &[u8; 32],
        key: &[u8],
        value: &[u8],
        r: Option<&Box<OpRecord>>,
    ) {
        let shard_id = self.shard_id;
        let curr_version = self.curr_version;
        let sn_end = self.sn_end;

        let mut buf = self.read_entry_buf.take().unwrap();
        buf.clear();

        let indexer = Arc::clone(&self.indexer);

        let mut storage = StorageDelegator::from((&mut *self, &mut *ebw));

        let mut mutator = EntryMutatorBuilder::new(&mut storage)
            .with_indexer(Arc::as_ref(&indexer))
            .build();

        let _result = mutator.update_kv(
            &mut buf,
            key_hash,
            key,
            value,
            shard_id,
            curr_version,
            sn_end,
            r,
        );

        self.read_entry_buf = Some(buf);
    }

    /// Deletes a key-value pair from the database.
    ///
    /// # Arguments
    /// * `ebw` - Entry buffer writer guard
    /// * `key_hash` - Hash of the key to delete
    /// * `key` - Key to delete
    /// * `r` - Optional operation record for verification
    ///
    /// # Process Flow
    /// 1. Locate existing entry
    /// 2. Create deletion marker with:
    ///    - Original key reference
    ///    - Updated version
    ///    - New serial number
    /// 3. Update indices
    ///
    /// # Performance
    /// - Lazy deletion strategy
    /// - Efficient index updates
    /// - Optimized for compaction
    ///
    /// # Consistency
    /// - Maintains deletion history
    /// - Preserves entry ordering
    /// - Handles concurrent access
    fn delete_kv(
        &mut self,
        ebw: &mut Guard,
        key_hash: &[u8; 32],
        key: &[u8],
        r: Option<&Box<OpRecord>>,
    ) {
        let shard_id = self.shard_id;
        let curr_version = self.curr_version;
        let sn_end = self.sn_end;

        let mut buf = self.read_entry_buf.take().unwrap();
        buf.clear();

        let indexer = Arc::clone(&self.indexer);

        let mut storage = StorageDelegator::from((&mut *self, &mut *ebw));

        let mut mutator = EntryMutatorBuilder::new(&mut storage)
            .with_indexer(Arc::as_ref(&indexer))
            .build();

        let _result = mutator.delete_kv(&mut buf, key_hash, key, shard_id, curr_version, sn_end, r);

        self.read_entry_buf = Some(buf);
    }

    /// Creates a new key-value pair in the database.
    ///
    /// # Arguments
    /// * `ebw` - Entry buffer writer guard
    /// * `key_hash` - Hash of the key to create
    /// * `key` - Key to create
    /// * `value` - Value to associate with the key
    /// * `r` - Optional operation record for verification
    ///
    /// # Process Flow
    /// 1. Verify key doesn't exist
    /// 2. Create new entry with:
    ///    - Initial version
    ///    - New serial number
    ///    - Key-value data
    /// 3. Update indices
    ///
    /// # Performance
    /// - Efficient entry creation
    /// - Optimized index updates
    /// - Minimal I/O overhead
    ///
    /// # Error Handling
    /// - Handles duplicate keys
    /// - Maintains consistency
    /// - Reports creation errors
    fn create_kv(
        &mut self,
        ebw: &mut Guard,
        key_hash: &[u8; 32],
        key: &[u8],
        value: &[u8],
        r: Option<&Box<OpRecord>>,
    ) {
        let shard_id = self.shard_id;
        let curr_version = self.curr_version;
        let sn_end = self.sn_end;

        let mut buf = self.read_entry_buf.take().unwrap();
        buf.clear();

        let indexer = Arc::clone(&self.indexer);

        let mut storage = StorageDelegator::from((&mut *self, &mut *ebw));

        let mut mutator = EntryMutatorBuilder::new(&mut storage)
            .with_indexer(Arc::as_ref(&indexer))
            .build();

        let _result = mutator.create_kv(
            &mut buf,
            key_hash,
            key,
            value,
            shard_id,
            curr_version,
            sn_end,
            r,
        );

        self.read_entry_buf = Some(buf);
    }

    /// Tests compaction functionality.
    ///
    /// # Arguments
    /// * `r` - Optional operation record for verification
    /// * `comp_idx` - Index for compaction tracking
    ///
    /// This is a test-only method that simulates compaction
    /// without the normal triggers and conditions.
    pub fn test_compact(&mut self, r: Option<&Box<OpRecord>>, comp_idx: usize) {
        self.cache = Arc::new(EntryCache::new());
        let mut ebw = self.update_buffer.lock_arc();
        self.compact(&mut ebw, r, comp_idx);
    }

    /// Performs compaction of old entries.
    ///
    /// # Arguments
    /// * `ebw` - Entry buffer writer guard
    /// * `r` - Optional operation record for verification
    /// * `comp_idx` - Index for compaction tracking
    ///
    /// # Process Flow
    /// 1. Check compaction criteria
    /// 2. Process compact jobs:
    ///    - Read old entries
    ///    - Verify active status
    ///    - Rewrite active entries
    /// 3. Update indices and positions
    ///
    /// # Performance
    /// - Batched processing
    /// - Efficient space reclamation
    /// - Optimized I/O patterns
    /// - Background execution
    ///
    /// # Storage Optimization
    /// - Maintains utilization ratio
    /// - Removes obsolete entries
    /// - Defragments storage
    /// - Updates reference chains

    fn try_twice_compact(&mut self, ebw: &mut Guard, r: Option<&Box<OpRecord>>) {
        if self.is_compactible() {
            self.compact(ebw, r, 0);
            self.compact(ebw, r, 1);
        }
    }

    fn compact(&mut self, ebw: &mut Guard, r: Option<&Box<OpRecord>>, comp_idx: usize) {
        // We check if an entry is active by comparing its position and serial number
        // against the indexer's records. If they match, this entry is the latest version.
        // If not, it means the entry was updated or deleted elsewhere.
        fn is_active_entry(
            indexer: &Indexer,
            key_hash: &[u8; 32],
            file_pos: i64,
            serial_number: u64,
        ) -> bool {
            indexer.key_exists(key_hash, file_pos, serial_number)
        }

        let (job, kh) = loop {
            let job = self.compact_consumer.consume();
            let e = EntryBz { bz: &job.entry_bz };
            let kh = e.key_hash();

            if is_active_entry(&self.indexer, &kh, job.old_pos, e.serial_number()) {
                break (job, kh);
            }
            self.compact_consumer.send_returned(job);
        };

        let entry_bz = EntryBz { bz: &job.entry_bz };

        compare_dig_entry(r, &entry_bz, comp_idx);

        let new_entry = Entry {
            key: entry_bz.key(),
            value: entry_bz.value(),
            next_key_hash: entry_bz.next_key_hash(),
            version: entry_bz.version(),
            serial_number: self.sn_end,
        };

        let dsn_list = [entry_bz.serial_number()];
        compare_put_entry(r, &new_entry, &dsn_list, comp_idx);

        let new_pos = ebw.append(&new_entry, &dsn_list);
        self.indexer
            .change_kv(&kh, job.old_pos, new_pos, dsn_list[0], self.sn_end);

        self.sn_end += 1;
        self.sn_start = entry_bz.serial_number() + 1;
        self.compact_done_pos = job.old_pos + entry_bz.len() as i64;

        // scenemaker needs compacted entry
        self.cache.insert(self.shard_id, job.old_pos, &entry_bz);
        self.compact_consumer.send_returned(job);
    }

    /// Checks if compaction should be triggered.
    ///
    /// # Returns
    /// `true` if compaction criteria are met, `false` otherwise
    ///
    /// # Criteria
    /// - Entry count threshold
    /// - Storage utilization
    /// - Active entry ratio
    ///
    /// # Performance Impact
    /// - Affects write latency
    /// - Influences storage efficiency
    /// - Impacts cache behavior
    fn is_compactible(&self) -> bool {
        is_compactible(
            self.utilization_div,
            self.utilization_ratio,
            self.compact_thres,
            self.indexer.len(self.shard_id),
            self.sn_start,
            self.sn_end,
        )
    }

    /// Compares active entry information with a record.
    ///
    /// # Arguments
    /// * `rec` - Optional operation record for verification
    ///
    /// # Verification
    /// - Entry existence
    /// - Version consistency
    /// - Serial number order
    /// - Index accuracy
    ///
    /// # Error Handling
    /// - Reports inconsistencies
    /// - Maintains audit trail
    /// - Enables debugging
    fn compare_active_info(&self, rec: Option<&Box<OpRecord>>) {
        if cfg!(feature = "check_rec") {
            _compare_active_info(self, rec);
        }
    }
}

/// Compares active information with a record for testing.
///
/// # Arguments
/// * `updater` - Updater instance
/// * `rec` - Optional operation record to compare against
fn _compare_active_info(updater: &Updater, rec: Option<&Box<OpRecord>>) {
    if let Some(rec) = rec {
        let num_active = updater.indexer.len(updater.shard_id);
        assert_eq!(rec.num_active, num_active, "incorrect num_active");
        assert_eq!(rec.oldest_active_sn, updater.sn_start, "incorrect sn_start");
    }
}

/// Compares an old entry with a record for testing.
///
/// # Arguments
/// * `rec` - Optional operation record to compare against
/// * `entry_bz` - Entry to compare
fn _compare_old_entry(rec: Option<&Box<OpRecord>>, entry_bz: &EntryBz) {
    if let Some(rec) = rec {
        let v = rec.rd_list.last().unwrap();
        assert_eq!(&v[..], entry_bz.bz, "compare_old_entry failed");
    }
}

/// Compares a previous entry with a record for testing.
///
/// # Arguments
/// * `rec` - Optional operation record to compare against
/// * `entry_bz` - Entry to compare
fn _compare_prev_entry(rec: Option<&Box<OpRecord>>, entry_bz: &EntryBz) {
    if let Some(rec) = rec {
        let v = rec.rd_list.first().unwrap();
        assert_eq!(&v[..], entry_bz.bz, "compare_prev_entry failed");
    }
}

/// Compares changes to a previous entry with a record for testing.
///
/// # Arguments
/// * `rec` - Optional operation record to compare against
/// * `entry` - Entry to compare
/// * `dsn_list` - List of deactivated serial numbers
fn _compare_prev_changed(rec: Option<&Box<OpRecord>>, entry: &Entry, dsn_list: &[u64]) {
    if let Some(rec) = rec {
        let v = rec.wr_list.first().unwrap();
        let equal = entry_equal(&v[..], entry, dsn_list);
        if !equal {
            let tmp = EntryBz { bz: &v[..] };
            let r = Entry::from_bz(&tmp);
            let key_hash = tmp.key_hash();
            let shard_id = byte0_to_shard_id(key_hash[0]);
            println!(
                "AA cmpr prev_C shard_id={shard_id}\nref={r:?}\nimp={entry:?}\ndsn_list={dsn_list:?}"
            );
            for (_, sn) in tmp.dsn_iter() {
                println!("--{sn}");
            }
        }
        assert!(equal, "compare_prev_changed failed");
    }
}

/// Compares a new entry with a record for testing.
///
/// # Arguments
/// * `rec` - Optional operation record to compare against
/// * `entry` - Entry to compare
/// * `dsn_list` - List of deactivated serial numbers
fn _compare_new_entry(rec: Option<&Box<OpRecord>>, entry: &Entry, dsn_list: &[u64]) {
    if let Some(rec) = rec {
        let v = rec.wr_list.last().unwrap();
        let equal = entry_equal(&v[..], entry, dsn_list);
        assert!(equal, "compare_new_entry failed");
    }
}

/// Compares a dug entry with a record for testing.
///
/// # Arguments
/// * `rec` - Optional operation record to compare against
/// * `entry_bz` - Entry to compare
/// * `comp_idx` - Index of the compaction job
fn _compare_dig_entry(rec: Option<&Box<OpRecord>>, entry_bz: &EntryBz, comp_idx: usize) {
    if let Some(rec) = rec {
        let v = rec.dig_list.get(comp_idx).unwrap();
        if &v[..] != entry_bz.bz {
            let tmp = EntryBz { bz: &v[..] };
            let r = Entry::from_bz(&tmp);
            let i = Entry::from_bz(entry_bz);
            let key_hash = entry_bz.key_hash();
            let shard_id = byte0_to_shard_id(key_hash[0]);
            println!(
                "AA cmpr dig_E shard_id={}\nref={:?}\nimp={:?}\nref={:?}\nimp={:?}",
                shard_id,
                r,
                i,
                &v[..],
                entry_bz.bz
            );
        }
        assert_eq!(&v[..], entry_bz.bz, "compare_dig_entry failed");
    }
}

/// Compares a put entry with a record for testing.
///
/// # Arguments
/// * `rec` - Optional operation record to compare against
/// * `entry` - Entry to compare
/// * `dsn_list` - List of deactivated serial numbers
/// * `comp_idx` - Index of the compaction job
fn _compare_put_entry(
    rec: Option<&Box<OpRecord>>,
    entry: &Entry,
    dsn_list: &[u64],
    comp_idx: usize,
) {
    if let Some(rec) = rec {
        let v = rec.put_list.get(comp_idx).unwrap();
        assert!(
            entry_equal(&v[..], entry, dsn_list),
            "compare_put_entry failed"
        );
    }
}

/// Public wrapper for _compare_dig_entry.
fn compare_dig_entry(rec: Option<&Box<OpRecord>>, entry_bz: &EntryBz, comp_idx: usize) {
    if cfg!(feature = "check_rec") {
        _compare_dig_entry(rec, entry_bz, comp_idx);
    }
}

/// Public wrapper for _compare_put_entry.
fn compare_put_entry(
    rec: Option<&Box<OpRecord>>,
    entry: &Entry,
    dsn_list: &[u64],
    comp_idx: usize,
) {
    if cfg!(feature = "check_rec") {
        _compare_put_entry(rec, entry, dsn_list, comp_idx);
    }
}

/// A storage delegator that provides an `EntryStorage` implementation by
/// wrapping an `Updater` instance.
///
/// This allows the mutator logic to interact with the database state
/// through a generic storage interface.
struct StorageDelegator<'a> {
    updater: &'a mut Updater,
    ebw: &'a mut Guard,
}

impl<'a> From<(&'a mut Updater, &'a mut Guard)> for StorageDelegator<'a> {
    fn from((updater, ebw): (&'a mut Updater, &'a mut Guard)) -> Self {
        Self { updater, ebw }
    }
}

impl EntryStorage for StorageDelegator<'_> {
    fn append_entry(&mut self, entry: &Entry, deactivated_sns: &[u64]) -> i64 {
        let pos = self.updater.append_entry(self.ebw, entry, deactivated_sns);
        self.updater.sn_end = self.updater.sn_end.saturating_add(1);
        pos
    }

    fn read_previous_entry<F>(
        &mut self,
        height: i64,
        key_hash: &Hash32,
        read_buf: &mut ReadBuf,
        predicate: F,
    ) -> i64
    where
        F: Fn(&EntryBz) -> bool,
    {
        let res = self
            .updater
            .lookup_adjacent_entry(self.ebw, height, key_hash, read_buf, predicate);

        if res.is_none() {
            panic!(
                "lookup_adjacent_entry failed height={height} key_hash={} ",
                hex::encode(key_hash)
            );
        }
        res.unwrap()
    }

    fn read_prior_entry<F>(
        &mut self,
        height: i64,
        key_hash: &Hash32,
        read_buf: &mut ReadBuf,
        predicate: F,
    ) -> i64
    where
        F: Fn(&EntryBz) -> bool,
    {
        let res = self
            .updater
            .lookup_entry(self.ebw, height, key_hash, read_buf, predicate);
        if res.is_none() {
            panic!(
                "lookup_prior_entry failed height={height} key_hash={} ",
                hex::encode(key_hash)
            );
        }
        res.unwrap()
    }

    fn try_twice_compact(&mut self) {
        self.updater.try_twice_compact(self.ebw, None);
    }
}

#[cfg(test)]
mod updater_tests {
    use crate::{
        entryfile::{entrybuffer, EntryFileWriter},
        tasks::BlockPairTaskHub,
        test_helper::{to_k80, SimpleTask},
        utils::ringchannel::{self, Producer},
    };
    use qmdb_common::entryfile::entry::entry_to_bytes;
    use qmdb_common::utils::OpRecord;
    use tempfile::Builder;

    use super::*;

    /// Creates a new Updater instance for testing.
    ///
    /// # Arguments
    /// * `prefix` - Prefix for temporary directory name
    ///
    /// # Returns
    /// A tuple containing:
    /// - Temporary directory
    /// - Updater instance
    /// - Producer for compact jobs
    fn new_updater(prefix: &str) -> (tempfile::TempDir, Updater, Producer<CompactJob>) {
        let temp_dir = Builder::new().prefix(prefix).tempdir().unwrap();
        let (entry_buffer_w, _entry_buffer_r) = entrybuffer::new(8, 1024);
        let cache_arc = Arc::new(EntryCache::new());
        let entry_file_arc = Arc::new(EntryFile::new(
            512,
            2048,
            temp_dir.path().to_str().unwrap().to_string(),
            None,
        ));
        let btree_arc = Arc::new(Indexer::new(16));
        let job = CompactJob {
            old_pos: 0,
            entry_bz: Vec::new(),
        };
        let (producer, consumer) = ringchannel::new(100, &job);
        let updater = Updater {
            shard_id: 0,
            task_hub: Arc::new(BlockPairTaskHub::<SimpleTask>::new()),
            update_buffer: Arc::new(Mutex::new(entry_buffer_w)),
            cache: cache_arc,
            entry_file: entry_file_arc,
            indexer: btree_arc,
            read_entry_buf: Some(ReadBuf::new()),
            curr_version: 0,
            sn_start: 0,
            sn_end: 0,
            compact_done_pos: 0,
            utilization_div: 10,
            utilization_ratio: 7,
            compact_thres: 8,
            runnable_task_id_set: IntSet::default(),
            next_task_id: 0,
            compact_consumer: consumer,
            hit_count: 0,
            miss_count: 0,
            end_block_task_ids: SmallVec::new(),
        };
        (temp_dir, updater, producer)
    }

    /// Creates a new test entry.
    fn new_test_entry<'a>() -> Entry<'a> {
        Entry {
            key: "key".as_bytes(),
            value: "value".as_bytes(),
            next_key_hash: &[0xab; 32],
            version: 12345,
            serial_number: 99999,
        }
    }

    /// Appends and flushes an entry to a file.
    ///
    /// # Arguments
    /// * `entry_file` - File to append to
    /// * `entry` - Entry to append
    /// * `dsn_list` - List of deactivated serial numbers
    ///
    /// # Returns
    /// Position of the appended entry
    fn append_and_flush_entry_to_file(
        entry_file: Arc<EntryFile>,
        entry: &Entry,
        dsn_list: &[u64],
    ) -> i64 {
        let mut w = EntryFileWriter::new(entry_file.clone(), 512);
        let mut entry_bz = [0u8; 512];
        let _entry_size = entry.dump(&mut entry_bz, dsn_list);
        let pos = w.append(&EntryBz { bz: &entry_bz[..] }).unwrap();
        let _ = w.flush();
        pos
    }

    /// Puts an entry in the cache.
    ///
    /// # Arguments
    /// * `updater` - Updater instance
    /// * `file_pos` - Position in the file
    /// * `entry` - Entry to cache
    /// * `dsn_list` - List of deactivated serial numbers
    fn put_entry_in_cache(updater: &Updater, file_pos: i64, entry: &Entry, dsn_list: &[u64]) {
        let mut read_buf = [0u8; 1024];
        let entry_size = entry.dump(&mut read_buf[..], dsn_list);
        let entry_bz = EntryBz {
            bz: &read_buf[..entry_size],
        };
        updater.cache.insert(updater.shard_id, file_pos, &entry_bz);
    }

    #[test]
    fn test_read_entry_cache_hit() {
        let (_temp_dir, mut updater, _producer) = new_updater("test_read_entry_cache_hit");

        let entry = new_test_entry();
        let dsn_list = [1, 2, 3, 4];
        put_entry_in_cache(&updater, 123, &entry, &dsn_list);

        let mut ebw = updater.update_buffer.lock_arc();
        let mut read_entry_buf = ReadBuf::new();
        updater.read_entry(&mut ebw, updater.shard_id, 123, &mut read_entry_buf);
        assert_eq!(
            "03050000046b657976616c7565ababababababab",
            hex::encode(&read_entry_buf.as_slice()[0..20])
        );
    }

    #[test]
    fn test_read_entry_from_buffer() {
        let (_temp_dir, mut updater, _producer) = new_updater("test_read_entry_from_buffer");
        let entry = new_test_entry();
        let dsn_list = [1, 2, 3, 4];
        let pos = updater.update_buffer.lock_arc().append(&entry, &dsn_list);

        let mut ebw = updater.update_buffer.lock_arc();
        let mut read_entry_buf = ReadBuf::new();
        updater.read_entry(&mut ebw, 7, pos, &mut read_entry_buf);
        assert_eq!(
            "03050000046b657976616c7565ababababababab",
            hex::encode(&read_entry_buf.as_slice()[0..20])
        );
    }

    #[test]
    fn test_read_entry_from_file() {
        let (_temp_dir, mut updater, _producer) = new_updater("test_read_entry_from_file");
        let entry = new_test_entry();
        let dsn_list = [1, 2, 3, 4];
        let pos = append_and_flush_entry_to_file(updater.entry_file.clone(), &entry, &dsn_list);

        let mut ebw = updater.update_buffer.lock_arc();
        let mut read_entry_buf = ReadBuf::new();
        updater.read_entry(&mut ebw, 7, pos, &mut read_entry_buf);
        assert_eq!(
            "03050000046b657976616c7565ababababababab",
            hex::encode(&read_entry_buf.as_slice()[0..20])
        );
    }

    #[test]
    #[should_panic(expected = "incorrect num_active")]
    fn test_compare_active_info1() {
        let (_temp_dir, updater, _producer) = new_updater("test_compare_active_info1");
        let mut op = Box::new(OpRecord::new(0));
        op.num_active = 123;
        let rec = Option::Some(&op);
        _compare_active_info(&updater, rec);
    }

    #[test]
    #[should_panic(expected = "incorrect sn_start")]
    fn test_compare_active_info2() {
        let (_temp_dir, updater, _producer) = new_updater("test_compare_active_info2");
        let mut op = Box::new(OpRecord::new(0));
        op.oldest_active_sn = 123;
        let rec = Option::Some(&op);
        _compare_active_info(&updater, rec);
    }

    #[test]
    fn test_create_kv() {
        let (_temp_dir, mut updater, _producer) = new_updater("test_create_kv");

        let mut entry = new_test_entry();
        let hash = qmdb_common::entryfile::entry::entry_key_hash(entry.key);
        entry.next_key_hash = [0xff; 32].as_slice();
        let dsn_list = [];
        let pos = append_and_flush_entry_to_file(updater.entry_file.clone(), &entry, &dsn_list);

        updater.indexer.add_kv(&hash[..10], pos, 0);
        assert_eq!(1, updater.indexer.len(0));
        assert_eq!(0, updater.sn_end);

        let mut ebw = updater.update_buffer.lock_arc();
        let mut new_hash = hash.to_vec();
        new_hash[31] = 0xff;
        updater.create_kv(
            &mut ebw,
            &new_hash.try_into().unwrap(),
            "key".as_bytes(),
            "value".as_bytes(),
            Option::None,
        );

        assert_eq!(2, updater.indexer.len(0));
        assert_eq!(2, updater.sn_end);
        // TODO: check more
    }

    #[test]
    #[ignore = "WIP"]
    fn test_write_kv() {
        let (_temp_dir, mut updater, _producer) = new_updater("test_write_kv");
        let entry = new_test_entry();
        let dsn_list = [];
        let pos = append_and_flush_entry_to_file(updater.entry_file.clone(), &entry, &dsn_list);

        updater
            .indexer
            .add_kv(&to_k80(0x7777_0000_0000_0000), pos, 0);
        let mut ebw = updater.update_buffer.lock_arc();
        updater.create_kv(
            &mut ebw,
            &[0x77u8; 32],
            "key".as_bytes(),
            "value".as_bytes(),
            Option::None,
        );
        assert_eq!(2, updater.indexer.len(0));
        assert_eq!(2, updater.sn_end);

        updater.write_kv(
            &mut ebw,
            &[0x77u8; 32],
            "key".as_bytes(),
            "val2".as_bytes(),
            Option::None,
        );
        assert_eq!(2, updater.indexer.len(0));
        assert_eq!(3, updater.sn_end);
        // TODO: check more
    }

    #[test]
    #[ignore = "WIP"]
    fn test_delete_kv() {
        let (_temp_dir, mut updater, _producer) = new_updater("test_create_kv");

        let mut entry = new_test_entry();
        let hash = qmdb_common::entryfile::entry::entry_key_hash(entry.key);
        entry.next_key_hash = [0xff; 32].as_slice();
        let dsn_list = [];
        let pos = append_and_flush_entry_to_file(updater.entry_file.clone(), &entry, &dsn_list);
        updater.indexer.add_kv(&hash[..10], pos, 0);

        let mut ebw = updater.update_buffer.lock_arc();
        let mut new_hash = hash;
        new_hash[31] = 0xff;
        updater.create_kv(
            &mut ebw,
            &new_hash,
            "key".as_bytes(),
            "value".as_bytes(),
            Option::None,
        );

        let entry2 = Entry {
            key: "key2".as_bytes(),
            value: "val2".as_bytes(),
            next_key_hash: &new_hash,
            version: 12345,
            serial_number: 100000,
        };
        let hash2 = qmdb_common::entryfile::entry::entry_key_hash(entry2.key);
        let pos2: i64 =
            append_and_flush_entry_to_file(updater.entry_file.clone(), &entry2, &dsn_list);
        put_entry_in_cache(&updater, pos2, &entry2, &dsn_list);
        updater.indexer.add_kv(&hash2, pos2, 0);
        assert_eq!(3, updater.indexer.len(0));
        assert_eq!(2, updater.sn_end);

        updater.delete_kv(
            &mut ebw,
            &hash.try_into().unwrap(),
            "key".as_bytes(),
            Option::None,
        );
        assert_eq!(1, updater.indexer.len(0));
        assert_eq!(3, updater.sn_end);
        // TODO: check more
    }

    #[test]
    fn test_is_compactible() {
        // utilization: 60%
        let (_temp_dir, mut updater, _producer) = new_updater("test_is_compactible");
        updater.sn_start = 0;
        updater.sn_end = 20;
        updater.compact_thres = 10;

        for i in 0..20 {
            updater.indexer.add_kv(&to_k80(i), (i * 8) as i64, 0);
            assert_eq!(8 < i && i < 14, updater.is_compactible());
        }

        updater.sn_end = 40;
        assert!(updater.is_compactible());

        updater.compact_thres = 41;
        assert!(!updater.is_compactible());
    }

    #[test]
    fn test_try_compact() {
        let (_temp_dir, mut updater, mut producer) = new_updater("test_try_compact");
        let entry = new_test_entry();
        let dsn_list = [0u64; 0];
        let mut read_buf = [0u8; 500];
        let entry_bz = entry_to_bytes(&entry, &dsn_list, &mut read_buf);
        let pos = append_and_flush_entry_to_file(updater.entry_file.clone(), &entry, &dsn_list);
        let kh = entry_bz.key_hash();
        updater.indexer.add_kv(&kh[..], pos, 0);
        updater.sn_end = 10;
        updater.compact_thres = 0;
        updater.utilization_ratio = 1;
        assert!(updater.is_compactible());
        assert_eq!(1, updater.indexer.len(0));
        assert_eq!(10, updater.sn_end);

        producer
            .produce(CompactJob {
                old_pos: 0,
                entry_bz: read_buf.to_vec(),
            })
            .unwrap();
        producer.receive_returned().unwrap();

        let mut ebw = updater.update_buffer.lock_arc();
        updater.compact(&mut ebw, Option::None, 0);
        assert_eq!(1, updater.indexer.len(0));
        assert_eq!(11, updater.sn_end);
        // TODO: check mores
    }
}

#[cfg(test)]
mod compare_tests {
    use super::*;
    use qmdb_common::{entryfile::helpers::EntryBuilder, utils::OpRecord};

    fn new_test_entry<'a>() -> Entry<'a> {
        Entry {
            key: "key".as_bytes(),
            value: "value".as_bytes(),
            next_key_hash: &[0xab; 32],
            version: 12345,
            serial_number: 99999,
        }
    }

    #[test]
    #[should_panic(expected = "compare_old_entry failed")]
    fn test_compare_old_entry() {
        let mut op = Box::new(OpRecord::new(0));
        op.rd_list.push(vec![4, 5, 6]);
        op.rd_list.push(vec![1, 2, 3]);
        let rec = Option::Some(&op);
        let bz: [u8; 3] = [4, 5, 6];
        _compare_old_entry(rec, &EntryBz { bz: &bz[..] });
    }

    #[test]
    #[should_panic(expected = "compare_prev_entry failed")]
    fn test_compare_prev_entry() {
        let mut op = Box::new(OpRecord::new(0));
        op.rd_list.push(vec![1, 2, 3]);
        op.rd_list.push(vec![4, 5, 6]);
        let rec = Option::Some(&op);
        let bz: [u8; 3] = [4, 5, 6];
        _compare_prev_entry(rec, &EntryBz { bz: &bz[..] });
    }

    #[test]
    #[should_panic(expected = "compare_prev_changed failed")]
    fn test_compare_prev_changed() {
        let entry = new_test_entry();
        let dsn_list: [u64; 4] = [1, 2, 3, 4];

        let mut op = Box::new(OpRecord::new(0));
        op.wr_list
            .push(EntryBuilder::kv("abc", "def").build_and_dump(&[]));
        op.wr_list.push(vec![4, 5, 6]);
        let rec = Option::Some(&op);
        _compare_prev_changed(rec, &entry, &dsn_list);
    }

    #[test]
    #[should_panic(expected = "compare_new_entry failed")]
    fn test_compare_new_entry() {
        let entry = new_test_entry();
        let dsn_list: [u64; 4] = [1, 2, 3, 4];

        let mut op = Box::new(OpRecord::new(0));
        op.wr_list.push(vec![1, 2, 3]);
        op.wr_list.push(vec![4, 5, 6]);
        let rec = Option::Some(&op);
        _compare_new_entry(rec, &entry, &dsn_list);
    }

    #[test]
    #[should_panic(expected = "compare_dig_entry failed")]
    fn test_compare_dig_entry() {
        let entry1 = EntryBuilder::kv("abc", "def").build_and_dump(&[]);
        let entry2 = EntryBuilder::kv("hhh", "www").build_and_dump(&[]);
        let entry3 = EntryBuilder::kv("123", "456").build_and_dump(&[]);

        let mut op = Box::new(OpRecord::new(0));
        op.dig_list.push(entry1);
        op.rd_list.push(entry2);
        let rec = Option::Some(&op);
        _compare_dig_entry(rec, &EntryBz { bz: &entry3 }, 0);
    }

    #[test]
    #[should_panic(expected = "compare_put_entry failed")]
    fn test_compare_put_entry() {
        let entry = new_test_entry();
        let dsn_list: [u64; 4] = [1, 2, 3, 4];

        let mut op = Box::new(OpRecord::new(0));
        op.put_list.push(vec![1, 2, 3]);
        op.put_list.push(vec![4, 5, 6]);
        let rec = Option::Some(&op);
        _compare_put_entry(rec, &entry, &dsn_list, 1);
    }
}

