//! Flushing and synchronization module for QMDB.
//!
//! This module implements the critical functionality of persisting database state
//! and maintaining consistency across multiple shards. It handles:
//!
//! # Core Responsibilities
//! - Atomic flushing of in-memory buffers to disk
//! - Coordinated multi-shard state transitions
//! - Merkle tree updates and proof generation
//! - Efficient pruning of historical data
//! - Metadata synchronization
//! - Proof request handling
//!
//! # Architecture
//! The flushing system consists of several key components:
//!
//! ## Flusher
//! The main coordinator that:
//! - Manages multiple FlusherShards
//! - Coordinates block height progression
//! - Handles metadata updates
//! - Manages pruning policies
//!
//! ## FlusherShard
//! Shard-specific component that:
//! - Manages buffer flushing
//! - Updates Merkle trees
//! - Handles proof requests
//! - Maintains shard-level consistency
//!
//! ## Synchronization
//! Uses multiple mechanisms:
//! - Barriers for multi-shard coordination
//! - Channels for proof requests
//! - Locks for metadata access
//! - Condvars for request notification
//!
//! # Process Flow
//! 1. Block Processing:
//!    - Increment block height
//!    - Calculate pruning boundaries
//!    - Spawn shard-specific flush tasks
//!
//! 2. Shard Flushing:
//!    - Process buffered entries
//!    - Update Merkle trees
//!    - Handle proof requests
//!    - Perform pruning if needed
//!
//! 3. Synchronization:
//!    - Coordinate across shards
//!    - Update metadata
//!    - Signal block completion
//!
//! # Performance Considerations
//! - Uses efficient barrier synchronization
//! - Implements parallel processing across shards
//! - Optimizes proof generation and verification
//! - Manages memory pressure through pruning
//!
//! # Configuration
//! Key parameters include:
//! - `MAX_PROOF_REQ`: Maximum concurrent proof requests
//! - `MIN_PRUNE_COUNT`: Minimum entries for pruning
//! - `PRUNE_EVERY_NBLOCKS`: Pruning frequency
//! - `SHARD_COUNT`: Number of parallel shards
//!
//! # Example Usage
//! ```no_run
//! use qmdb::flusher::{flusher::Flusher, shard::FlusherShard, barrier_set::BarrierSet};
//! use qmdb::merkletree::Tree;
//! use qmdb::metadb::MetaDB;
//! use qmdb_common::def::SHARD_COUNT;
//! use std::sync::Arc;
//! use crossbeam::channel::unbounded;
//! use parking_lot::RwLock;
//!
//! // Create required components
//! let tree = Tree::new(2048, 0, 512, "/path/to/data".to_string(), "/path/to/metadb".to_string(), true, None);
//! let meta_db = Arc::new(RwLock::new(MetaDB::initialize_dir("/path/to/metadb", None, 2)));
//! let oldest_sn = 0u64;
//! let curr_height = 1i64;
//! let max_height = 100i64;
//! let (end_block_sender, end_block_chan) = unbounded();
//! let (prune_sender, _prune_receiver) = unbounded();
//!
//! // Create a single flusher shard (for demonstration)
//! let shard = FlusherShard::new(
//!     tree,              // Merkle tree for this shard
//!     oldest_sn,         // Oldest active serial number
//!     0,                 // Shard identifier
//! );
//! let shards = vec![Box::new(shard)];
//!
//! // Create main flusher
//! let mut flusher = Flusher::new(
//!     shards,              // Vector of shard flushers
//!     meta_db,             // Metadata database
//!     curr_height,         // Current block height
//!     max_height,          // Maximum blocks to keep
//!     end_block_sender,    // Block completion channel
//!     prune_sender,        // Pruned-segment notifications for the prefetcher
//! );
//!
//! // Start flushing
//! flusher.flush(1);
//! ```
//!
//! # Error Handling
//! The module handles various error conditions:
//! - Channel closure or overflow
//! - I/O errors during flushing
//! - Proof generation failures
//! - Synchronization timeouts
//!
//! # Best Practices
//! 1. Configure appropriate pruning parameters
//! 2. Monitor flush latency and throughput
//! 3. Balance proof request load
//! 4. Implement proper error recovery
//! 5. Maintain backup strategies

use crate::entryfile::{EntryBufferReader, EntryFile};
use crate::metadb::{MetaDB, MetaInfo};
use parking_lot::RwLock;
use qmdb_common::def::SHARD_COUNT;
use std::sync::Arc;
use std::thread;
use crate::prefetcher::dioprefetcher::PruneCmd;

use super::barrier_set::BarrierSet;
use super::shard::{FlusherShard, ProofReqElem};

/// Type alias for the metadata database implementation.
/// Uses RocksDB for efficient key-value storage with atomic batch operations.
type RocksMetaDB = MetaDB;

/// Synchronization barriers for coordinating multi-shard operations.
///
/// Ensures all shards complete their operations before proceeding to the next phase,
/// maintaining consistency across the entire database.
///
/// # Synchronization Points
/// - Flush Barrier: Coordinates buffer flushing
/// - MetaDB Barrier: Coordinates metadata updates
///
/// # Performance
/// - Minimizes synchronization overhead
/// - Enables parallel processing where possible
/// - Maintains consistency guarantees

/// Type representing a proof request with synchronization primitives.
///
/// Combines:
/// - Serial number of the requested entry
/// - Optional result containing proof path or error
/// - Synchronization primitives for thread safety
///
/// # Thread Safety
/// - Uses Mutex for exclusive access
/// - Uses Condvar for efficient waiting
/// - Wrapped in Arc for safe sharing

/// Main flusher component that coordinates operations across all shards.
///
/// The Flusher is responsible for:
/// - Managing block height progression
/// - Coordinating multi-shard operations
/// - Handling pruning policies
/// - Maintaining metadata consistency
///
/// # Architecture
/// - Uses a vector of shard-specific flushers
/// - Maintains global metadata state
/// - Coordinates block transitions
/// - Manages pruning lifecycle
///
/// # Performance
/// - Parallel processing across shards
/// - Efficient metadata updates
/// - Optimized pruning strategies
/// - Minimal synchronization overhead
#[allow(clippy::vec_box)]
pub struct Flusher {
    /// Individual flusher components for each shard
    shards: Vec<Box<FlusherShard>>,
    /// Reference to the metadata database
    meta: Arc<RwLock<RocksMetaDB>>,
    /// Current block height
    curr_height: i64,
    /// Maximum height of blocks to keep
    max_kept_height: i64,
    /// Channel for signaling block completion
    end_block_chan: crossbeam::channel::Sender<Arc<MetaInfo>>,
    /// [Added] 保存广播通道的发送端
    prune_sender: crossbeam::channel::Sender<PruneCmd>,
}

impl Flusher {
    /// Creates a new Flusher instance to coordinate multi-shard operations.
    ///
    /// # Arguments
    /// * `shards` - Vector of shard-specific flushers
    /// * `meta` - Reference to the metadata database
    /// * `curr_height` - Current block height
    /// * `max_kept_height` - Maximum height of blocks to keep
    /// * `end_block_chan` - Channel for signaling block completion
    ///
    /// # Performance
    /// - Constant-time initialization
    /// - Pre-allocates resources for efficiency
    /// - Sets up optimal channel capacities
    ///
    /// # Thread Safety
    /// - Safe to share across threads
    /// - Uses Arc for shared resources
    /// - Maintains consistent state
    pub fn new(
        shards: Vec<Box<FlusherShard>>,
        meta: Arc<RwLock<RocksMetaDB>>,
        curr_height: i64,
        max_kept_height: i64,
        end_block_chan: crossbeam::channel::Sender<Arc<MetaInfo>>,
        // [Added] 构造时传入 sender
        prune_sender: crossbeam::channel::Sender<PruneCmd>,
    ) -> Self {
        Self {
            shards,
            meta,
            curr_height,
            max_kept_height,
            end_block_chan,
            prune_sender,
        }
    }

    /// Gets the proof request senders for all shards.
    ///
    /// This method provides channels for submitting proof requests to each shard,
    /// allowing concurrent proof generation across shards.
    ///
    /// # Returns
    /// Vector of channels for sending proof requests to each shard
    ///
    /// # Performance
    /// - Pre-allocates vector capacity
    /// - Constant-time channel cloning
    /// - Thread-safe channel access
    pub fn get_proof_req_senders(&self) -> Vec<crossbeam::channel::Sender<ProofReqElem>> {
        let mut v = Vec::with_capacity(SHARD_COUNT);
        for i in 0..SHARD_COUNT {
            v.push(self.shards[i].proof_req_sender.clone());
        }
        v
    }

    /// Main flush loop that coordinates operations across all shards.
    ///
    /// This method implements the core flushing logic:
    /// 1. Block Height Management:
    ///    - Increments block height
    ///    - Calculates pruning boundaries
    ///
    /// 2. Shard Coordination:
    ///    - Spawns flush tasks for each shard
    ///    - Uses barriers for synchronization
    ///    - Handles metadata updates
    ///
    /// 3. Pruning:
    ///    - Removes data beyond max_kept_height
    ///    - Coordinates pruning across shards
    ///    - Updates indices accordingly
    ///
    /// # Arguments
    /// * `shard_count` - Number of shards to coordinate
    ///
    /// # Performance
    /// - Parallel processing across shards
    /// - Efficient barrier synchronization
    /// - Optimized pruning strategies
    /// - Minimal lock contention
    ///
    /// # Error Handling
    /// - Handles thread panics
    /// - Maintains consistency on errors
    /// - Logs critical issues
    pub fn flush(&mut self, shard_count: usize) {
        loop {
            self.curr_height += 1;
            let prune_to_height = self.curr_height - self.max_kept_height;
            let bar_set = Arc::new(BarrierSet::new(shard_count));
            thread::scope(|s| {
                for shard in self.shards.iter_mut() {
                    let bar_set = bar_set.clone();
                    let curr_height = self.curr_height;
                    let meta = self.meta.clone();
                    let end_block_chan = self.end_block_chan.clone();
                    let prune_sender = self.prune_sender.clone();
                    s.spawn(move || {
                        shard.flush(prune_to_height, curr_height, meta, bar_set, end_block_chan, prune_sender);
                    });
                }
            });
        }
    }

    /// Gets the entry file for a specific shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    ///
    /// # Returns
    /// Reference to the shard's entry file
    ///
    /// # Thread Safety
    /// - Returns Arc for safe sharing
    /// - Maintains consistent access
    pub fn get_entry_file(&self, shard_id: usize) -> Arc<EntryFile> {
        self.shards[shard_id].tree.entry_file_wr.entry_file.clone()
    }

    /// Sets the entry buffer reader for a specific shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    /// * `ebr` - Entry buffer reader to set
    ///
    /// # Performance
    /// - Zero-copy buffer management
    /// - Efficient reader switching
    pub fn set_entry_buf_reader(&mut self, shard_id: usize, ebr: EntryBufferReader) {
        self.shards[shard_id].buf_read = Some(ebr);
    }
}

#[cfg(test)]
mod flusher_tests {
    use super::*;
    use crate::entryfile::entrybuffer;
    use crate::merkletree::check::check_hash_consistency;
    use crate::merkletree::{
        recover::{bytes_to_edge_nodes, recover_tree},
        singleproof::verify_single_proof,
        Tree,
    };
    use crate::metadb::MetaDB;
    use crate::test_helper::cipher::create_cipher;
    use crossbeam::channel::bounded;
    use parking_lot::RwLock;
    use qmdb_common::def::{
        DEFAULT_ENTRY_SIZE, DEFAULT_FILE_SIZE, ENTRIES_PATH, SENTRY_COUNT, SMALL_BUFFER_SIZE,
        TWIG_PATH, TWIG_SHIFT,
    };
    use qmdb_common::entryfile::entry::{entry_to_bytes, sentry_entry, Entry};
    use qmdb_common::entryfile::readbuf::ReadBuf;
    use std::sync::Arc;
    use std::thread::sleep;
    use std::{fs, thread, time};

    #[test]
    fn test_flusher() {
        let temp_dir = tempfile::Builder::new()
            .prefix("test_flusher")
            .tempdir()
            .unwrap();
        let dir_path = temp_dir.path().to_str().unwrap().to_string();

        // Create all required directories
        let meta_dir = format!("{dir_path}/metadb");
        let data_dir = format!("{dir_path}/data");
        let dir_entry = format!("{}/{}{}", data_dir, ENTRIES_PATH, ".test");
        let dir_twig = format!("{}/{}{}", data_dir, TWIG_PATH, ".test");

        fs::create_dir_all(&dir_entry).unwrap();
        fs::create_dir_all(&dir_twig).unwrap();

        let cipher = create_cipher();

        let (eb_sender, _eb_receiver) = bounded(2);
        let meta = Arc::new(RwLock::new(MetaDB::initialize_dir(&meta_dir, None, 2)));

        let mut flusher = Flusher {
            shards: Vec::with_capacity(1),
            meta: meta.clone(),
            curr_height: 0,
            max_kept_height: 1000,
            end_block_chan: eb_sender,
            prune_sender: bounded(1).0,
        };

        let shard_id = 0;
        let mut tree = Tree::new(
            0,
            SMALL_BUFFER_SIZE as usize,
            DEFAULT_FILE_SIZE,
            data_dir.clone(),
            ".test".to_string(),
            true,
            cipher.clone(),
        );

        let (entry_file_size, twig_file_size);
        {
            let mut meta = meta.write_arc();
            let mut bz = [0u8; DEFAULT_ENTRY_SIZE];
            for sn in 0..SENTRY_COUNT {
                let e = sentry_entry(shard_id, sn as u64, &mut bz[..]);
                tree.append_entry(&e).unwrap();
            }
            let n_list = tree.flush_files(0, 0);
            let n_list = tree.upper_tree.evict_twigs(n_list, 0, 0);
            tree.upper_tree
                .sync_upper_nodes(n_list, tree.youngest_twig_id);
            check_hash_consistency(&tree);
            (entry_file_size, twig_file_size) = tree.get_file_sizes();
            meta.set_entry_file_size(shard_id, entry_file_size);
            meta.set_twig_file_size(shard_id, twig_file_size);
            meta.set_next_serial_num(shard_id, SENTRY_COUNT as u64);
            meta.insert_extra_data(0, "".to_owned());
            meta.commit();
        }

        let entry_file = tree.entry_file_wr.entry_file.clone();
        let fs = FlusherShard::new(tree, 0, shard_id);

        flusher.shards.push(Box::new(fs));
        let _tree_p = &mut flusher.shards[0].tree as *mut Tree;
        let (mut u_eb_wr, u_eb_rd) = entrybuffer::new(entry_file_size, SMALL_BUFFER_SIZE as usize);
        flusher.shards[shard_id].buf_read = Some(u_eb_rd);
        // prepare entry
        let e0 = Entry {
            key: "Key0Key0Key0Key0Key0Key0Key0Key0Key0".as_bytes(),
            value: "Value0Value0Value0Value0Value0Value0".as_bytes(),
            next_key_hash: [1; 32].as_slice(),
            version: 1,
            serial_number: SENTRY_COUNT as u64,
        };
        let mut buf = [0; 1024];
        let bz0 = entry_to_bytes(&e0, &[], &mut buf);
        let pos0 = u_eb_wr.append(&e0, &[]);
        u_eb_wr.end_block(0, 0, SENTRY_COUNT as u64);
        let _handler = thread::spawn(move || {
            flusher.flush(1);
        });
        sleep(time::Duration::from_secs(3));
        let mut buf = ReadBuf::new();
        entry_file.read_entry(pos0, &mut buf);
        assert_eq!(buf.as_slice(), bz0.bz);

        let meta = meta.read_arc();
        let oldest_active_sn = meta.get_oldest_active_sn(shard_id);
        let oldest_active_twig_id = oldest_active_sn >> TWIG_SHIFT;
        let youngest_twig_id = meta.get_youngest_twig_id(shard_id);
        let edge_nodes = bytes_to_edge_nodes(&meta.get_edge_nodes(shard_id));
        let (last_pruned_twig_id, ef_prune_to) = meta.get_last_pruned_twig(shard_id);
        let root = meta.get_root_hash(shard_id);
        let entryfile_size = meta.get_entry_file_size(shard_id);
        let twigfile_size = meta.get_twig_file_size(shard_id);
        let (tree, recovered_root) = recover_tree(
            0,
            SMALL_BUFFER_SIZE as usize,
            DEFAULT_FILE_SIZE as usize,
            true,
            data_dir.to_string(),
            ".test".to_string(),
            &edge_nodes,
            last_pruned_twig_id,
            ef_prune_to,
            oldest_active_twig_id,
            youngest_twig_id,
            &[entryfile_size, twigfile_size],
            cipher,
        );
        assert_eq!(recovered_root, root);
        check_hash_consistency(&tree);
        //let mut proof_path = tree.get_proof(SENTRY_COUNT as u64).unwrap();
        //check_proof(&mut proof_path).unwrap();

        // Also test compact single-proof
        let sp = tree.get_single_proof(SENTRY_COUNT as u64).unwrap();
        assert!(verify_single_proof(&sp));
    }
}

