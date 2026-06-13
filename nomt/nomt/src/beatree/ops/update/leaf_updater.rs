use std::sync::Arc;

use crate::beatree::{
    leaf::node::{self as leaf_node, LeafBuilder, LeafNode, LEAF_NODE_BODY_SIZE},
    ops::bit_ops::separate,
    Key,
};
use crate::io::PagePool;

use super::{LEAF_BULK_SPLIT_TARGET, LEAF_BULK_SPLIT_THRESHOLD, LEAF_MERGE_THRESHOLD};

pub struct BaseLeaf {
    node: Arc<LeafNode>,
    separator: Key,
    low: usize,
}

impl BaseLeaf {
    pub fn new(node: Arc<LeafNode>, separator: Key) -> Self {
        BaseLeaf {
            node,
            separator,
            low: 0,
        }
    }

    // Try to find the given key starting from `self.low` up to the end.
    // Returns None if `self.low` is already at the end of the node.
    // If there are available keys in the node, then it returns the index
    // of the specified key with the boolean set to true or the index containing
    // the first key bigger than the one specified and the boolean set to false.
    fn find_key(&mut self, key: &Key) -> Option<(bool, usize)> {
        if self.low == self.node.n() {
            return None;
        }

        // apply binary search only on a subset of all cell_pointers
        let interesed_cell_pointers = &self.node.cell_pointers()[self.low..];
        let res = interesed_cell_pointers.binary_search_by(|cell_pointer| {
            let k = leaf_node::extract_key(cell_pointer);
            k.cmp(&key)
        });

        // the returned position is relative to `cell_pointers[self.low..]`
        match res {
            // if the key is found, then we return its position in the base leaf,
            // updating `self.low` to be the item just after the found key
            Ok(pos) => {
                let absolute_pos = self.low + pos;
                self.low = absolute_pos + 1;
                return Some((true, absolute_pos));
            }
            // If the key is not present, then `self.low` is updated to point to
            // the first key bigger than the one we looked for
            Err(pos) => {
                self.low = self.low + pos;
                return Some((false, self.low));
            }
        }
    }

    fn key(&self, i: usize) -> Key {
        self.node.key(i)
    }

    fn key_cell(&self, i: usize) -> (Key, &[u8], bool) {
        let (value, overflow) = self.node.value(i);
        (self.node.key(i), value, overflow)
    }

    fn cell(&self, i: usize) -> (&[u8], bool) {
        self.node.value(i)
    }
}

#[derive(Debug, PartialEq)]
enum LeafOp {
    // Key, Value, Overflow
    Insert(Key, Vec<u8>, bool),
    // From, To, Values size
    KeepChunk(usize, usize, usize),
}

pub enum DigestResult {
    NeedsMerge(Key),
    Finished,
}

/// A callback which takes ownership of newly created leaves.
pub trait HandleNewLeaf {
    fn handle_new_leaf(
        &mut self,
        separator: Key,
        node: LeafNode,
        cutoff: Option<Key>,
    ) -> std::io::Result<()>;
}

pub struct LeafUpdater {
    // the 'base' node we are working from. does not exist if DB is empty.
    base: Option<BaseLeaf>,
    // the cutoff key, which determines if an operation is in-scope.
    // does not exist for the last leaf in the database.
    cutoff: Option<Key>,
    // a separator override. this is set as `Some` either as part of a bulk split or when the
    // leaf is having values merged in from some earlier node.
    separator_override: Option<Key>,
    ops: Vec<LeafOp>,
    // gauges total size of leaf after ops applied.
    // if bulk split is undergoing, this just stores the total size of the last leaf,
    // and the gauges for the previous leaves are stored in `bulk_split`.
    gauge: LeafGauge,
    page_pool: PagePool,
}

impl LeafUpdater {
    pub fn new(page_pool: PagePool, base: Option<BaseLeaf>, cutoff: Option<Key>) -> Self {
        LeafUpdater {
            base,
            cutoff,
            separator_override: None,
            ops: Vec::new(),
            gauge: LeafGauge::default(),
            page_pool,
        }
    }

    pub fn is_in_scope(&self, key: &Key) -> bool {
        self.cutoff.map_or(true, |k| *key < k)
    }

    pub fn reset_base(&mut self, base: Option<BaseLeaf>, cutoff: Option<Key>) {
        self.base = base;
        self.cutoff = cutoff;
    }

    pub fn remove_cutoff(&mut self) {
        self.cutoff = None;
    }

    /// Ingest a key/cell pair. Provide a callback which is called if this deletes an existing
    /// overflow cell.
    pub fn ingest(
        &mut self,
        key: Key,
        value_change: Option<Vec<u8>>,
        overflow: bool,
        with_deleted_overflow: impl FnMut(&[u8]),
    ) {
        self.keep_up_to(Some(&key), with_deleted_overflow);

        if let Some(value) = value_change {
            self.gauge.ingest(1, value.len());
            self.ops.push(LeafOp::Insert(key, value, overflow));
        }
    }

    // If `NeedsMerge` is returned, `ops` are prepopulated with the merged values and
    // separator_override is set.
    // If `Finished` is returned, `ops` is guaranteed empty and separator_override is empty.
    pub fn digest(&mut self, new_leaves: &mut impl HandleNewLeaf) -> std::io::Result<DigestResult> {
        // no cells are going to be deleted from this point onwards - this keeps everything.
        self.keep_up_to(None, |_| {});

        // note: if we need a merge, it'd be more efficient to attempt to combine it with the last
        // leaf of the bulk split first rather than pushing the ops onwards. probably irrelevant
        // in practice; bulk splits are rare.
        if self.gauge.body_size() > LEAF_BULK_SPLIT_THRESHOLD {
            self.try_build_leaves(new_leaves, LEAF_BULK_SPLIT_TARGET)?
        }

        // If the gauge is over LEAF_NODE_BODY_SIZE at least one node
        // respecting the half-full requirement will always be created.
        // There are cases where this will create two leaves.
        if self.gauge.body_size() > LEAF_NODE_BODY_SIZE {
            self.try_build_leaves(new_leaves, self.gauge.body_size() / 2)?
        }

        if self.gauge.body_size() == 0 {
            self.separator_override = None;

            Ok(DigestResult::Finished)
        } else if self.gauge.body_size() >= LEAF_MERGE_THRESHOLD || self.cutoff.is_none() {
            let node = self.build_leaf(&self.ops);
            let separator = self.separator();

            new_leaves.handle_new_leaf(separator, node, self.cutoff)?;

            self.ops.clear();
            self.gauge = LeafGauge::default();
            self.separator_override = None;
            Ok(DigestResult::Finished)
        } else {
            if self.separator_override.is_none() {
                // UNWRAP: if cutoff exists, then base must too.
                // Merge is only performed when not at the rightmost leaf. this is protected by the
                // check on self.cutoff above.
                self.separator_override = Some(self.base.as_ref().unwrap().separator);
            }

            self.prepare_merge_ops();

            // UNWRAP: protected above.
            Ok(DigestResult::NeedsMerge(self.cutoff.unwrap()))
        }
    }

    fn keep_up_to(&mut self, up_to: Option<&Key>, mut with_deleted_overflow: impl FnMut(&[u8])) {
        let Some(base) = self.base.as_mut() else {
            // empty db
            return;
        };

        let from = base.low;
        let (found, to) = match up_to {
            // Nothing more to do, the end has already been reached
            None if from == base.node.n() => return,
            // Jump directly to the end of the base node and update `base.low` accordingly
            None => {
                base.low = base.node.n();
                (false, base.node.n())
            }
            Some(up_to) => match base.find_key(up_to) {
                Some(res) => res,
                // already at the end
                None => return,
            },
        };

        if from == to {
            // nothing to keep
            return;
        }

        let values_size = base.node.values_size(from, to);
        self.ops.push(LeafOp::KeepChunk(from, to, values_size));
        self.gauge.ingest(to - from, values_size);

        if found {
            let (val, overflow) = base.cell(to);
            if overflow {
                with_deleted_overflow(val);
            }
        }
    }

    // Attempt to build as many leaves as possible with the specified body size target
    fn try_build_leaves(
        &mut self,
        new_leaves: &mut impl HandleNewLeaf,
        target: usize,
    ) -> std::io::Result<()> {
        let mut start = 0;
        while let Some(item_count) = self.consume_and_update_until(start, target) {
            let leaf_ops = &self.ops[start..][..item_count];

            let separator = if start == 0 {
                self.separator()
            } else {
                // UNWRAP: separator override is always set when more items follow after a split.
                self.separator_override.take().unwrap()
            };
            let new_node = self.build_leaf(leaf_ops);

            // set the separator override for the next
            if let Some(op) = self.ops.get(start + item_count) {
                let next = self.op_first_key(op);
                let last = new_node.key(new_node.n() - 1);
                self.separator_override = Some(separate(&last, &next));
            }

            new_leaves.handle_new_leaf(
                separator,
                new_node,
                self.separator_override.or(self.cutoff),
            )?;
            start += item_count;
        }

        self.ops.drain(..start);
        Ok(())
    }

    /// The separator of the next leaf that will be built.
    pub fn separator(&self) -> Key {
        // the first leaf always gets a separator of all 0.
        self.separator_override
            .or(self.base.as_ref().map(|b| b.separator))
            .unwrap_or([0u8; 32])
    }

    // Starting from the specified index `from` within `self.ops`, consume and possibly
    // change the operations themselves to achieve a sequence of operations that are able to
    // construct a Leaf node with the specified target size.
    //
    // If reaching the target is not possible, then the gauge reflecting the last operations
    // will be returned as an error.
    //
    // The only scenario where the returned operations are associated to a body_size
    // below the target is when there is an item which causes the size to jump
    // from below to target to overfull.
    //
    // Given the fact that the maximum value size is `MAX_LEAF_VALUE_SIZE`
    // the previous scenario will only create nodes in the following range of body_size:
    // `[LEAF_NODE_BODY_SIZE - MAX_LEAF_VALUE_SIZE .. LEAF_NODE_BODY_SIZE]`
    //
    // This means that the half-full requirement will always be respected
    // because `LEAF_NODE_BODY_SIZE - MAX_LEAF_VALUE_SIZE > LEAF_MERGE_THRESHOLD`.
    fn consume_and_update_until(&mut self, from: usize, target: usize) -> Option<usize> {
        assert!(target >= LEAF_MERGE_THRESHOLD);
        let mut pos = from;
        let mut gauge = LeafGauge::default();
        let mut from_below_target_to_overfull = false;

        while pos < self.ops.len() && gauge.body_size() < target {
            let (n_items, values_size) = match &self.ops[pos] {
                LeafOp::Insert(_, val, _) => {
                    if gauge.body_size_after(1, val.len()) > LEAF_NODE_BODY_SIZE {
                        from_below_target_to_overfull = true;
                        break;
                    }
                    (1, val.len())
                }
                LeafOp::KeepChunk(from, to, values_size) => {
                    if gauge.body_size_after(to - from, *values_size) > target {
                        // Try to split the chunk to make it fit into the available space.
                        // `try_split_keep_chunk` works on the gauge thus it accounts for a possible
                        // stop of the prefix compression even if working on a KeepChunk operation
                        //
                        // UNWRAP: `KeepChunk` op only exists when base is Some.
                        let (left_n_items, left_values_size) = try_split_keep_chunk(
                            self.base.as_ref().unwrap(),
                            &gauge,
                            &mut self.ops,
                            pos,
                            target,
                            LEAF_NODE_BODY_SIZE,
                        );

                        if left_n_items == 0 {
                            // If no item from the chunk is capable of fitting,
                            // then extract the first element from the chunk and repeat the loop
                            // to see if `stop_prefix_compression` is activated
                            self.extract_insert_from_keep_chunk(pos);
                            continue;
                        }
                        (left_n_items, left_values_size)
                    } else {
                        (to - from, *values_size)
                    }
                }
            };

            gauge.ingest(n_items, values_size);
            pos += 1;
        }

        // Use `pos - from` ops only if they create a node with a body size bigger than the target
        // or accept a size below the target only if an item causes the node to transition
        // from a body size below the target to overfull.
        if gauge.body_size() >= target || from_below_target_to_overfull {
            Some(pos - from)
        } else {
            self.gauge = gauge;
            None
        }
    }

    fn extract_insert_from_keep_chunk(&mut self, index: usize) {
        let LeafOp::KeepChunk(from, to, values_size) = self.ops[index] else {
            panic!("Attempted to extract `LeafOp::Insert` from non `LeafOp::KeepChunk` operation");
        };

        // UNWRAP: `KeepChunk` exists only if base is Some.
        let base = self.base.as_ref().unwrap();
        let key = base.node.key(from);
        let (value, overflow) = base.node.value(from);

        if from == to - 1 {
            // 0-sized chunks are not allowed,
            // thus 1-Sized chunks become just an `LeafOp::Insert`.
            self.ops[index] = LeafOp::Insert(key, value.to_vec(), overflow);
        } else {
            self.ops[index] = LeafOp::KeepChunk(from + 1, to, values_size - value.len());
            self.ops
                .insert(index, LeafOp::Insert(key, value.to_vec(), overflow));
        }
    }

    fn prepare_merge_ops(&mut self) {
        let Some(ref base) = self.base else { return };

        // then replace `KeepChunk` ops with pure key-value ops, preparing for the base to be changed.
        let mut new_insert = 0;
        for i in 0..self.ops.len() {
            match self.ops[new_insert + i] {
                LeafOp::Insert(..) => (),
                LeafOp::KeepChunk(from, to, ..) => {
                    self.ops.remove(new_insert + i);

                    for pos in (from..to).into_iter().rev() {
                        let (k, v, o) = base.key_cell(pos);
                        self.ops
                            .insert(new_insert + i, LeafOp::Insert(k, v.to_vec(), o));
                    }
                    new_insert += to - from - 1;
                }
            }
        }
    }

    fn op_first_key(&self, leaf_op: &LeafOp) -> Key {
        // UNWRAP: `KeepChunk` leaf ops only exist when base is `Some`.
        match leaf_op {
            LeafOp::Insert(k, _, _) => *k,
            LeafOp::KeepChunk(from, _, _) => self.base.as_ref().unwrap().key(*from),
        }
    }

    fn build_leaf(&self, ops: &[LeafOp]) -> LeafNode {
        let (n_values, total_value_size) = ops
            .iter()
            .map(|op| match op {
                LeafOp::Insert(_, v, _) => (1, v.len()),
                LeafOp::KeepChunk(from, to, values_size) => (to - from, *values_size),
            })
            .fold((0, 0), |(acc_n, acc_size), (n, size)| {
                (acc_n + n, acc_size + size)
            });

        let mut leaf_builder = LeafBuilder::new(&self.page_pool, n_values, total_value_size);

        for op in ops {
            match op {
                LeafOp::Insert(k, v, o) => {
                    leaf_builder.push_cell(*k, v, *o);
                }
                LeafOp::KeepChunk(from, to, _) => {
                    // UNWRAP: if the operation is a KeepChunk variant, then base must exist
                    leaf_builder.push_chunk(&self.base.as_ref().unwrap().node, *from, *to)
                }
            }
        }
        leaf_builder.finish()
    }
}

// Given a vector of `LeafOp`, try to split the `index` operation,
// which is expected to be KeepChunk, into two halves,
// targeting a `target` size and and not exceeding a `limit`.
//
// `target` and `limit` are required to understand when to accept a split
// with a final size smaller than the target. Constraining the split to always
// be bigger than the target causes the update algorithm to frequently
// fall into underfull to overfull scenarios.
fn try_split_keep_chunk(
    base: &BaseLeaf,
    gauge: &LeafGauge,
    ops: &mut Vec<LeafOp>,
    index: usize,
    target: usize,
    limit: usize,
) -> (usize, usize) {
    let LeafOp::KeepChunk(from, to, values_size) = ops[index] else {
        panic!("Attempted to split non `LeafOp::KeepChunk` operation");
    };

    let mut left_chunk_n_items = 0;
    let mut left_chunk_values_size = 0;
    for pos in from..to {
        let size = base.cell(pos).0.len();

        left_chunk_values_size += size;
        left_chunk_n_items += 1;

        let body_size_after = gauge.body_size_after(left_chunk_n_items, left_chunk_values_size);
        if body_size_after >= target {
            // if an item jumps from below the target to bigger then the limit, do not use it
            if body_size_after > limit {
                left_chunk_values_size -= size;
                left_chunk_n_items -= 1;
            }
            break;
        }
    }

    // there must be at least one element taken from the chunk,
    // and if all elements are taken then nothing needs to be changed
    if left_chunk_n_items != 0 && to - from != left_chunk_n_items {
        ops.insert(
            index,
            LeafOp::KeepChunk(from, from + left_chunk_n_items, left_chunk_values_size),
        );

        ops[index + 1] = LeafOp::KeepChunk(
            from + left_chunk_n_items,
            to,
            values_size - left_chunk_values_size,
        );
    }

    (left_chunk_n_items, left_chunk_values_size)
}

#[derive(Default)]
struct LeafGauge {
    n: usize,
    value_size_sum: usize,
}

impl LeafGauge {
    fn ingest(&mut self, n: usize, values_size: usize) {
        self.n += n;
        self.value_size_sum += values_size;
    }

    fn body_size_after(&self, n: usize, values_size: usize) -> usize {
        leaf_node::body_size(self.n + n, self.value_size_sum + values_size)
    }

    fn body_size(&self) -> usize {
        leaf_node::body_size(self.n, self.value_size_sum)
    }
}

#[cfg(test)]
mod tests {
    use crate::beatree::{
        branch::BRANCH_NODE_SIZE,
        leaf::node::{body_size, MAX_LEAF_VALUE_SIZE},
        ops::update::{leaf_updater::LeafGauge, LEAF_MERGE_THRESHOLD},
    };

    use super::{
        separate, BaseLeaf, DigestResult, HandleNewLeaf, Key, LeafBuilder, LeafNode, LeafOp,
        LeafUpdater, PagePool,
    };
    use std::{collections::HashMap, sync::Arc};

    lazy_static::lazy_static! {
        static ref PAGE_POOL: PagePool = PagePool::new();
    }

    #[derive(Default)]
    struct TestHandleNewLeaf {
        inner: HashMap<Key, (LeafNode, Option<Key>)>,
    }

    impl HandleNewLeaf for TestHandleNewLeaf {
        fn handle_new_leaf(
            &mut self,
            separator: Key,
            node: LeafNode,
            cutoff: Option<Key>,
        ) -> std::io::Result<()> {
            self.inner.insert(separator, (node, cutoff));
            Ok(())
        }
    }

    fn key(x: u8) -> Key {
        [x; 32]
    }

    fn make_leaf(vs: Vec<(Key, Vec<u8>, bool)>) -> Arc<LeafNode> {
        let n = vs.len();
        let total_value_size = vs.iter().map(|(_, v, _)| v.len()).sum();

        let mut builder = LeafBuilder::new(&PAGE_POOL, n, total_value_size);
        for (k, v, overflow) in vs {
            builder.push_cell(k, &v, overflow);
        }

        Arc::new(builder.finish())
    }

    #[test]
    fn leaf_binary_search() {
        let leaf = make_leaf(vec![
            (key(1), vec![1u8; 500], false),
            (key(3), vec![1u8; 500], false),
            (key(5), vec![1u8; 500], false),
            (key(7), vec![1u8; 500], false),
            (key(9), vec![1u8; 500], false),
        ]);

        let mut base = BaseLeaf {
            node: leaf,
            low: 0,
            separator: key(1),
        };

        assert_eq!(base.find_key(&key(0)), Some((false, 0)));
        assert_eq!(base.find_key(&key(1)), Some((true, 0)));
        assert_eq!(base.find_key(&key(2)), Some((false, 1)));
        assert_eq!(base.find_key(&key(3)), Some((true, 1)));
        assert_eq!(base.find_key(&key(4)), Some((false, 2)));
        assert_eq!(base.find_key(&key(5)), Some((true, 2)));
        assert_eq!(base.find_key(&key(6)), Some((false, 3)));
        assert_eq!(base.find_key(&key(7)), Some((true, 3)));
        assert_eq!(base.find_key(&key(8)), Some((false, 4)));
        assert_eq!(base.find_key(&key(9)), Some((true, 4)));
        assert_eq!(base.find_key(&key(10)), None);
    }

    #[test]
    fn is_in_scope() {
        let mut updater = LeafUpdater::new(PAGE_POOL.clone(), None, None);
        assert!(updater.is_in_scope(&key(0xff)));

        updater.reset_base(None, Some(key(0xfe)));
        assert!(updater.is_in_scope(&key(0xf0)));
        assert!(updater.is_in_scope(&key(0xfd)));
        assert!(!updater.is_in_scope(&key(0xfe)));
        assert!(!updater.is_in_scope(&key(0xff)));
    }

    #[test]
    fn update() {
        let leaf = make_leaf(vec![
            (key(1), vec![1u8; 1000], false),
            (key(2), vec![1u8; 1000], false),
            (key(3), vec![1u8; 1000], false),
        ]);

        let mut updater = LeafUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseLeaf {
                node: leaf,
                low: 0,
                separator: key(1),
            }),
            None,
        );
        let mut new_leaves = TestHandleNewLeaf::default();

        updater.ingest(key(2), Some(vec![2u8; 1000]), false, |_| {});
        let DigestResult::Finished = updater.digest(&mut new_leaves).unwrap() else {
            panic!()
        };

        let new_leaf_entry = new_leaves.inner.get(&key(1)).unwrap();

        let new_leaf = &new_leaf_entry.0;
        assert_eq!(new_leaf.n(), 3);
        assert_eq!(new_leaf.get(&key(1)).unwrap().0, &[1u8; 1000]);
        assert_eq!(new_leaf.get(&key(2)).unwrap().0, &[2u8; 1000]);
        assert_eq!(new_leaf.get(&key(3)).unwrap().0, &[1u8; 1000]);
    }

    #[test]
    fn insert_rightsized() {
        let leaf = make_leaf(vec![
            (key(1), vec![1u8; 900], false),
            (key(2), vec![1u8; 900], false),
            (key(3), vec![1u8; 900], false),
        ]);

        let mut updater = LeafUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseLeaf {
                node: leaf,
                low: 0,
                separator: key(1),
            }),
            None,
        );
        let mut new_leaves = TestHandleNewLeaf::default();

        updater.ingest(key(4), Some(vec![1u8; 900]), false, |_| {});
        let DigestResult::Finished = updater.digest(&mut new_leaves).unwrap() else {
            panic!()
        };

        let new_leaf_entry = new_leaves.inner.get(&key(1)).unwrap();

        let new_leaf = &new_leaf_entry.0;
        assert_eq!(new_leaf.n(), 4);
        assert_eq!(new_leaf.get(&key(1)).unwrap().0, &[1u8; 900]);
        assert_eq!(new_leaf.get(&key(2)).unwrap().0, &[1u8; 900]);
        assert_eq!(new_leaf.get(&key(3)).unwrap().0, &[1u8; 900]);
        assert_eq!(new_leaf.get(&key(4)).unwrap().0, &[1u8; 900]);
    }

    #[test]
    fn insert_overflowing() {
        let leaf = make_leaf(vec![
            (key(1), vec![1u8; 1200], false),
            (key(2), vec![1u8; 1200], false),
            (key(3), vec![1u8; 1200], false),
        ]);

        let mut updater = LeafUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseLeaf {
                node: leaf,
                low: 0,
                separator: key(1),
            }),
            None,
        );
        let mut new_leaves = TestHandleNewLeaf::default();

        updater.ingest(key(4), Some(vec![1u8; 1200]), false, |_| {});
        let DigestResult::Finished = updater.digest(&mut new_leaves).unwrap() else {
            panic!()
        };

        let new_leaf_entry_1 = new_leaves.inner.get(&key(1)).unwrap();
        let new_leaf_entry_2 = new_leaves.inner.get(&separate(&key(2), &key(3))).unwrap();

        let new_leaf_1 = &new_leaf_entry_1.0;
        let new_leaf_2 = &new_leaf_entry_2.0;

        assert_eq!(new_leaf_1.n(), 2);
        assert_eq!(new_leaf_2.n(), 2);

        assert_eq!(new_leaf_1.get(&key(1)).unwrap().0, &[1u8; 1200]);
        assert_eq!(new_leaf_1.get(&key(2)).unwrap().0, &[1u8; 1200]);
        assert_eq!(new_leaf_2.get(&key(3)).unwrap().0, &[1u8; 1200]);
        assert_eq!(new_leaf_2.get(&key(4)).unwrap().0, &[1u8; 1200]);
    }

    #[test]
    fn delete() {
        let leaf = make_leaf(vec![
            (key(1), vec![1u8; 1200], false),
            (key(2), vec![1u8; 1200], false),
            (key(3), vec![1u8; 1200], false),
        ]);

        let mut updater = LeafUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseLeaf {
                node: leaf,
                low: 0,
                separator: key(1),
            }),
            None,
        );
        let mut new_leaves = TestHandleNewLeaf::default();

        updater.ingest(key(2), None, false, |_| {});
        let DigestResult::Finished = updater.digest(&mut new_leaves).unwrap() else {
            panic!()
        };

        let new_leaf_entry = new_leaves.inner.get(&key(1)).unwrap();

        let new_leaf = &new_leaf_entry.0;
        assert_eq!(new_leaf.n(), 2);
        assert_eq!(new_leaf.get(&key(1)).unwrap().0, &[1u8; 1200]);
        assert_eq!(new_leaf.get(&key(3)).unwrap().0, &[1u8; 1200]);
    }

    #[test]
    fn delete_underflow_and_merge() {
        let leaf = make_leaf(vec![
            (key(1), vec![1u8; 800], false),
            (key(2), vec![1u8; 800], false),
            (key(3), vec![1u8; 800], false),
        ]);

        let leaf2 = make_leaf(vec![
            (key(4), vec![1u8; 1100], false),
            (key(5), vec![1u8; 1100], false),
        ]);

        let mut updater = LeafUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseLeaf {
                node: leaf,
                low: 0,
                separator: key(1),
            }),
            Some(key(4)),
        );
        let mut new_leaves = TestHandleNewLeaf::default();

        updater.ingest(key(2), None, false, |_| {});
        let DigestResult::NeedsMerge(merge_key) = updater.digest(&mut new_leaves).unwrap() else {
            panic!()
        };
        assert_eq!(merge_key, key(4));

        assert!(new_leaves.inner.get(&key(1)).is_none());

        updater.reset_base(
            Some(BaseLeaf {
                node: leaf2,
                low: 0,
                separator: key(4),
            }),
            None,
        );

        let DigestResult::Finished = updater.digest(&mut new_leaves).unwrap() else {
            panic!()
        };
        let new_leaf_entry = new_leaves.inner.get(&key(1)).unwrap();

        let new_leaf = &new_leaf_entry.0;
        assert_eq!(new_leaf.n(), 4);
        assert_eq!(new_leaf.get(&key(1)).unwrap().0, &[1u8; 800]);
        assert_eq!(new_leaf.get(&key(3)).unwrap().0, &[1u8; 800]);
        assert_eq!(new_leaf.get(&key(4)).unwrap().0, &[1u8; 1100]);
        assert_eq!(new_leaf.get(&key(5)).unwrap().0, &[1u8; 1100]);
    }

    #[test]
    fn delete_calls_with_deleted_overflow() {
        let leaf = make_leaf(vec![
            (key(1), vec![1u8; 1200], false),
            (key(2), vec![1u8; 1200], true),
            (key(3), vec![1u8; 1200], false),
        ]);

        let mut updater = LeafUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseLeaf {
                node: leaf,
                low: 0,
                separator: key(1),
            }),
            None,
        );
        let mut new_leaves = TestHandleNewLeaf::default();

        let mut called = false;
        updater.ingest(key(2), None, false, |_| called = true);
        assert!(called);
        let DigestResult::Finished = updater.digest(&mut new_leaves).unwrap() else {
            panic!()
        };
    }

    #[test]
    fn delete_completely() {
        let leaf = make_leaf(vec![
            (key(1), vec![1u8; 1200], false),
            (key(2), vec![1u8; 1200], false),
        ]);

        let mut updater = LeafUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseLeaf {
                node: leaf,
                low: 0,
                separator: key(1),
            }),
            None,
        );
        let mut new_leaves = TestHandleNewLeaf::default();

        updater.ingest(key(1), None, false, |_| {});
        updater.ingest(key(2), None, false, |_| {});
        let DigestResult::Finished = updater.digest(&mut new_leaves).unwrap() else {
            panic!()
        };

        assert!(new_leaves.inner.get(&key(1)).is_none());
    }

    #[test]
    fn delete_underflow_rightmost() {
        let leaf = make_leaf(vec![
            (key(1), vec![1u8; 1200], false),
            (key(2), vec![1u8; 1200], false),
        ]);

        let mut updater = LeafUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseLeaf {
                node: leaf,
                low: 0,
                separator: key(1),
            }),
            None,
        );
        let mut new_leaves = TestHandleNewLeaf::default();

        updater.ingest(key(1), None, false, |_| {});
        let DigestResult::Finished = updater.digest(&mut new_leaves).unwrap() else {
            panic!()
        };

        let new_leaf_entry = new_leaves.inner.get(&key(1)).unwrap();
        let new_leaf = &new_leaf_entry.0;
        assert_eq!(new_leaf.n(), 1);
        assert_eq!(new_leaf.get(&key(2)).unwrap().0, &[1u8; 1200]);
    }

    #[test]
    fn split_with_underflow() {
        let leaf = make_leaf(vec![
            (key(1), vec![1u8; 1800], false),
            (key(2), vec![1u8; 1800], false),
            (key(3), vec![1u8; 300], false),
        ]);

        let mut updater = LeafUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseLeaf {
                node: leaf,
                low: 0,
                separator: key(1),
            }),
            Some(key(5)),
        );
        let mut new_leaves = TestHandleNewLeaf::default();

        updater.ingest(key(4), Some(vec![1; 300]), false, |_| {});
        let DigestResult::NeedsMerge(merge_key) = updater.digest(&mut new_leaves).unwrap() else {
            panic!()
        };
        assert_eq!(merge_key, key(5));

        let new_leaf_entry = new_leaves.inner.get(&key(1)).unwrap();
        let new_leaf = &new_leaf_entry.0;
        assert_eq!(new_leaf.n(), 2);
        assert_eq!(new_leaf.get(&key(1)).unwrap().0, &[1u8; 1800]);
        assert_eq!(new_leaf.get(&key(2)).unwrap().0, &[1u8; 1800]);

        assert_eq!(updater.separator_override, Some(separate(&key(2), &key(3))));
        assert_eq!(
            updater.ops,
            vec![
                LeafOp::Insert(key(3), vec![1u8; 300], false),
                LeafOp::Insert(key(4), vec![1u8; 300], false),
            ]
        );
    }

    #[test]
    fn split_left_node_below_target() {
        let mut updater = LeafUpdater::new(PAGE_POOL.clone(), None, None);
        let mut new_leaves = TestHandleNewLeaf::default();

        updater.ingest(key(1), Some(vec![1; 1100]), false, |_| {});
        updater.ingest(key(2), Some(vec![1; 1100]), false, |_| {});
        updater.ingest(key(3), Some(vec![1; 1000]), false, |_| {});
        updater.ingest(key(4), Some(vec![1; 1000]), false, |_| {});
        updater.ingest(key(5), Some(vec![1; 1000]), false, |_| {});
        updater.ingest(key(6), Some(vec![1; 1300]), false, |_| {});

        let tot_body_size = updater.gauge.body_size();
        let midpoint = tot_body_size / 2;

        updater.try_build_leaves(&mut new_leaves, midpoint).unwrap();

        let leaf_1 = &new_leaves.inner.get(&[0; 32]).unwrap().0;
        assert_eq!(leaf_1.n(), 3);
        let leaf_body_size = body_size(leaf_1.n(), leaf_1.values_size(0, leaf_1.n()));
        assert!(leaf_body_size < midpoint);
        let leaf_2 = &new_leaves.inner.get(&separate(&key(3), &key(4))).unwrap().0;
        assert_eq!(leaf_2.n(), 3);
    }

    #[test]
    fn split_left_node_always_rightsized() {
        let mut updater = LeafUpdater::new(PAGE_POOL.clone(), None, None);
        let mut new_leaves = TestHandleNewLeaf::default();

        updater.ingest(key(1), Some(vec![1; 1300]), false, |_| {});
        updater.ingest(key(2), Some(vec![1; 650]), false, |_| {});
        updater.ingest(key(3), Some(vec![1; MAX_LEAF_VALUE_SIZE]), false, |_| {});
        updater.ingest(key(4), Some(vec![1; 678]), false, |_| {});

        // The first two sum up to a body_size of 2018 which is less than
        // LEAF_MERGE_THRESHOLD but even followd by the biggest leaf value possible
        // it's not possible to end up in an  to overfull scenario on the first
        // created leaf.

        updater
            .try_build_leaves(&mut new_leaves, updater.gauge.body_size() / 2)
            .unwrap();
        assert_eq!(new_leaves.inner.len(), 1);
    }

    #[test]
    fn split_right_node_rightsized_but_below_target() {
        let mut updater = LeafUpdater::new(PAGE_POOL.clone(), None, None);
        let mut new_leaves = TestHandleNewLeaf::default();

        updater.ingest(key(1), Some(vec![1; 1300]), false, |_| {});
        updater.ingest(key(2), Some(vec![1; 1100]), false, |_| {});
        updater.ingest(key(3), Some(vec![1; 600]), false, |_| {});
        updater.ingest(key(4), Some(vec![1; 1000]), false, |_| {});
        updater.ingest(key(5), Some(vec![1; 1000]), false, |_| {});

        let tot_body_size = updater.gauge.body_size();
        let midpoint = tot_body_size / 2;

        updater.try_build_leaves(&mut new_leaves, midpoint).unwrap();

        // A leaf is perfectly created.
        let leaf_1 = &new_leaves.inner.get(&[0; 32]).unwrap().0;
        assert_eq!(leaf_1.n(), 3);
        let leaf_body_size = body_size(3, leaf_1.values_size(0, 3));
        assert!(leaf_body_size > midpoint);

        // There is no second created leaf because the remaining ops
        // do not exceed the target.
        assert_eq!(new_leaves.inner.len(), 1);
        assert_eq!(updater.ops.len(), 2);
        // The last ops still represents a valid constructed leaf node.
        assert!(updater.gauge.body_size() < midpoint);
        assert!(updater.gauge.body_size() > LEAF_MERGE_THRESHOLD);
    }

    #[test]
    fn consume_and_update_until_only_inserts() {
        let mut updater = LeafUpdater::new(PAGE_POOL.clone(), None, None);

        updater.ingest(key(1), Some(vec![1; 1000]), false, |_| {});
        updater.ingest(key(2), Some(vec![1; 1000]), false, |_| {});
        updater.ingest(key(3), Some(vec![1; 500]), false, |_| {});
        updater.ingest(key(4), Some(vec![1; 1000]), false, |_| {});

        assert_eq!(updater.consume_and_update_until(0, 2200), Some(3));

        updater.ops.clear();
        updater.gauge = LeafGauge::default();

        updater.ingest(key(1), Some(vec![1; 1100]), false, |_| {});
        updater.ingest(key(2), Some(vec![1; 1100]), false, |_| {});
        updater.ingest(key(3), Some(vec![1; 1000]), false, |_| {});
        updater.ingest(key(4), Some(vec![1; 1000]), false, |_| {});
        updater.ingest(key(5), Some(vec![1; 1000]), false, |_| {});
        updater.ingest(key(6), Some(vec![1; 1300]), false, |_| {});

        // below target
        assert_eq!(updater.consume_and_update_until(0, 3250), Some(3));
    }

    #[test]
    fn consume_and_update_until_only_keeps() {
        let leaf = make_leaf(vec![
            (key(1), vec![1u8; 1000], false),
            (key(2), vec![1u8; 1000], false),
            (key(3), vec![1u8; 500], false),
            (key(4), vec![1u8; 1000], false),
        ]);

        let mut updater = LeafUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseLeaf {
                node: leaf.clone(),
                low: 0,
                separator: key(1),
            }),
            None,
        );
        updater.ops = vec![
            LeafOp::KeepChunk(0, 1, leaf.values_size(0, 1)),
            LeafOp::KeepChunk(1, 2, leaf.values_size(1, 2)),
            LeafOp::KeepChunk(2, 4, leaf.values_size(2, 4)),
        ];

        // one split exptected
        assert_eq!(updater.consume_and_update_until(0, 2200), Some(3));
        assert!(matches!(updater.ops[0], LeafOp::KeepChunk(0, 1, _)));
        assert!(matches!(updater.ops[1], LeafOp::KeepChunk(1, 2, _)));
        assert!(matches!(updater.ops[2], LeafOp::KeepChunk(2, 3, _)));
        assert!(matches!(updater.ops[3], LeafOp::KeepChunk(3, 4, _)));

        // below target
        let leaf = make_leaf(vec![
            (key(3), vec![1u8; 1000], false),
            (key(4), vec![1u8; 1100], false),
        ]);

        let mut updater = LeafUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseLeaf {
                node: leaf.clone(),
                low: 0,
                separator: key(1),
            }),
            None,
        );
        updater.ops = vec![
            LeafOp::Insert(key(1), vec![1; 1100], false),
            LeafOp::Insert(key(2), vec![1; 1100], false),
            LeafOp::KeepChunk(0, 2, leaf.values_size(0, 2)),
            LeafOp::Insert(key(5), vec![1; 900], false),
            LeafOp::Insert(key(6), vec![1; 1300], false),
        ];

        // one split exptected
        assert_eq!(updater.consume_and_update_until(0, 3250), Some(3));
        assert!(matches!(updater.ops[0], LeafOp::Insert(_, _, _)));
        assert!(matches!(updater.ops[1], LeafOp::Insert(_, _, _)));
        assert!(matches!(updater.ops[2], LeafOp::KeepChunk(0, 1, _)));
        assert!(matches!(updater.ops[3], LeafOp::KeepChunk(1, 2, _)));
        assert!(matches!(updater.ops[4], LeafOp::Insert(_, _, _)));
        assert!(matches!(updater.ops[5], LeafOp::Insert(_, _, _)));
    }

    #[test]
    fn try_split_keep_chunk() {
        let leaf = make_leaf(vec![
            (key(1), vec![1u8; 1000], false),
            (key(2), vec![1u8; 1000], false),
            (key(3), vec![1u8; 500], false),
            (key(4), vec![1u8; 1000], false),
        ]);

        let base = BaseLeaf {
            node: leaf.clone(),
            separator: [0; 32],
            low: 0,
        };

        // standard split
        let mut ops = vec![LeafOp::KeepChunk(0, 4, leaf.values_size(0, 4))];
        super::try_split_keep_chunk(
            &base,
            &LeafGauge::default(),
            &mut ops,
            0,
            2200,
            BRANCH_NODE_SIZE,
        );
        assert_eq!(ops.len(), 2);
        assert!(matches!(ops[0], LeafOp::KeepChunk(_,_ , size) if size > 2200));

        // Perform a split which is not able to reach the target
        let mut ops = vec![LeafOp::KeepChunk(0, 2, leaf.values_size(0, 2))];
        super::try_split_keep_chunk(
            &base,
            &LeafGauge::default(),
            &mut ops,
            0,
            2500,
            BRANCH_NODE_SIZE,
        );
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], LeafOp::KeepChunk(_,_ , size) if size < 2500));

        // Perform a split with a target too little,
        // but something smaller than the limit will still be split.
        let mut ops = vec![LeafOp::KeepChunk(0, 2, leaf.values_size(0, 2))];
        super::try_split_keep_chunk(
            &base,
            &LeafGauge::default(),
            &mut ops,
            0,
            500,
            BRANCH_NODE_SIZE,
        );
        assert_eq!(ops.len(), 2);
        assert!(matches!(ops[0], LeafOp::KeepChunk(_,_ , size) if size > 500));

        // Perform a split with a limit too little,
        // nothing will still be split.
        let mut ops = vec![LeafOp::KeepChunk(0, 2, leaf.values_size(0, 2))];
        super::try_split_keep_chunk(&base, &LeafGauge::default(), &mut ops, 0, 500, 500);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], LeafOp::KeepChunk(0,2 , size) if size == leaf.values_size(0, 2)));
    }

    #[test]
    fn prepare_merge_ops() {
        let leaf = make_leaf(vec![
            (key(3), vec![1u8; 500], false),
            (key(4), vec![1u8; 500], false),
            (key(5), vec![1u8; 500], false),
            (key(6), vec![1u8; 500], false),
        ]);

        let mut updater = LeafUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseLeaf {
                node: leaf.clone(),
                low: 0,
                separator: key(1),
            }),
            None,
        );
        updater.ops = vec![
            LeafOp::Insert(key(1), vec![1; 1100], false),
            LeafOp::Insert(key(2), vec![1; 1100], false),
            LeafOp::KeepChunk(0, 2, leaf.values_size(0, 2)),
            LeafOp::KeepChunk(2, 4, leaf.values_size(2, 4)),
            LeafOp::Insert(key(5), vec![1; 900], false),
            LeafOp::Insert(key(6), vec![1; 1300], false),
        ];

        updater.prepare_merge_ops();
        for op in updater.ops {
            assert!(matches!(op, LeafOp::Insert(_, _, _)));
        }
    }

    #[test]
    fn extract_insert_from_keep_chunk() {
        let leaf = make_leaf(vec![
            (key(3), vec![1u8; 500], false),
            (key(4), vec![1u8; 500], false),
        ]);

        let mut updater = LeafUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseLeaf {
                node: leaf.clone(),
                low: 0,
                separator: key(1),
            }),
            None,
        );
        updater.ops = vec![
            LeafOp::Insert(key(2), vec![1; 1100], false),
            LeafOp::KeepChunk(0, 2, leaf.values_size(0, 2)),
            LeafOp::Insert(key(5), vec![1; 1100], false),
        ];

        updater.extract_insert_from_keep_chunk(1);
        assert_eq!(updater.ops.len(), 4);
        assert!(matches!(updater.ops[1], LeafOp::Insert(_, _, _)));
        assert!(matches!(updater.ops[2], LeafOp::KeepChunk(1, 2, _)));
        // 0-sized junk cannot exists
        updater.extract_insert_from_keep_chunk(2);
        assert_eq!(updater.ops.len(), 4);
        assert!(matches!(updater.ops[2], LeafOp::Insert(_, _, _)));
    }
}
