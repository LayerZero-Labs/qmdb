//! Hover block generator - generates DELETE/INSERT/UPDATE/READ blocks.
//!
//! Design:
//! - Shares ShuffleParam with TestGenV2 for consistent key mapping
//! - Generates changesets for specific operation types
//! - Supports both sequential stride and random key selection
//! - Returns affected keys for bitmap correctness checking
//!
//! Key ranges:
//! - Stable range: [0, entry_count_wo_del) - for UPDATE/READ
//! - Deletable range: [entry_count_wo_del, entry_count_wo_del * 2) - for DELETE/INSERT

use byteorder::{BigEndian, ByteOrder};
use qmdb::test_helper::RandSrc;
use qmdb_common::def::{OP_CREATE, OP_DELETE, OP_READ, OP_WRITE};
use qmdb_common::utils::changeset::ChangeSet;
use std::sync::Arc;

use super::shuffle_param::ShuffleParam;

/// Operations per changeset for hover blocks
pub const HOVER_OPS_PER_CSET: usize = 10;

/// Result of generating a hover block, including bitmap update info
pub struct HoverBlockResult {
    /// The generated subblocks
    pub subblocks: Vec<Arc<Vec<Vec<ChangeSet>>>>,
    /// Key numbers affected (for DELETE/INSERT bitmap updates)
    pub affected_keys: Vec<u64>,
}

/// Hover block generator
pub struct HoverGen {
    /// Shared shuffle parameters for key mapping
    sp: ShuffleParam,
    /// Number of changesets per task
    cset_in_task: usize,
    /// Number of tasks per block
    task_in_block: usize,
    /// Current round (used in value generation)
    cur_round: usize,
}

impl HoverGen {
    /// Create a new HoverGen with the given parameters
    pub fn new(sp: ShuffleParam, cset_in_task: usize, task_in_block: usize) -> Self {
        Self {
            sp,
            cset_in_task,
            task_in_block,
            cur_round: 0,
        }
    }

    /// Set the current round (affects value content)
    pub fn set_round(&mut self, round: usize) {
        self.cur_round = round;
    }

    /// Calculate ops per block for hover operations
    pub fn ops_per_block(&self) -> u64 {
        (self.task_in_block * self.cset_in_task * HOVER_OPS_PER_CSET) as u64
    }

    /// Generate key-value pair for a given number
    fn fill_kv(&self, num: u64, k: &mut [u8], v: &mut [u8]) -> [u8; 32] {
        BigEndian::write_u64(&mut k[12..20], num);
        let hash = qmdb_common::entryfile::entry::entry_key_hash(&k[12..20]);
        for i in 0..12 {
            k[i] = hash[i] ^ hash[12 + i];
        }
        let kh = qmdb_common::entryfile::entry::entry_key_hash(&k[..20]);
        BigEndian::write_u32(&mut v[..4], self.cur_round as u32);
        v[4..].copy_from_slice(&kh[4..]);
        kh
    }

    /// Generate a hover block with sequential stride pattern
    ///
    /// - `op_type`: OP_DELETE, OP_CREATE, OP_WRITE, or OP_READ
    /// - `base`: Base offset for key numbers
    /// - `range`: Size of the key range
    /// - `start_pos`: Starting position within range (modulo range)
    /// - `stride`: Step size between consecutive keys
    /// - `subblock_size`: Number of tasks per subblock
    ///
    /// Returns subblocks and affected key numbers (for DELETE/INSERT bitmap updates)
    pub fn gen_block_strided(
        &self,
        op_type: u8,
        base: u64,
        range: u64,
        start_pos: u64,
        stride: u64,
        subblock_size: usize,
    ) -> HoverBlockResult {
        let num_subblocks = (self.task_in_block + subblock_size - 1) / subblock_size;
        let mut subblocks = Vec::with_capacity(num_subblocks);
        let mut pos = start_pos % range;

        // Track affected keys for DELETE/INSERT operations
        let track_keys = op_type == OP_DELETE || op_type == OP_CREATE;
        let mut affected_keys = if track_keys {
            Vec::with_capacity(self.task_in_block * self.cset_in_task * HOVER_OPS_PER_CSET)
        } else {
            Vec::new()
        };

        for sb_idx in 0..num_subblocks {
            let sb_start = sb_idx * subblock_size;
            let sb_end = std::cmp::min(sb_start + subblock_size, self.task_in_block);
            let sb_count = sb_end - sb_start;

            let mut tasks = Vec::with_capacity(sb_count);
            for _ in 0..sb_count {
                let mut csets = Vec::with_capacity(self.cset_in_task);
                for cset_idx in 0..self.cset_in_task {
                    let first_cset = cset_idx == 0;
                    let (cset, new_pos, keys) = self.gen_cset_strided_with_keys(
                        op_type, base, range, pos, stride, first_cset, track_keys,
                    );
                    pos = new_pos;
                    if track_keys {
                        affected_keys.extend(keys);
                    }
                    csets.push(cset);
                }
                tasks.push(csets);
            }
            subblocks.push(Arc::new(tasks));
        }

        HoverBlockResult {
            subblocks,
            affected_keys,
        }
    }

    /// Generate a hover block with random key selection
    ///
    /// - `op_type`: OP_DELETE, OP_CREATE, OP_WRITE, or OP_READ
    /// - `base`: Base offset for key numbers  
    /// - `range`: Size of the key range
    /// - `randsrc`: Random source for key selection
    /// - `subblock_size`: Number of tasks per subblock
    ///
    /// Returns subblocks and affected key numbers (for DELETE/INSERT bitmap updates)
    pub fn gen_block_random(
        &self,
        op_type: u8,
        base: u64,
        range: u64,
        randsrc: &mut RandSrc,
        subblock_size: usize,
    ) -> HoverBlockResult {
        let num_subblocks = (self.task_in_block + subblock_size - 1) / subblock_size;
        let mut subblocks = Vec::with_capacity(num_subblocks);

        // Track affected keys for DELETE/INSERT operations
        let track_keys = op_type == OP_DELETE || op_type == OP_CREATE;
        let mut affected_keys = if track_keys {
            Vec::with_capacity(self.task_in_block * self.cset_in_task * HOVER_OPS_PER_CSET)
        } else {
            Vec::new()
        };

        for sb_idx in 0..num_subblocks {
            let sb_start = sb_idx * subblock_size;
            let sb_end = std::cmp::min(sb_start + subblock_size, self.task_in_block);
            let sb_count = sb_end - sb_start;

            let mut tasks = Vec::with_capacity(sb_count);
            for _ in 0..sb_count {
                let mut csets = Vec::with_capacity(self.cset_in_task);
                for cset_idx in 0..self.cset_in_task {
                    let first_cset = cset_idx == 0;
                    let (cset, keys) = self.gen_cset_random_with_keys(
                        op_type, base, range, randsrc, first_cset, track_keys,
                    );
                    if track_keys {
                        affected_keys.extend(keys);
                    }
                    csets.push(cset);
                }
                tasks.push(csets);
            }
            subblocks.push(Arc::new(tasks));
        }

        HoverBlockResult {
            subblocks,
            affected_keys,
        }
    }

    /// Generate a changeset with strided key selection, returning affected keys
    fn gen_cset_strided_with_keys(
        &self,
        op_type: u8,
        base: u64,
        range: u64,
        mut pos: u64,
        stride: u64,
        first_cset: bool,
        track_keys: bool,
    ) -> (ChangeSet, u64, Vec<u64>) {
        let mut cset = ChangeSet::new();
        let mut k = [0u8; 20];
        let mut v = [0u8; 32];
        let mut keys = if track_keys {
            Vec::with_capacity(HOVER_OPS_PER_CSET)
        } else {
            Vec::new()
        };

        for _op_idx in 0..HOVER_OPS_PER_CSET {
            let num = base + pos;
            let actual_op = if op_type == OP_READ && first_cset {
                OP_WRITE
            } else {
                op_type
            };

            let kh = self.fill_kv(num, &mut k[..], &mut v[..]);
            cset.add_op(actual_op, &kh, &k[..], &v[..], None);

            if track_keys {
                keys.push(num);
            }

            pos = (pos + stride) % range;
        }

        cset.sort();
        (cset, pos, keys)
    }

    /// Generate a changeset with random key selection, returning affected keys
    fn gen_cset_random_with_keys(
        &self,
        op_type: u8,
        base: u64,
        range: u64,
        randsrc: &mut RandSrc,
        first_cset: bool,
        track_keys: bool,
    ) -> (ChangeSet, Vec<u64>) {
        let mut cset = ChangeSet::new();
        let mut k = [0u8; 20];
        let mut v = [0u8; 32];
        let mut keys = if track_keys {
            Vec::with_capacity(HOVER_OPS_PER_CSET)
        } else {
            Vec::new()
        };

        let segment_size = range / HOVER_OPS_PER_CSET as u64;

        for op_idx in 0..HOVER_OPS_PER_CSET {
            let segment_start = (op_idx as u64) * segment_size;
            let rand_offset = randsrc.get_uint64() % segment_size;
            let num = base + segment_start + rand_offset;

            let actual_op = if op_type == OP_READ && first_cset {
                OP_WRITE
            } else {
                op_type
            };

            let kh = self.fill_kv(num, &mut k[..], &mut v[..]);
            cset.add_op(actual_op, &kh, &k[..], &v[..], None);

            if track_keys {
                keys.push(num);
            }
        }

        cset.sort();
        (cset, keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hover_gen_basic() {
        let sp = ShuffleParam::new(10_000_000, 9_000_000);
        let hover_gen = HoverGen::new(sp, 2, 100);

        assert_eq!(hover_gen.ops_per_block(), 2000); // 100 * 2 * 10
    }
}
