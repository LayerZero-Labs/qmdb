use std::ops::Deref;

use crate::beatree::{
    branch::node::{self, get_key, BRANCH_NODE_BODY_SIZE},
    ops::{
        bit_ops::separator_len,
        update::{
            branch_updater::{BaseBranch, BranchGauge},
            BRANCH_MERGE_THRESHOLD,
        },
    },
    Key, PageNumber,
};

// BranchOp used to create a new node starting off a possible base node.
//
// `Update` and `KeepChunk` refers only to compressed separator within the base node.
pub enum BranchOp {
    // Separator and its page number
    Insert(Key, PageNumber),
    // Contains the position at which the separator is saved in the base,
    // along with the updated page number
    Update(usize, PageNumber),
    // Contains a range of separators that will be transfered unchanged
    // in the new node from the base.
    //
    // These are always items which were prefix-compressed.
    KeepChunk(KeepChunk),
}

// KeepChunk represents a sequence of separators contained in a branch node.
// `start` and `end` represents the separators range where `end` is non inclusive.
// 0-Sized chunks are not allowed, thus `start > end` must always hold.
// `sum_separator_lengths` is the sum of the separator lengths of each separator
// represented by the chunk itself.
#[derive(Debug, Clone, Copy)]
pub struct KeepChunk {
    pub start: usize,
    pub end: usize,
    pub sum_separator_lengths: usize,
}

impl KeepChunk {
    pub fn len(&self) -> usize {
        self.end - self.start
    }
}

// Keeps track of all BranchOp that needs to be applied to the new branch node.
//
// It ensures that all operations that are extracted to build a branch node
// will respect the constraints on the usage of uncompressed separators.
pub struct BranchOpsTracker {
    ops: Vec<BranchOp>,
    // gauges total size of branch after ops applied.
    gauge: BranchGauge,
    // ensure that the gauge correctly reflects the ops
    valid_gauge: bool,
}

impl BranchOpsTracker {
    pub fn new() -> Self {
        Self {
            ops: vec![],
            gauge: BranchGauge::default(),
            valid_gauge: true,
        }
    }

    // Push a new BranchOp::Insert operation.
    pub fn push_insert(&mut self, key: Key, pn: PageNumber) {
        assert!(self.valid_gauge);
        let op = BranchOp::Insert(key, pn);
        self.gauge.ingest_branch_op(None, &op);
        self.ops.push(op);
    }

    // Push a new BranchOp::Update operation.
    pub fn push_update(&mut self, base: &BaseBranch, pos: usize, pn: PageNumber) {
        assert!(self.valid_gauge);
        let op = BranchOp::Update(pos, pn);
        self.gauge.ingest_branch_op(Some(base), &op);
        self.ops.push(op);

        // Replace with Insert if:
        // 1. Prefix compression is stopped.
        // 2. Update op is referring to an uncompressed separator.
        if base.node.prefix_compressed() as usize <= pos || self.gauge.prefix_compressed.is_some() {
            self.replace_with_insert(Some(base), self.ops.len() - 1);
        }
    }

    // Push a new BranchOp::KeepChunk operation.
    pub fn push_chunk(&mut self, base: &BaseBranch, start: usize, end: usize) {
        assert!(self.valid_gauge);

        let base_compressed_end = std::cmp::min(end, base.node.prefix_compressed() as usize);

        if start != base_compressed_end {
            let chunk = KeepChunk {
                start,
                end: base_compressed_end,
                sum_separator_lengths: node::uncompressed_separator_range_size(
                    base.node.prefix_len() as usize,
                    base.node.separator_range_len(start, base_compressed_end),
                    base_compressed_end - start,
                    separator_len(&base.key(start)),
                ),
            };

            let branch_op = BranchOp::KeepChunk(chunk);
            self.gauge.ingest_branch_op(Some(base), &branch_op);
            self.ops.push(branch_op);

            // Replace with Insert if prefix compression is stopped.
            if self.gauge.prefix_compressed.is_some() {
                self.replace_with_insert(Some(base), self.ops.len() - 1);
            }
        }

        // Every kept uncompressed separator becomes an Insert operation.
        for i in base_compressed_end..end {
            let (key, pn) = base.key_value(i);
            self.push_insert(key, pn);
        }
    }

    fn replace_with_insert(&mut self, base: Option<&BaseBranch>, op_index: usize) -> usize {
        match self.ops[op_index] {
            BranchOp::Insert(_, _) => 1,
            BranchOp::Update(pos, new_pn) => {
                // UNWRAP: `Update` op only exists when base is Some.
                self.ops[op_index] = BranchOp::Insert(base.unwrap().key(pos), new_pn);
                1
            }
            BranchOp::KeepChunk(chunk) => {
                self.ops.remove(op_index);

                for pos in (chunk.start..chunk.end).into_iter().rev() {
                    // UNWRAP: `KeepChunk` op only exists when base is Some.
                    let (key, pn) = base.unwrap().key_value(pos);
                    self.ops.insert(op_index, BranchOp::Insert(key, pn));
                }
                chunk.end - chunk.start
            }
        }
    }

    // Extract the first item within a `BranchOp::KeepChunk` operation into a `BranchOp::Insert`.
    fn extract_insert_from_keep_chunk(&mut self, base: &BaseBranch, index: usize) {
        let BranchOp::KeepChunk(chunk) = self.ops[index] else {
            panic!(
                "Attempted to extract `BranchOp::Insert` from non `BranchOp::KeepChunk` operation"
            );
        };

        let (key, pn) = base.key_value(chunk.start);
        let separator_len = separator_len(&key);

        if chunk.start == chunk.end - 1 {
            // 0-sized chunks are not allowed,
            // thus 1-Sized chunks become just an `BranchOp::Insert`.
            self.ops[index] = BranchOp::Insert(key, pn);
        } else {
            self.ops[index] = BranchOp::KeepChunk(KeepChunk {
                start: chunk.start + 1,
                end: chunk.end,
                sum_separator_lengths: chunk.sum_separator_lengths - separator_len,
            });
            self.ops.insert(index, BranchOp::Insert(key, pn));
        }
    }

    // Body size of a branch node that would be built with all available ops.
    pub fn body_size(&self) -> usize {
        assert!(self.valid_gauge);
        self.gauge.body_size()
    }

    // Extract all available operations alongside their gauge.
    pub fn extract_ops<'a>(&'a mut self) -> (BranchOps<'a>, BranchGauge) {
        assert!(self.valid_gauge);
        let end = self.ops.len();
        let gauge = std::mem::take(&mut self.gauge);
        (
            BranchOps {
                ops: &mut self.ops,
                end,
            },
            gauge,
        )
    }

    // Extract a series of operations that is able to construct a branch node
    // with the specified target size.
    //
    // If `stop_prefix_compression` has to be called, then the target becomes BRANCH_MERGE_THRESHOLD
    // to minimize the amount of uncompressed items inserted in the node.
    //
    // SAFETY: This function is expected to be called in a loop until None is returned.
    pub fn extract_ops_until<'a>(
        &'a mut self,
        base: Option<&BaseBranch>,
        mut target: usize,
    ) -> Option<(BranchOps<'a>, BranchGauge)> {
        let mut pos = 0;
        let mut gauge = BranchGauge::default();

        while pos < self.ops.len() && gauge.body_size() < target {
            match *&self.ops[pos] {
                BranchOp::Insert(key, _) => {
                    if gauge.body_size_after(key, separator_len(&key)) > BRANCH_NODE_BODY_SIZE {
                        // Rare case: body was artifically small due to long shared prefix.

                        // Change the target requirement to minimize the number of non
                        // compressed separators saved into one node.
                        target = BRANCH_MERGE_THRESHOLD;

                        if gauge.body_size() < BRANCH_MERGE_THRESHOLD {
                            // Start applying items without prefix compression. we assume items are less
                            // than half the body size, so the next item should apply cleanly.
                            gauge.stop_prefix_compression();
                        } else {
                            // The initial target was not reached, but BRANCH_MERGE_THRESHOLD was met. To avoid
                            // inserting compressed separators, let's build the node with the operations collected until now.
                            break;
                        }
                    }
                }
                BranchOp::Update(update_pos, _) => {
                    // UNWRAP: `Update` op only exist when base is Some.
                    let key = base.unwrap().key(update_pos);

                    if gauge.body_size_after(key, separator_len(&key)) > BRANCH_NODE_BODY_SIZE {
                        if gauge.body_size() < BRANCH_MERGE_THRESHOLD {
                            // Replace the Update op and repeat the loop
                            // to see if `stop_prefix_compression` is activated
                            self.replace_with_insert(base, pos);
                            continue;
                        } else {
                            // The initial target was not reached, but BRANCH_MERGE_THRESHOLD was met. To avoid
                            // inserting compressed separators, let's build the node with the operations collected until now.
                            target = BRANCH_MERGE_THRESHOLD;
                            break;
                        }
                    }
                }
                BranchOp::KeepChunk(chunk) => {
                    // UNWRAP: `KeepChunk` op only exists when base is Some.
                    let base = base.unwrap();

                    if gauge.body_size_after_chunk(base, &chunk) > target {
                        // Try to split the chunk to make it fit into the available space.
                        // `try_split_keep_chunk` works on the gauge thus it accounts for a possible
                        // stop of the prefix compression even if working on a KeepChunk operation
                        let left_n_items = self.try_split_keep_chunk(
                            base,
                            &gauge,
                            pos,
                            target,
                            BRANCH_NODE_BODY_SIZE,
                        );

                        if left_n_items == 0 {
                            // If no item from the chunk is capable of fitting,
                            // then extract the first element from the chunk and repeat the loop
                            // to see if `stop_prefix_compression` is activated
                            self.extract_insert_from_keep_chunk(base, pos);
                            continue;
                        }
                    }
                }
            };

            gauge.ingest_branch_op(base, &self.ops[pos]);
            let n_ops = if gauge.prefix_compressed.is_some() {
                // replace everything with Insert if the prefix compression was stopped
                self.replace_with_insert(base, pos)
            } else {
                1
            };
            pos += n_ops;
        }

        if gauge.body_size() >= target {
            self.valid_gauge = false;
            Some((
                BranchOps {
                    ops: &mut self.ops,
                    end: pos,
                },
                gauge,
            ))
        } else {
            self.valid_gauge = true;
            self.gauge = gauge;
            None
        }
    }

    // Replace `KeepChunk` and `Update` ops with `Insert` ops,
    // preparing for the base to be changed.
    pub fn prepare_merge_ops(&mut self, base: Option<&BaseBranch>) {
        assert!(self.valid_gauge);

        let mut i = 0;
        while i < self.ops.len() {
            let replaced_ops = self.replace_with_insert(base, i);
            i += replaced_ops;
        }
    }

    // Try to split `self.ops[index]`, which is expected to be KeepChunk,
    // into two halves, targeting a `target` size and and not exceeding a `limit`.
    //
    // `target` and `limit` are required to understand when to accept a split
    // with a final size smaller than the target. Constraining the split to always
    // be bigger than the target causes the update algorithm to frequently
    // fall into underfull to overfull scenarios.
    fn try_split_keep_chunk(
        &mut self,
        base: &BaseBranch,
        gauge: &BranchGauge,
        index: usize,
        target: usize,
        limit: usize,
    ) -> usize {
        let BranchOp::KeepChunk(chunk) = self.ops[index] else {
            panic!("Attempted to split non `BranchOp::KeepChunk` operation");
        };

        let mut left_chunk_n_items = 0;
        let mut left_chunk_sum_separator_lengths = 0;
        let mut gauge = gauge.clone();
        for i in chunk.start..chunk.end {
            left_chunk_n_items += 1;

            let key = get_key(&base.node, i);
            let separator_len = separator_len(&key);
            let body_size_after = gauge.body_size_after(key, separator_len);

            if body_size_after >= target {
                // if an item jumps from below the target to bigger then the limit, do not use it
                if body_size_after > limit {
                    left_chunk_n_items -= 1;
                } else {
                    gauge.ingest_key(key, separator_len);
                    left_chunk_sum_separator_lengths += separator_len;
                }
                break;
            }
            left_chunk_sum_separator_lengths += separator_len;
            gauge.ingest_key(key, separator_len);
        }

        // if none or all elements are taken then nothing needs to be changed
        if left_chunk_n_items != 0 && chunk.len() != left_chunk_n_items {
            let left_chunk = KeepChunk {
                start: chunk.start,
                end: chunk.start + left_chunk_n_items,
                sum_separator_lengths: left_chunk_sum_separator_lengths,
            };

            let right_chunk = KeepChunk {
                start: chunk.start + left_chunk_n_items,
                end: chunk.end,
                sum_separator_lengths: chunk.sum_separator_lengths
                    - left_chunk_sum_separator_lengths,
            };

            self.ops.insert(index, BranchOp::KeepChunk(left_chunk));
            self.ops[index + 1] = BranchOp::KeepChunk(right_chunk);
        }
        left_chunk_n_items
    }
}

// Simple wrapper over a series of BranchOp meant to be used as &[BranchOp],
// with the added feature of draining the slice from the initial container
// of the operations.
pub struct BranchOps<'a> {
    ops: &'a mut Vec<BranchOp>,
    end: usize,
}

impl<'a> Deref for BranchOps<'a> {
    type Target = [BranchOp];
    fn deref(&self) -> &Self::Target {
        &self.ops[..self.end]
    }
}

impl<'a> Drop for BranchOps<'a> {
    fn drop(&mut self) {
        self.ops.drain(..self.end);
    }
}

#[cfg(test)]
mod tests {
    use crate::beatree::{
        branch::node::{self, get_key, BRANCH_NODE_BODY_SIZE},
        ops::{
            bit_ops::separator_len,
            update::{
                branch_ops::{BranchOp, BranchOpsTracker, KeepChunk},
                branch_updater::{
                    tests::{make_branch, make_branch_with_body_size_target, prefixed_key},
                    BaseBranch, BranchGauge,
                },
                BRANCH_BULK_SPLIT_TARGET, BRANCH_BULK_SPLIT_THRESHOLD, BRANCH_MERGE_THRESHOLD,
            },
        },
        Key, PageNumber,
    };

    #[test]
    fn bulk_split() {
        let key = |i| prefixed_key(0x00, 16, i);

        let mut gauge = BranchGauge::default();
        let mut n_keys = 0;

        let mut ops_tracker = BranchOpsTracker::new();

        while gauge.body_size() < BRANCH_BULK_SPLIT_THRESHOLD {
            let key = key(n_keys);

            gauge.ingest_key(key, separator_len(&key));
            ops_tracker.push_insert(key, PageNumber(n_keys as u32));

            n_keys += 1;
        }

        let mut n_splits = 0;
        let mut n_items_in_splits = 0;
        while let Some((ops, _gauge)) =
            ops_tracker.extract_ops_until(None, BRANCH_BULK_SPLIT_TARGET)
        {
            n_splits += 1;
            n_items_in_splits += ops.len();
        }

        let expected_bulk_split = BRANCH_BULK_SPLIT_THRESHOLD / BRANCH_BULK_SPLIT_TARGET;

        // After ingesting all those ops, the bulk split is expected to be initiated.
        assert_eq!(n_splits, expected_bulk_split);
        // The gauge needs to contain all the ops not present in the previous split.
        assert_eq!(ops_tracker.gauge.n(), n_keys - n_items_in_splits);
        // Operations are expected to be drainded.
        assert_eq!(ops_tracker.ops.len(), n_keys - n_items_in_splits);
    }

    #[test]
    fn extract_ops_until_only_inserts() {
        let key = |i| prefixed_key(0x00, 16, i);
        let mut gauge = BranchGauge::default();
        let target = BRANCH_MERGE_THRESHOLD;
        let mut ops_tracker = BranchOpsTracker::new();

        // Collect BranchOp::Insert into updater.ops until the gauge associated
        // to the ops is just after the target.
        let mut rightsized = false;
        ops_tracker.ops = (0..)
            .map(|i| key(i))
            .take_while(|key| {
                let res = !rightsized;
                rightsized = gauge.body_size_after(*key, separator_len(key)) >= target;

                if res {
                    gauge.ingest_key(*key, separator_len(key));
                }
                res
            })
            .enumerate()
            .map(|(i, key)| BranchOp::Insert(key, PageNumber(i as u32)))
            .collect();
        let n_ops = ops_tracker.ops.len();

        // All ops are expected to be consumed without any modification.
        let Some((res_ops, res_gauge)) = ops_tracker.extract_ops_until(None, target) else {
            panic!()
        };

        assert_eq!(res_ops.len(), n_ops);
        assert_eq!(res_gauge.body_size(), gauge.body_size());
        drop(res_ops);
        assert!(ops_tracker.ops.is_empty());
    }

    #[test]
    fn extract_ops_until_only_updates() {
        let key = |i| prefixed_key(0x00, 16, i);

        let mut gauge = BranchGauge::default();
        let target = BRANCH_MERGE_THRESHOLD;

        let branch = make_branch_with_body_size_target(key, |size| size < BRANCH_NODE_BODY_SIZE);

        let mut ops_tracker = BranchOpsTracker::new();
        let base = Some(BaseBranch::new(branch.clone()));

        // Collect BranchOp::Update into updater.ops until the gauge associated
        // to the ops is just after the target.
        let mut rightsized = false;
        ops_tracker.ops = (0..)
            .map(|i| (i, get_key(&branch, i)))
            .take_while(|(_i, key)| {
                let res = !rightsized;
                rightsized = gauge.body_size_after(*key, separator_len(key)) >= target;

                if res {
                    gauge.ingest_key(*key, separator_len(key));
                }
                res
            })
            .map(|(i, _key)| BranchOp::Update(i, PageNumber(i as u32)))
            .collect();
        let n_ops = ops_tracker.ops.len();

        // All ops are expected to be consumed without any modification.
        let Some((res_ops, res_gauge)) = ops_tracker.extract_ops_until(base.as_ref(), target)
        else {
            panic!()
        };

        assert_eq!(res_ops.len(), n_ops);
        assert_eq!(res_gauge.body_size(), gauge.body_size());
        drop(res_ops);
        assert!(ops_tracker.ops.is_empty());
    }

    #[test]
    fn extract_ops_until_only_keeps() {
        let key = |i| prefixed_key(0x00, 16, i);

        let mut gauge = BranchGauge::default();
        let target = BRANCH_MERGE_THRESHOLD;
        let mut ops_tracker = BranchOpsTracker::new();

        let branch = make_branch_with_body_size_target(key, |size| size < BRANCH_NODE_BODY_SIZE);

        let base = BaseBranch::new(branch.clone());

        // Collect BranchOp::KeepChunk into updater.ops until the gauge associated
        // to the ops is just after the target.
        // The chunks are increasingly bigger, the first one covers 1 element, and each
        // of the following chunks covers one more element.
        let mut rightsized = false;
        let mut from = 0;
        ops_tracker.ops = (1usize..)
            .map(|to| {
                let start = from;
                let end = from + to;
                from += to;
                KeepChunk {
                    start,
                    end,
                    sum_separator_lengths: node::uncompressed_separator_range_size(
                        branch.prefix_len() as usize,
                        branch.separator_range_len(start, end),
                        end - start,
                        separator_len(&get_key(&branch, start)),
                    ),
                }
            })
            .take_while(|keep_chunk| {
                let res = !rightsized;
                rightsized = gauge.body_size_after_chunk(&base, &keep_chunk) >= target;
                if res {
                    gauge.ingest_chunk(&base, &keep_chunk);
                }
                res
            })
            .map(|keep_chunk| BranchOp::KeepChunk(keep_chunk))
            .collect();

        let n_ops = ops_tracker.ops.len();

        // Almost all ops are expected to be consumed.
        let Some((res_ops, res_gauge)) = ops_tracker.extract_ops_until(Some(&base), target) else {
            panic!()
        };

        // Last keep_chunk is expected to have been split.
        assert_eq!(res_ops.len(), n_ops);
        drop(res_ops);
        assert_eq!(ops_tracker.ops.len(), 1);
        assert!(res_gauge.body_size() > target);
    }

    #[test]
    fn extract_ops_until_decrease_target_no_stop_prefix_compression_with_inserts() {
        // BranchOp::Insert exceeds BRANCH_MERGE_THRESHOLD and a single
        // additional item bumps the size to exceed BRANCH_NODE_BODY_SIZE.
        // To avoid using stop prefix compression,
        // the target is decreased to BRANCH_MERGE_THRESHOLD.
        let compressed_key = |i| prefixed_key(0x00, 30, i);
        let uncompressed_key = |i| prefixed_key(0xFF, 30, i);
        let mut gauge = BranchGauge::default();
        let target = 3500;
        let mut ops_tracker = BranchOpsTracker::new();

        // Collect BranchOp::Insert into updater.ops until the gauge associated
        // to the ops is just after BRANCH_MERGE_THRESHOLD.
        let mut rightsized = false;
        ops_tracker.ops = (0..)
            .map(|i| compressed_key(i))
            .take_while(|key| {
                let res = !rightsized;
                rightsized =
                    gauge.body_size_after(*key, separator_len(key)) >= BRANCH_MERGE_THRESHOLD;

                if res {
                    gauge.ingest_key(*key, separator_len(key));
                }
                res
            })
            .enumerate()
            .map(|(i, key)| BranchOp::Insert(key, PageNumber(i as u32)))
            .collect();

        let n_compressed_insertions = ops_tracker.ops.len();
        let n_uncompressed_insertions = 5;

        ops_tracker.ops.extend(
            (0..n_uncompressed_insertions)
                .map(|i| BranchOp::Insert(uncompressed_key(i), PageNumber(i as u32))),
        );

        let Some((res_ops, res_gauge)) = ops_tracker.extract_ops_until(None, target) else {
            panic!()
        };

        assert_eq!(res_ops.len(), n_compressed_insertions);
        assert_eq!(res_gauge.body_size(), gauge.body_size());
        drop(res_ops);
        assert_eq!(ops_tracker.ops.len(), n_uncompressed_insertions);
    }

    #[test]
    fn extract_ops_until_decrease_target_no_stop_prefix_compression_with_updates() {
        // A single BranchOp::Update bumps the size from being already above
        // BRANCH_MERGE_THRESHOLD to exceeding the BRANCH_NODE_BODY_SIZE.
        // To avoid using stop prefix compression,
        // the target is decreased to BRANCH_MERGE_THRESHOLD.
        let compressed_key = |i| prefixed_key(0x00, 30, i);
        let uncompressed_key = |i| prefixed_key(0xFF, 30, i);
        let mut gauge = BranchGauge::default();
        let target = 3500;
        let mut ops_tracker = BranchOpsTracker::new();

        // Collect BranchOp::Insert into updater.ops until the gauge associated
        // to the ops is just after BRANCH_MERGE_THRESHOLD.
        let mut rightsized = false;
        ops_tracker.ops = (0..)
            .map(|i| compressed_key(i))
            .take_while(|key| {
                let res = !rightsized;
                rightsized =
                    gauge.body_size_after(*key, separator_len(key)) >= BRANCH_MERGE_THRESHOLD;

                if res {
                    gauge.ingest_key(*key, separator_len(key));
                }
                res
            })
            .enumerate()
            .map(|(i, key)| BranchOp::Insert(key, PageNumber(i as u32)))
            .collect();

        let n_compressed_insertions = ops_tracker.ops.len();

        let branch = make_branch_with_body_size_target(uncompressed_key, |size| {
            size < BRANCH_NODE_BODY_SIZE
        });
        let base = BaseBranch::new(branch.clone());

        ops_tracker.ops.push(BranchOp::Update(0, PageNumber(1)));

        let Some((res_ops, res_gauge)) = ops_tracker.extract_ops_until(Some(&base), target) else {
            panic!()
        };

        assert_eq!(res_ops.len(), n_compressed_insertions);
        assert_eq!(res_gauge.body_size(), gauge.body_size());
        assert!(res_gauge.body_size() < target);
        assert!(res_gauge.body_size() >= BRANCH_MERGE_THRESHOLD);
        drop(res_ops);
        assert_eq!(ops_tracker.ops.len(), 1);
    }

    #[test]
    fn extract_ops_until_stop_prefix_compression_on_updates() {
        let compressed_key = |i| prefixed_key(0x00, 30, i);
        let uncompressed_key = |i| prefixed_key(0xFF, 30, i);

        let mut gauge = BranchGauge::default();
        let mut ops_tracker = BranchOpsTracker::new();

        // Collect BranchOp::Insert into ops_tracker.ops until the gauge associated
        // to the ops is just after the BRANCH_MERGE_THRESHOLD / 2
        ops_tracker.ops = (0..)
            .map(|i| compressed_key(i))
            .take_while(|key| {
                if gauge.body_size_after(*key, separator_len(key)) >= BRANCH_MERGE_THRESHOLD / 2 {
                    false
                } else {
                    gauge.ingest_key(*key, separator_len(key));
                    true
                }
            })
            .enumerate()
            .map(|(i, key)| BranchOp::Insert(key, PageNumber(i as u32)))
            .collect();
        let n_insert_op = ops_tracker.ops.len();

        // Use a branch containing keys which do not share a prefix with the just ingested keys
        let branch = make_branch_with_body_size_target(uncompressed_key, |size| {
            size < BRANCH_NODE_BODY_SIZE
        });

        let base = BaseBranch::new(branch.clone());

        // Extends ops_tracker.ops with BranchOp::Update operations until the final expected gauge
        // reaches the target.
        gauge.stop_prefix_compression();
        let mut rightsized = false;
        let update_ops: Vec<BranchOp> = (0..)
            .map(|i| (i, get_key(&branch, i)))
            .take_while(|(_i, key)| {
                let res = !rightsized;
                rightsized =
                    gauge.body_size_after(*key, separator_len(key)) >= BRANCH_MERGE_THRESHOLD;

                if res {
                    gauge.ingest_key(*key, separator_len(key));
                }
                res
            })
            .map(|(i, _key)| BranchOp::Update(i, PageNumber(i as u32)))
            .collect();

        ops_tracker.ops.extend(update_ops);

        let Some((res_ops, res_gauge)) =
            ops_tracker.extract_ops_until(Some(&base), BRANCH_NODE_BODY_SIZE)
        else {
            panic!()
        };

        // Make sure that not only the update operation on which the stop_prefix_compression
        // requirement has been found has been updated to insert, but also all subsequent operations.
        for i in n_insert_op..res_ops.len() {
            assert!(matches!(
                res_ops[i],
                BranchOp::Insert(k, PageNumber(_)) if k == uncompressed_key(i - n_insert_op)
            ));
        }

        // The target has been downgraded to reduce the amount of uncompressed items
        assert!(res_gauge.body_size() > BRANCH_MERGE_THRESHOLD);
        assert!(res_gauge.body_size() < BRANCH_NODE_BODY_SIZE);
    }

    #[test]
    fn extract_ops_until_top_prefix_compression_on_keeps() {
        let compressed_key = |i| prefixed_key(0x00, 30, i);
        let uncompressed_key = |i| prefixed_key(0xFF, 30, i);

        let mut gauge = BranchGauge::default();
        let mut ops_tracker = BranchOpsTracker::new();

        // Collect BranchOp::Insert into updater.ops until the gauge associated
        // to the ops is just after the BRANCH_MERGE_THRESHOLD / 2
        ops_tracker.ops = (0..)
            .map(|i| compressed_key(i))
            .take_while(|key| {
                if gauge.body_size_after(*key, separator_len(key)) >= BRANCH_MERGE_THRESHOLD / 2 {
                    false
                } else {
                    gauge.ingest_key(*key, separator_len(key));
                    true
                }
            })
            .enumerate()
            .map(|(i, key)| BranchOp::Insert(key, PageNumber(i as u32)))
            .collect();
        let n_insert_op = ops_tracker.ops.len();

        // Use a branch containing keys which do not share a prefix with the just ingested keys.
        let branch = make_branch_with_body_size_target(uncompressed_key, |size| {
            size < BRANCH_NODE_BODY_SIZE
        });

        let base = BaseBranch::new(branch.clone());

        // Extends updater.ops with BranchOp::KeepChunk operations until the final expected gauge
        // reaches the target.
        // The chunks are increasingly bigger, the first one covers 7 elements, and each
        // of the following chunks covers one more element.
        gauge.stop_prefix_compression();
        let mut rightsized = false;
        let mut from = 0;
        let mut total_kept_itmes = 0;
        let keep_ops: Vec<BranchOp> = (7..)
            .map(|to| {
                let start = from;
                let end = from + to;
                from += to;
                KeepChunk {
                    start,
                    end,
                    sum_separator_lengths: node::uncompressed_separator_range_size(
                        branch.prefix_len() as usize,
                        branch.separator_range_len(start, end),
                        end - start,
                        separator_len(&get_key(&branch, start)),
                    ),
                }
            })
            .take_while(|keep_chunk| {
                let res = !rightsized;
                rightsized =
                    gauge.body_size_after_chunk(&base, &keep_chunk) >= BRANCH_MERGE_THRESHOLD;
                if res {
                    gauge.ingest_chunk(&base, &keep_chunk);
                    total_kept_itmes += keep_chunk.end - keep_chunk.start;
                }
                res
            })
            .map(|keep_chunk| BranchOp::KeepChunk(keep_chunk))
            .collect();

        ops_tracker.ops.extend(keep_ops);

        let Some((res_ops, res_gauge)) =
            ops_tracker.extract_ops_until(Some(&base), BRANCH_NODE_BODY_SIZE)
        else {
            panic!()
        };

        // Make sure that not only the KeepChunk operation on which the stop_prefix_compression
        // requirement has been found has been updated to insert, but also all subsequent operations.
        for i in n_insert_op..res_ops.len() {
            assert!(matches!(
                res_ops[i],
                BranchOp::Insert(k, PageNumber(_)) if k == uncompressed_key(i - n_insert_op)
            ));
        }

        // Ensure that the last keep operation has been split correctly.
        let expected_size_split_chunk = total_kept_itmes - (res_ops.len() - n_insert_op);
        drop(res_ops);
        let BranchOp::KeepChunk(split_keep_chunk) = &ops_tracker.ops[0] else {
            panic!()
        };
        let split_keep_chunk = split_keep_chunk.end - split_keep_chunk.start;
        assert_eq!(split_keep_chunk, expected_size_split_chunk);

        // The target has been downgraded to reduce the amount of uncompressed items.
        assert!(res_gauge.body_size() > BRANCH_MERGE_THRESHOLD);
        assert!(res_gauge.body_size() < BRANCH_NODE_BODY_SIZE);
    }

    #[test]
    fn push_after_stop_prefix_compression() {
        let key = |i| prefixed_key(0x00, 6, i);

        let mut ops_tracker = BranchOpsTracker::new();

        let branch = make_branch((2..10).map(|i| (key(i), i)).collect());

        let base = BaseBranch::new(branch.clone());

        ops_tracker.push_insert(key(0), PageNumber(0)); // insert
        ops_tracker.gauge.stop_prefix_compression();
        ops_tracker.push_insert(key(1), PageNumber(0)); // insert
        ops_tracker.push_update(&base, 0, PageNumber(1)); // update
        ops_tracker.push_chunk(&base, 1, 8); // keep_chunk

        // Make sure that all ops are Insert type.
        for op in ops_tracker.ops {
            assert!(matches!(op, BranchOp::Insert(..)));
        }
    }

    #[test]
    fn try_split_keep_chunk() {
        let key = |i| prefixed_key(0x00, 16, i);
        let keys: Vec<(Key, usize)> = (0..100).map(|i| (key(i), i)).collect();

        let mut base_node_gauge = BranchGauge::default();

        let mut sum_separator_lengths = 0;
        for (key, _) in keys.clone() {
            let len = separator_len(&key);
            base_node_gauge.ingest_key(key, len);
            sum_separator_lengths += len;
        }

        let branch = make_branch(keys.clone());
        let base = BaseBranch::new(branch);

        let chunk = KeepChunk {
            start: 0,
            end: keys.len(),
            sum_separator_lengths,
        };

        // Perform a standard split
        let gauge = BranchGauge::default();
        let target = base_node_gauge.body_size() / 3;
        let limit = base_node_gauge.body_size() / 2;
        let mut ops_tracker = BranchOpsTracker::new();
        ops_tracker.ops = vec![BranchOp::KeepChunk(chunk)];
        let left_split_len = ops_tracker.try_split_keep_chunk(&base, &gauge, 0, target, limit);

        assert_eq!(ops_tracker.ops.len(), 2);
        let chunk1 = match &ops_tracker.ops[0] {
            BranchOp::KeepChunk(c1) => c1,
            _ => panic!(),
        };
        let chunk2 = match &ops_tracker.ops[1] {
            BranchOp::KeepChunk(c2) => c2,
            _ => panic!(),
        };
        assert_eq!(chunk1.len(), left_split_len);
        assert_eq!(chunk1.len() + chunk2.len(), keys.len());
        assert!(gauge.body_size_after_chunk(&base, chunk1) > target);
        assert!(gauge.body_size_after_chunk(&base, chunk1) < limit);

        // Perform a split which is not able to reach the target
        let gauge = BranchGauge::default();
        let target = base_node_gauge.body_size() * 2;
        let limit = base_node_gauge.body_size() * 2;
        let mut ops_tracker = BranchOpsTracker::new();
        ops_tracker.ops = vec![BranchOp::KeepChunk(chunk)];
        ops_tracker.try_split_keep_chunk(&base, &gauge, 0, target, limit);

        assert_eq!(ops_tracker.ops.len(), 1);
        let chunk1 = match &ops_tracker.ops[0] {
            BranchOp::KeepChunk(c1) => c1,
            _ => panic!(),
        };
        assert_eq!(chunk1.len(), keys.len());
        assert!(gauge.body_size_after_chunk(&base, chunk1) < target);

        // Perform a split with a target too little,
        // but something smaller than the limit will still be split.
        let gauge = BranchGauge::default();
        let target = 1;
        let limit = BRANCH_NODE_BODY_SIZE;
        let mut ops_tracker = BranchOpsTracker::new();
        ops_tracker.ops = vec![BranchOp::KeepChunk(chunk)];
        ops_tracker.try_split_keep_chunk(&base, &gauge, 0, target, limit);

        assert_eq!(ops_tracker.ops.len(), 2);
        let chunk1 = match &ops_tracker.ops[0] {
            BranchOp::KeepChunk(c1) => c1,
            _ => panic!(),
        };
        assert_eq!(chunk1.len(), 1);
        assert!(gauge.body_size_after_chunk(&base, chunk1) > target);
        assert!(gauge.body_size_after_chunk(&base, chunk1) < limit);

        // Perform a split with a limit too little,
        // nothing will still be split.
        let gauge = BranchGauge::default();
        let target = 1;
        let limit = 1;
        let mut ops_tracker = BranchOpsTracker::new();
        ops_tracker.ops = vec![BranchOp::KeepChunk(chunk)];
        ops_tracker.try_split_keep_chunk(&base, &gauge, 0, target, limit);

        assert_eq!(ops_tracker.ops.len(), 1);
        let chunk1 = match &ops_tracker.ops[0] {
            BranchOp::KeepChunk(c1) => c1,
            _ => panic!(),
        };
        assert_eq!(chunk1.len(), keys.len());
    }

    #[test]
    fn replace_with_insert() {
        let key = |i| prefixed_key(0x00, 16, i);
        let n_keys = 100;
        let keys: Vec<(Key, usize)> = (1..1 + n_keys).map(|i| (key(i), i)).collect();

        let mut base_node_gauge = BranchGauge::default();

        let mut sum_separator_lengths = 0;
        for (key, _) in keys.clone() {
            let len = separator_len(&key);
            base_node_gauge.ingest_key(key, len);
            sum_separator_lengths += len;
        }

        let branch = make_branch(keys.clone());
        let base = BaseBranch::new(branch);

        let insert1 = BranchOp::Insert(key(0), PageNumber(1));
        let insert2 = BranchOp::Insert(key(n_keys + 2), PageNumber(2));
        let chunk = KeepChunk {
            start: 1,
            end: keys.len(),
            sum_separator_lengths,
        };

        let mut ops_tracker = BranchOpsTracker::new();
        ops_tracker.ops = vec![
            insert1,
            BranchOp::Update(0, PageNumber(3)),
            BranchOp::KeepChunk(chunk),
            insert2,
        ];

        // insert remains insert
        ops_tracker.replace_with_insert(None, 0);
        assert!(matches!(ops_tracker.ops[0], BranchOp::Insert(k,..) if k == key(0)));
        assert_eq!(ops_tracker.ops.len(), 4);

        // update becomes insert
        ops_tracker.replace_with_insert(Some(&base), 1);
        assert!(matches!(ops_tracker.ops[1], BranchOp::Insert(k,..) if k == key(1)));
        assert_eq!(ops_tracker.ops.len(), 4);

        // keep becomes multiple inserts
        ops_tracker.replace_with_insert(Some(&base), 2);
        for (i, op) in ops_tracker.ops[2..ops_tracker.ops.len() - 2]
            .iter()
            .enumerate()
        {
            assert!(matches!(op, BranchOp::Insert(k,..) if *k == key(i + 2)));
        }
        assert!(
            matches!(ops_tracker.ops[ops_tracker.ops.len() - 1], BranchOp::Insert(k,..) if k == key(n_keys + 2))
        );
        assert_eq!(ops_tracker.ops.len(), 2 + n_keys);
    }

    #[test]
    fn extract_insert_from_keep_chunk() {
        let key = |i| prefixed_key(0x00, 16, i);
        let n_keys = 100;
        let keys: Vec<(Key, usize)> = (0..n_keys).map(|i| (key(i), i)).collect();
        let sum_separator_lengths = keys.clone().iter().map(|(k, _)| separator_len(k)).sum();

        let branch = make_branch(keys.clone());
        let base = BaseBranch::new(branch);

        let chunk = KeepChunk {
            start: 0,
            end: keys.len(),
            sum_separator_lengths,
        };

        let mut ops_tracker = BranchOpsTracker::new();
        ops_tracker.ops = vec![BranchOp::KeepChunk(chunk)];

        ops_tracker.extract_insert_from_keep_chunk(&base, 0);
        assert_eq!(ops_tracker.ops.len(), 2);
        assert!(matches!(ops_tracker.ops[0], BranchOp::Insert(k,..) if k == key(0)));
        assert!(matches!(ops_tracker.ops[1], BranchOp::KeepChunk(c) if c.len() == n_keys - 1));

        // 0-sized chunks are not allowed.
        let chunk = KeepChunk {
            start: 0,
            end: 1,
            sum_separator_lengths,
        };
        ops_tracker.ops = vec![BranchOp::KeepChunk(chunk)];
        ops_tracker.extract_insert_from_keep_chunk(&base, 0);
        assert_eq!(ops_tracker.ops.len(), 1);
        assert!(matches!(ops_tracker.ops[0], BranchOp::Insert(k,..) if k == key(0)));
    }
}
