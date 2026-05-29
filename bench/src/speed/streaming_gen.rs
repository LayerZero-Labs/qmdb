//! Streaming test vector generation with multiple worker threads.
//!
//! Design:
//! - N worker threads, each generating complete population blocks
//! - Round-robin: Worker i generates blocks i, i+N, i+2N, ...
//! - Hover blocks are generated inline at configured intervals
//! - Results sent via channels, consumer reassembles in order
//!
//! This allows pipelining: test generation overlaps with benchmark execution.

use crossbeam::channel::{bounded, Receiver, Sender};
use qmdb::test_helper::RandSrc;
use qmdb_common::def::{OP_CREATE, OP_DELETE, OP_READ, OP_WRITE};
use qmdb_common::utils::changeset::ChangeSet;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use super::hover_gen::HoverGen;
use super::shuffle_param::ShuffleParam;
use super::test_gen::TestGenV2;

/// Type of block being generated
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockType {
    /// Database population (CREATE operations)
    Population,
    /// Hover delete operations
    HoverDelete,
    /// Hover insert (recreate) operations
    HoverInsert,
    /// Hover update operations
    HoverUpdate,
    /// Hover read operations
    HoverRead,
}

impl BlockType {
    pub fn is_hover(&self) -> bool {
        !matches!(self, BlockType::Population)
    }

    pub fn name(&self) -> &'static str {
        match self {
            BlockType::Population => "population",
            BlockType::HoverDelete => "deletes",
            BlockType::HoverInsert => "inserts",
            BlockType::HoverUpdate => "updates",
            BlockType::HoverRead => "reads",
        }
    }
}

/// A complete block with its index and type
pub struct IndexedBlock {
    /// Block index (for population blocks)
    pub block_idx: usize,
    /// Type of block
    pub block_type: BlockType,
    /// Subblocks for this block
    pub subblocks: Vec<Arc<Vec<Vec<ChangeSet>>>>,
    /// Affected key numbers (for DELETE/INSERT bitmap updates)
    pub affected_keys: Vec<u64>,
}

/// Hover configuration
#[derive(Clone, Debug, Default)]
pub struct HoverConfig {
    /// Insert hover blocks every N population blocks (0 = disabled)
    pub hover_interval: usize,
    /// Number of delete+insert block pairs
    pub hover_recreate_blocks: usize,
    /// Number of update+read block pairs
    pub hover_write_blocks: usize,
}

impl HoverConfig {
    pub fn is_enabled(&self) -> bool {
        self.hover_interval > 0
    }

    pub fn total_hover_blocks(&self) -> usize {
        (self.hover_recreate_blocks + self.hover_write_blocks) * 2
    }
}

/// Message from worker to consumer
enum WorkerMsg {
    Block(IndexedBlock),
    Done,
}

/// Stride constant for sequential hover operations
const PRIME1: u64 = 1299827;

/// Configuration for StreamingTestGen
#[derive(Clone)]
pub struct StreamingGenConfig {
    pub entry_count: u64,
    pub cset_in_task: usize,
    pub block_size: usize,
    pub subblock_size: usize,
    pub num_workers: usize,
    pub channel_capacity: usize,
    /// Maximum population blocks to generate (should match blk_in_round from TestGenV2)
    pub max_blocks: usize,
    /// Hover configuration
    pub hover: HoverConfig,
}

impl Default for StreamingGenConfig {
    fn default() -> Self {
        Self {
            entry_count: 10_000_000,
            cset_in_task: 2,
            block_size: 10_000,
            subblock_size: 1_000,
            num_workers: 8,
            channel_capacity: 32,
            max_blocks: 0,
            hover: HoverConfig::default(),
        }
    }
}

/// Streaming test generator with integrated hover support
pub struct StreamingTestGen {
    config: StreamingGenConfig,
    /// Receiver for population blocks from workers
    rx: Receiver<WorkerMsg>,
    /// Worker thread handles
    workers: Vec<JoinHandle<()>>,
    /// Total population blocks to generate (in Round 0)
    total_pop_blocks: usize,
    /// Current expected population block index
    current_pop_idx: usize,
    /// Buffer for out-of-order population blocks
    block_buffer: std::collections::HashMap<usize, IndexedBlock>,
    /// Number of workers that have finished
    workers_done: usize,
    /// Hover generator
    hover_gen: HoverGen,
    /// RandSrc for random hover operations
    hover_randsrc: RandSrc,
    /// Pending hover blocks queue
    hover_queue: std::collections::VecDeque<IndexedBlock>,
    /// Shuffle param (for calculating key ranges)
    entry_count_wo_del: u64,
    /// Population ops per block (for calculating cur_num/cur_delete_num)
    stable_ops_per_block: u64,
    delete_ops_per_block: u64,
}

impl StreamingTestGen {
    /// Create a new streaming test generator
    pub fn new(randsrc_filename: &str, config: StreamingGenConfig) -> Self {
        let total_pop_blocks = if config.max_blocks > 0 {
            config.max_blocks
        } else {
            config.entry_count as usize / config.block_size
        };

        let (tx, rx) = bounded::<WorkerMsg>(config.channel_capacity);

        // Create base TestGenV2 for workers
        let mut randsrc = RandSrc::new(randsrc_filename, "streaming-base");
        let base_test_gen = TestGenV2::new(
            &mut randsrc,
            config.entry_count,
            config.cset_in_task,
            config.block_size,
        );

        // Extract shuffle param for hover generator
        let sp = base_test_gen.sp.clone();
        let entry_count_wo_del = sp.entry_count_wo_del;
        let stable_ops_per_block =
            (config.block_size * config.cset_in_task * base_test_gen.wr_op_in_cset) as u64;
        let delete_ops_per_block =
            (config.block_size * config.cset_in_task * base_test_gen.delete_op_in_cset) as u64;

        // Create hover generator
        let hover_gen = HoverGen::new(sp, config.cset_in_task, config.block_size);
        let hover_randsrc = RandSrc::new(randsrc_filename, "streaming-hover");

        // Spawn worker threads
        let mut workers = Vec::with_capacity(config.num_workers);

        for worker_id in 0..config.num_workers {
            let tx = tx.clone();
            let config_clone = config.clone();
            let num_workers = config.num_workers;

            let mut test_gen = base_test_gen.clone();
            let tasks_per_block = config_clone.block_size;
            test_gen.skip_tasks((worker_id * tasks_per_block) as u64);

            let worker_total_blocks = total_pop_blocks;

            let handle = thread::Builder::new()
                .name(format!("streaming-gen-{}", worker_id))
                .spawn(move || {
                    Self::worker_loop(
                        worker_id,
                        num_workers,
                        worker_total_blocks,
                        config_clone,
                        test_gen,
                        tx,
                    );
                })
                .expect("failed to spawn worker");

            workers.push(handle);
        }

        drop(tx);

        Self {
            config,
            rx,
            workers,
            total_pop_blocks,
            current_pop_idx: 0,
            block_buffer: std::collections::HashMap::new(),
            workers_done: 0,
            hover_gen,
            hover_randsrc,
            hover_queue: std::collections::VecDeque::new(),
            entry_count_wo_del,
            stable_ops_per_block,
            delete_ops_per_block,
        }
    }

    /// Worker thread loop - generates population blocks only
    fn worker_loop(
        worker_id: usize,
        num_workers: usize,
        total_blocks: usize,
        config: StreamingGenConfig,
        mut test_gen: TestGenV2,
        tx: Sender<WorkerMsg>,
    ) {
        let subblock_size = config.subblock_size;
        let block_size = config.block_size;
        let num_subblocks = (block_size + subblock_size - 1) / subblock_size;
        let tasks_per_block = block_size;
        let mut block_idx = worker_id;

        while block_idx < total_blocks {
            let mut subblocks = Vec::with_capacity(num_subblocks);
            for i in 0..num_subblocks {
                let start = i * subblock_size;
                let end = (start + subblock_size).min(block_size);
                let count = end - start;
                subblocks.push(test_gen.gen_subblock(count));
            }

            let msg = WorkerMsg::Block(IndexedBlock {
                block_idx,
                block_type: BlockType::Population,
                subblocks,
                affected_keys: Vec::new(), // Population blocks don't need key tracking
            });

            if tx.send(msg).is_err() {
                break;
            }

            test_gen.skip_tasks(((num_workers - 1) * tasks_per_block) as u64);
            block_idx += num_workers;
        }

        let _ = tx.send(WorkerMsg::Done);
    }

    /// Generate hover blocks after a population block
    fn generate_hover_blocks(&mut self, pop_block_idx: usize) {
        let hover = &self.config.hover;
        if !hover.is_enabled() {
            return;
        }

        // Calculate current state
        let cur_stable_count = ((pop_block_idx + 1) as u64) * self.stable_ops_per_block;
        let cur_deletable_count = ((pop_block_idx + 1) as u64) * self.delete_ops_per_block;

        let stable_max = cur_stable_count.min(self.entry_count_wo_del);
        let deletable_base = self.entry_count_wo_del;
        let deletable_count = cur_deletable_count;

        let subblock_size = self.config.subblock_size;
        let op_in_blk = self.hover_gen.ops_per_block();

        // Generate hover blocks in order: delete, insert, update, read

        // 1. Delete blocks (strided)
        // Calculate stride to ensure unique keys within each block
        if deletable_count > 0
            && hover.hover_recreate_blocks > 0
            && op_in_blk * hover.hover_recreate_blocks as u64 <= deletable_count
        {
            let stride =
                (deletable_count / (op_in_blk * hover.hover_recreate_blocks as u64)) * PRIME1;

            for i in 0..hover.hover_recreate_blocks {
                let start_pos = (i as u64 * stride * op_in_blk) % deletable_count;
                let result = self.hover_gen.gen_block_strided(
                    OP_DELETE,
                    deletable_base,
                    deletable_count,
                    start_pos,
                    stride,
                    subblock_size,
                );
                self.hover_queue.push_back(IndexedBlock {
                    block_idx: pop_block_idx,
                    block_type: BlockType::HoverDelete,
                    subblocks: result.subblocks,
                    affected_keys: result.affected_keys,
                });
            }
        }

        // 2. Insert blocks (strided, same start positions as delete to recreate same keys)
        if deletable_count > 0
            && hover.hover_recreate_blocks > 0
            && op_in_blk * hover.hover_recreate_blocks as u64 <= deletable_count
        {
            let stride =
                (deletable_count / (op_in_blk * hover.hover_recreate_blocks as u64)) * PRIME1;

            for i in 0..hover.hover_recreate_blocks {
                let start_pos = (i as u64 * stride * op_in_blk) % deletable_count;
                let result = self.hover_gen.gen_block_strided(
                    OP_CREATE,
                    deletable_base,
                    deletable_count,
                    start_pos,
                    stride,
                    subblock_size,
                );
                self.hover_queue.push_back(IndexedBlock {
                    block_idx: pop_block_idx,
                    block_type: BlockType::HoverInsert,
                    subblocks: result.subblocks,
                    affected_keys: result.affected_keys,
                });
            }
        }

        // 3. Update blocks (random) - no key tracking needed
        if stable_max > 0 && hover.hover_write_blocks > 0 {
            for _ in 0..hover.hover_write_blocks {
                let result = self.hover_gen.gen_block_random(
                    OP_WRITE,
                    0,
                    stable_max,
                    &mut self.hover_randsrc,
                    subblock_size,
                );
                self.hover_queue.push_back(IndexedBlock {
                    block_idx: pop_block_idx,
                    block_type: BlockType::HoverUpdate,
                    subblocks: result.subblocks,
                    affected_keys: Vec::new(),
                });
            }
        }

        // 4. Read blocks (random) - no key tracking needed
        if stable_max > 0 && hover.hover_write_blocks > 0 {
            for _ in 0..hover.hover_write_blocks {
                let result = self.hover_gen.gen_block_random(
                    OP_READ,
                    0,
                    stable_max,
                    &mut self.hover_randsrc,
                    subblock_size,
                );
                self.hover_queue.push_back(IndexedBlock {
                    block_idx: pop_block_idx,
                    block_type: BlockType::HoverRead,
                    subblocks: result.subblocks,
                    affected_keys: Vec::new(),
                });
            }
        }
    }

    /// Check if hover should be generated after this population block
    fn should_generate_hover(&self, pop_block_idx: usize) -> bool {
        let interval = self.config.hover.hover_interval;
        if interval == 0 {
            return false;
        }
        // Generate hover at intervals, but not at the very last block
        (pop_block_idx + 1) % interval == 0 && pop_block_idx < self.total_pop_blocks - 1
    }

    /// Get the next block (population or hover), or None if generation is done
    pub fn next_block(&mut self) -> Option<IndexedBlock> {
        // First, check if there are pending hover blocks
        if let Some(hover_block) = self.hover_queue.pop_front() {
            return Some(hover_block);
        }

        // No hover blocks pending, get next population block
        if self.current_pop_idx >= self.total_pop_blocks {
            return None;
        }

        // Check if we already have the next block buffered
        if let Some(block) = self.block_buffer.remove(&self.current_pop_idx) {
            let pop_idx = self.current_pop_idx;
            self.current_pop_idx += 1;

            // Check if we need to generate hover blocks after this population block
            if self.should_generate_hover(pop_idx) {
                self.generate_hover_blocks(pop_idx);
            }

            return Some(block);
        }

        // Receive from channel until we get the block we need
        loop {
            match self.rx.recv() {
                Ok(WorkerMsg::Block(block)) => {
                    if block.block_idx == self.current_pop_idx {
                        let pop_idx = self.current_pop_idx;
                        self.current_pop_idx += 1;

                        if self.should_generate_hover(pop_idx) {
                            self.generate_hover_blocks(pop_idx);
                        }

                        return Some(block);
                    } else if block.block_idx > self.current_pop_idx {
                        self.block_buffer.insert(block.block_idx, block);
                    }
                }
                Ok(WorkerMsg::Done) => {
                    self.workers_done += 1;
                    if self.workers_done >= self.config.num_workers {
                        if let Some(block) = self.block_buffer.remove(&self.current_pop_idx) {
                            let pop_idx = self.current_pop_idx;
                            self.current_pop_idx += 1;
                            if self.should_generate_hover(pop_idx) {
                                self.generate_hover_blocks(pop_idx);
                            }
                            return Some(block);
                        }
                        return None;
                    }
                }
                Err(_) => {
                    if let Some(block) = self.block_buffer.remove(&self.current_pop_idx) {
                        let pop_idx = self.current_pop_idx;
                        self.current_pop_idx += 1;
                        if self.should_generate_hover(pop_idx) {
                            self.generate_hover_blocks(pop_idx);
                        }
                        return Some(block);
                    }
                    return None;
                }
            }
        }
    }

    /// Shutdown and wait for all workers
    pub fn shutdown(self) {
        while self.rx.try_recv().is_ok() {}
        drop(self.rx);
        for handle in self.workers {
            let _ = handle.join();
        }
    }

    /// Get config
    pub fn config(&self) -> &StreamingGenConfig {
        &self.config
    }

    /// Get total population blocks (Round 0)
    pub fn total_pop_blocks(&self) -> usize {
        self.total_pop_blocks
    }

    /// Get current population block index
    pub fn current_pop_block_idx(&self) -> usize {
        self.current_pop_idx
    }

    /// Get hover ops per block
    pub fn hover_ops_per_block(&self) -> u64 {
        self.hover_gen.ops_per_block()
    }
}
