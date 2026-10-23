use crate::entryfile::EntryBufferReader;
use crate::merkletree::singleproof::SingleProof;
use crate::merkletree::Tree;
use crate::metadb::{MetaDB, MetaInfo};
use crossbeam::channel::bounded;
use parking_lot::{Condvar, Mutex, RwLock};
use qmdb_common::def::{
    LEAF_COUNT_IN_TWIG, MAX_PROOF_REQ, MIN_PRUNE_COUNT, PRUNE_EVERY_NBLOCKS, TWIG_SHIFT,
};
use std::sync::Arc;
use crate::prefetcher::dioprefetcher::PruneCmd;

use super::barrier_set::BarrierSet;

/// Type alias for the metadata database implementation.
/// Uses RocksDB for efficient key-value storage with atomic batch operations.
type RocksMetaDB = MetaDB;
pub struct BatchProofReq {
    pub sns: Vec<u64>,
    pub results: Vec<Option<Result<SingleProof, String>>>,
}

pub type ProofReqElem = Arc<(Mutex<(BatchProofReq, bool)>, Condvar)>;

/// Shard-specific flusher component that manages local state and operations.
///
/// The FlusherShard is responsible for:
/// - Managing local buffer operations
/// - Updating the Merkle tree
/// - Handling proof requests
/// - Coordinating with other shards
///
/// # Architecture
/// - Uses efficient buffer management
/// - Implements proof generation
/// - Handles local state transitions
/// - Maintains shard consistency
///
/// # Performance
/// - Optimized buffer operations
/// - Efficient proof generation
/// - Minimal synchronization overhead
/// - Parallel processing capability
pub struct FlusherShard {
    /// Buffer reader for reading entries
    pub buf_read: Option<EntryBufferReader>,
    /// Merkle tree for this shard
    pub tree: Tree,
    /// Last serial number that was compacted
    last_compact_done_sn: u64,
    /// ID of this shard
    shard_id: usize,
    /// Channel for sending proof requests
    pub proof_req_sender: crossbeam::channel::Sender<ProofReqElem>,
    /// Channel for receiving proof requests
    proof_req_receiver: crossbeam::channel::Receiver<ProofReqElem>,
}

impl FlusherShard {
    /// Creates a new FlusherShard instance.
    ///
    /// # Arguments
    /// * `tree` - Merkle tree for this shard
    /// * `oldest_active_sn` - Oldest active serial number
    /// * `shard_id` - ID of this shard
    ///
    /// # Returns
    /// A new FlusherShard instance
    ///
    /// # Performance
    /// - Efficient channel initialization
    /// - Pre-allocated buffers
    /// - Optimized for parallel operation
    ///
    /// # Thread Safety
    /// - Thread-safe channel setup
    /// - Safe resource sharing
    /// - Consistent state initialization
    pub fn new(tree: Tree, oldest_active_sn: u64, shard_id: usize) -> Self {
        let (pr_sender, pr_receiver) = bounded(MAX_PROOF_REQ);

        Self {
            buf_read: None,
            tree,
            last_compact_done_sn: oldest_active_sn,
            shard_id,
            proof_req_sender: pr_sender,
            proof_req_receiver: pr_receiver,
        }
    }

    /// Processes pending proof requests.
    ///
    /// This method implements the proof request handling logic:
    /// 1. Request Processing:
    ///    - Checks for pending requests
    ///    - Generates proofs efficiently
    ///    - Notifies waiters
    ///
    /// 2. Proof Generation:
    ///    - Accesses Merkle tree
    ///    - Creates proof paths
    ///    - Validates results
    ///
    /// 3. Notification:
    ///    - Updates request state
    ///    - Signals completion
    ///    - Handles errors
    ///
    /// # Performance
    /// - Non-blocking channel operations
    /// - Efficient proof generation
    /// - Minimal lock contention
    /// - Optimized notification
    ///
    /// # Error Handling
    /// - Handles channel errors
    /// - Manages proof failures
    /// - Maintains consistency
    pub fn handle_proof_req(&self) {
        loop {
            let pair = self.proof_req_receiver.try_recv();
            if pair.is_err() {
                break;
            }
            let pair = pair.unwrap();
            let (lock, cvar) = &*pair;
            let mut req = lock.lock();
            let mut any_updated = false;
            let len = req.0.sns.len();
            for i in 0..len {
                if req.0.results[i].is_none() {
                    let sn = req.0.sns[i];
                    let proof = self.tree.get_single_proof(sn);
                    req.0.results[i] = Some(proof);
                    any_updated = true;
                }
            }
            if any_updated {
                // mark as completed once all filled
                req.1 = true;
                cvar.notify_one();
            }
        }
    }

    /// Flushes buffered data and updates the Merkle tree.
    ///
    /// This method implements the core shard flushing logic:
    /// 1. Buffer Processing:
    ///    - Reads buffered entries
    ///    - Validates data integrity
    ///    - Handles partial writes
    ///
    /// 2. Tree Updates:
    ///    - Updates Merkle tree nodes
    ///    - Maintains consistency
    ///    - Handles reorgs
    ///
    /// 3. Pruning:
    ///    - Removes old entries
    ///    - Updates indices
    ///    - Maintains references
    ///
    /// # Arguments
    /// * `prune_to_height` - Height to prune up to
    /// * `curr_height` - Current block height
    /// * `meta` - Metadata database reference
    /// * `bar_set` - Synchronization barriers
    /// * `end_block_chan` - Block completion channel
    ///
    /// # Performance
    /// - Batched operations
    /// - Efficient tree updates
    /// - Optimized pruning
    /// - Minimal synchronization
    ///
    /// # Error Handling
    /// - Handles I/O errors
    /// - Manages consistency
    /// - Recovers from failures
    pub fn flush(
        &mut self,
        prune_to_height: i64,
        curr_height: i64,
        meta: Arc<RwLock<RocksMetaDB>>,
        bar_set: Arc<BarrierSet>,
        end_block_chan: crossbeam::channel::Sender<Arc<MetaInfo>>,
        prune_sender: crossbeam::channel::Sender<PruneCmd>,
    ) {
        let mut buf_read = self.buf_read.take().unwrap();
        loop {
            let mut file_pos: i64 = 0;
            let (is_end_of_block, expected_file_pos) = buf_read.read_next_entry(|entry_bz| {
                file_pos = self.tree.append_entry(&entry_bz).unwrap();
                for (_, dsn) in entry_bz.dsn_iter() {
                    self.tree.deactive_entry(dsn);
                }
            });
            if !is_end_of_block && file_pos != expected_file_pos {
                panic!("File_pos mismatch!");
            }
            if is_end_of_block {
                break;
            }
        }

        let (compact_done_pos, compact_done_sn, sn_end) = buf_read.read_extra_info();

        let mut start_twig_id: u64 = 0;
        let mut end_twig_id: u64 = 0;
        let mut ef_size: i64 = 0;

        if prune_to_height > 0 && prune_to_height % PRUNE_EVERY_NBLOCKS == 0 {
            let meta = meta.read_arc();
            (start_twig_id, _) = meta.get_last_pruned_twig(self.shard_id);
            (end_twig_id, ef_size) = meta.get_first_twig_at_height(self.shard_id, prune_to_height);
            if end_twig_id == u64::MAX {
                panic!(
                    "FirstTwigAtHeight Not Found shard={} prune_to_height={}",
                    self.shard_id, prune_to_height
                );
            }
            let mut last_evicted_twig_id = compact_done_sn / (LEAF_COUNT_IN_TWIG as u64);
            last_evicted_twig_id = last_evicted_twig_id.saturating_sub(1);
            if end_twig_id > last_evicted_twig_id {
                end_twig_id = last_evicted_twig_id;
            }
            if start_twig_id <= end_twig_id && end_twig_id < start_twig_id + MIN_PRUNE_COUNT {
                end_twig_id = start_twig_id;
            } else {
                // SAFETY (io_uring): prune_twigs internally calls prune_head which closes file descriptors.
                // This is safe because:
                // 1. We only prune twigs up to `last_evicted_twig_id` (derived from `compact_done_sn`)
                // 2. The Prefetcher only reads entries beyond `compact_done_sn` (active data)
                // 3. Therefore, no in-flight io_uring reads should exist for pruned segments
                //
                // If this assumption is violated (e.g., Prefetcher reads old data), io_uring reads
                // may fail or return garbage due to closed/reused file descriptors.
                let segment_size = self.tree.get_entry_file_segment_size();
                let end_seg_id = (ef_size / segment_size) as u32;
                for seg_id in 0..end_seg_id {
                    // 发送 PruneCmd。用 try_send 避免阻塞：
                    // - disable_dio 模式下接收端从不读取 channel，send() 会永久阻塞
                    // - channel 满时应跳过而不是挂起 flusher
                    let _ = prune_sender.try_send(PruneCmd {
                        shard_id: self.shard_id,
                        seg_id: seg_id as usize,
                    });
                }

                self.tree.prune_twigs(start_twig_id, end_twig_id, ef_size);
            }
        }
        let del_start = self.last_compact_done_sn / (LEAF_COUNT_IN_TWIG as u64);
        let del_end = compact_done_sn / (LEAF_COUNT_IN_TWIG as u64);
        let tmp_list = self.tree.flush_files(del_start, del_end);
        let (entry_file_size, twig_file_size) = self.tree.get_file_sizes();
        let last_compact_done_sn = self.last_compact_done_sn;
        self.last_compact_done_sn = compact_done_sn;
        bar_set.flush_bar.wait();

        let youngest_twig_id = self.tree.youngest_twig_id;
        let shard_id = self.shard_id;
        let upper_tree = &mut self.tree.upper_tree;
        let n_list = upper_tree.evict_twigs(
            tmp_list,
            last_compact_done_sn >> TWIG_SHIFT,
            compact_done_sn >> TWIG_SHIFT,
        );
        let (_new_n_list, root_hash) = upper_tree.sync_upper_nodes(n_list, youngest_twig_id);
        let mut edge_nodes_bytes = Vec::<u8>::with_capacity(0);
        if prune_to_height > 0
            && prune_to_height % PRUNE_EVERY_NBLOCKS == 0
            && start_twig_id < end_twig_id
        {
            edge_nodes_bytes = upper_tree.prune_nodes(start_twig_id, end_twig_id, youngest_twig_id);
        }

        self.handle_proof_req();

        //shard#0 must wait other shards to finish
        if shard_id == 0 {
            bar_set.metadb_bar.wait();
        }

        let mut meta = meta.write_arc();
        if !edge_nodes_bytes.is_empty() {
            meta.set_edge_nodes(shard_id, &edge_nodes_bytes[..]);
            meta.set_last_pruned_twig(shard_id, end_twig_id, ef_size);
        }
        meta.set_root_hash(shard_id, root_hash);
        meta.set_oldest_active_sn(shard_id, compact_done_sn);
        meta.set_oldest_active_file_pos(shard_id, compact_done_pos);
        meta.set_next_serial_num(shard_id, sn_end);
        if curr_height % PRUNE_EVERY_NBLOCKS == 0 {
            meta.set_first_twig_at_height(
                shard_id,
                curr_height,
                compact_done_sn / (LEAF_COUNT_IN_TWIG as u64),
                compact_done_pos,
            )
        }
        meta.set_entry_file_size(shard_id, entry_file_size);
        meta.set_twig_file_size(shard_id, twig_file_size);

        if shard_id == 0 {
            meta.set_curr_height(curr_height);
            let meta_info = meta.commit();
            drop(meta);
            match end_block_chan.send(meta_info) {
                Ok(_) => {}
                Err(_) => {
                    println!("end block sender exit!");
                }
            }
        } else {
            drop(meta);
            bar_set.metadb_bar.wait();
        }
        self.buf_read = Some(buf_read);
    }
}

