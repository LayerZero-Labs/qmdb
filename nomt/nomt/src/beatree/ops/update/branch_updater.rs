use std::{ops::Range, sync::Arc};

use crate::beatree::{
    allocator::PageNumber,
    branch::{self as branch_node, node, BranchNode, BranchNodeBuilder, BRANCH_NODE_BODY_SIZE},
    ops::{
        bit_ops::{prefix_len, separator_len},
        find_key_pos,
    },
    Key,
};
use crate::io::PagePool;

use super::{
    branch_ops::{BranchOp, BranchOpsTracker, KeepChunk},
    get_key, BRANCH_BULK_SPLIT_TARGET, BRANCH_BULK_SPLIT_THRESHOLD, BRANCH_MERGE_THRESHOLD,
};

pub struct BaseBranch {
    pub node: Arc<BranchNode>,
    low: usize,
}

impl BaseBranch {
    pub fn new(node: Arc<BranchNode>) -> Self {
        BaseBranch { node, low: 0 }
    }

    // Try to find the given key starting from `self.low` up to the end.
    // Returns None if `self.low` is already at the end of the node,
    // or if there are no keys left bigger than the specified one.
    // If there are available keys in the node, then it returns the index
    // of the specified key with the boolean set to true or the index containing
    // the first key bigger than the one specified and the boolean set to false.
    fn find_key(&mut self, key: &Key) -> Option<(bool, usize)> {
        if self.low == self.node.n() as usize {
            return None;
        }

        let (found, pos) = find_key_pos(&self.node, key, Some(self.low));

        if found {
            // the key was present return its index and point to the right after key
            self.low = pos + 1;
            return Some((true, pos));
        } else if pos == self.low {
            // there are no keys left bigger than the specified one
            return None;
        } else {
            // key was not present, return and point to the smallest bigger key
            self.low = pos;
            return Some((false, pos));
        }
    }

    pub fn key(&self, i: usize) -> Key {
        get_key(&self.node, i)
    }

    pub fn key_value(&self, i: usize) -> (Key, PageNumber) {
        (self.key(i), self.node.node_pointer(i).into())
    }
}

pub enum DigestResult {
    Finished,
    NeedsMerge(Key),
}

/// A callback which takes ownership of newly created leaves.
pub trait HandleNewBranch {
    fn handle_new_branch(
        &mut self,
        separator: Key,
        node: BranchNode,
        cutoff: Option<Key>,
    ) -> std::io::Result<()>;
}

pub struct BranchUpdater {
    // The 'base' node we are working from. does not exist if DB is empty.
    base: Option<BaseBranch>,
    // The cutoff key, which determines if an operation is in-scope.
    // does not exist for the last branch in the database.
    cutoff: Option<Key>,
    ops_tracker: BranchOpsTracker,
    page_pool: PagePool,
}

impl BranchUpdater {
    pub fn new(page_pool: PagePool, base: Option<BaseBranch>, cutoff: Option<Key>) -> Self {
        BranchUpdater {
            base,
            cutoff,
            ops_tracker: BranchOpsTracker::new(),
            page_pool,
        }
    }

    /// Ingest a key and page number into the branch updater.
    pub fn ingest(&mut self, key: Key, pn: Option<PageNumber>) {
        // keep all elements that are skipped looking for `key`
        let res = self.keep_up_to(Some(&key));

        let Some(pn) = pn else { return };

        if let Some(pos) = res {
            // UNWRAP: if the item has been found it must be a base node
            self.ops_tracker
                .push_update(self.base.as_ref().unwrap(), pos, pn);
        } else {
            self.ops_tracker.push_insert(key, pn);
        }
    }

    pub fn digest(
        &mut self,
        new_branches: &mut impl HandleNewBranch,
    ) -> std::io::Result<DigestResult> {
        self.keep_up_to(None);

        // note: if we need a merge, it'd be more efficient to attempt to combine it with the last
        // branch of the bulk split first rather than pushing the ops onwards. probably irrelevant
        // in practice; bulk splits are rare.

        if self.ops_tracker.body_size() > BRANCH_BULK_SPLIT_THRESHOLD {
            self.try_split(new_branches, BRANCH_BULK_SPLIT_TARGET)?;
        }

        if self.ops_tracker.body_size() > BRANCH_NODE_BODY_SIZE {
            self.try_split(new_branches, self.ops_tracker.body_size() / 2)?;
        }

        if self.ops_tracker.body_size() == 0 {
            Ok(DigestResult::Finished)
        } else if self.ops_tracker.body_size() >= BRANCH_MERGE_THRESHOLD || self.cutoff.is_none() {
            let base = self.base.as_ref();
            let page_pool = &self.page_pool;
            let (ops, gauge) = self.ops_tracker.extract_ops();

            let node = build_branch(base, page_pool, &ops, &gauge);
            let separator = op_first_key(base, &ops[0]);
            new_branches.handle_new_branch(separator, node, self.cutoff)?;

            Ok(DigestResult::Finished)
        } else {
            self.ops_tracker.prepare_merge_ops(self.base.as_ref());

            // UNWRAP: protected above.
            Ok(DigestResult::NeedsMerge(self.cutoff.unwrap()))
        }
    }

    pub fn is_in_scope(&self, key: &Key) -> bool {
        self.cutoff.map_or(true, |k| *key < k)
    }

    pub fn reset_base(&mut self, base: Option<BaseBranch>, cutoff: Option<Key>) {
        self.base = base;
        self.cutoff = cutoff;
    }

    pub fn remove_cutoff(&mut self) {
        self.cutoff = None;
    }

    // Advance the base looking for `up_to`, stops if a bigger Key is found or the end is reached.
    // Collect in `self.ops` all separators that are skipped.
    // Returns the index at which 'up_to' was found, otherwise, returns None.
    fn keep_up_to(&mut self, up_to: Option<&Key>) -> Option<usize> {
        if self.base.is_none() {
            // empty db
            return None;
        }

        // UNWRAP: self.base is not None
        let base = self.base.as_mut().unwrap();

        let from = base.low;
        let base_n = base.node.n() as usize;

        let (found, to) = match up_to {
            // Nothing more to do, the end has already been reached
            None if from == base_n => return None,
            // Jump directly to the end of the base node and update `base.low` accordingly
            None => {
                base.low = base_n;
                (false, base_n)
            }
            Some(up_to) => match base.find_key(up_to) {
                Some(res) => res,
                // already at the end
                None => return None,
            },
        };

        if from != to {
            self.ops_tracker.push_chunk(base, from, to);
        }

        return if found { Some(to) } else { None };
    }

    // Try to perform a split of the current available ops with a target branch node size.
    fn try_split(
        &mut self,
        new_branches: &mut impl HandleNewBranch,
        target: usize,
    ) -> std::io::Result<()> {
        let base = self.base.as_ref();
        while let Some((ops, gauge)) = self
            .ops_tracker
            .extract_ops_until(self.base.as_ref(), target)
        {
            let node = build_branch(base, &self.page_pool, &ops, &gauge);
            let separator = op_first_key(base, &ops[0]);
            new_branches.handle_new_branch(separator, node, self.cutoff)?;
        }
        Ok(())
    }
}

fn op_first_key(base: Option<&BaseBranch>, branch_op: &BranchOp) -> Key {
    // UNWRAPs: `KeepChunk` and `Update` ops only exists when base is Some.
    match branch_op {
        BranchOp::Insert(k, _) => *k,
        BranchOp::Update(pos, _) => base.unwrap().key(*pos),
        BranchOp::KeepChunk(chunk) => base.unwrap().key(chunk.start),
    }
}

fn build_branch(
    base: Option<&BaseBranch>,
    page_pool: &PagePool,
    ops: &[BranchOp],
    gauge: &BranchGauge,
) -> BranchNode {
    let branch = BranchNode::new_in(&page_pool);

    let mut builder = BranchNodeBuilder::new(
        branch,
        gauge.n,
        gauge.prefix_compressed_items(),
        gauge.prefix_len,
    );

    let Some(base) = base else {
        // SAFETY: If no base is avaialble, then all ops are expected to be `BranchOp::Insert`
        for op in ops {
            match op {
                BranchOp::Insert(key, pn) => builder.push(*key, separator_len(key), pn.0),
                _ => panic!("Unextected BranchOp creating a BranchNode without BaseBranch"),
            }
        }
        return builder.finish();
    };

    // This second phase of joining Update and KeepChunk into a unique update chunk is performed
    // for two reasons:
    //
    // 1. It could often happen that the sequence of KeepChunk are interleaved by Update with only a change
    // in the node pointers
    // 2. To avoid keeping all the update information within the BranchOp::KeepChunk because it would require
    // further allocations
    let apply_chunk = |builder: &mut BranchNodeBuilder,
                       base_range: Range<usize>,
                       ops_range: Range<usize>| {
        let n_compressed_left = gauge
            .prefix_compressed_items()
            .saturating_sub(builder.n_pushed());

        let compressed_end = std::cmp::min(base_range.start + n_compressed_left, base_range.end);

        builder.push_chunk(
            &base.node,
            base_range.start,
            compressed_end,
            ops[ops_range]
                .iter()
                .filter_map(|op| {
                    if let BranchOp::Update(pos, pn) = op {
                        Some((pos - base_range.start, *pn))
                    } else {
                        None
                    }
                })
                .into_iter(),
        );

        for pos in compressed_end..base_range.end {
            let (key, pn) = base.key_value(pos);
            builder.push(key, separator_len(&key), pn.0);
        }
    };

    let mut pending_keep_chunk = None;
    // contains a range within `ops` which define the `pending_keep_chunk`
    let mut pending_ops_range = None;
    let mut i = 0;
    while i < ops.len() {
        // Check if the chunk could grow.
        // If yes, then update it and restart the loop on the next operation.
        // Otherwise, apply the pending chunk and let the same operation be re-evaluated.
        if pending_keep_chunk.is_some() {
            // UNWRAPS: pending_keep_chunk has just been checked to be Some.
            // If pending_keep_chunk is Some, then pending_ops_range is also.
            match &ops[i] {
                // found a insert, apply pending chunk
                BranchOp::Insert(_, _) => {
                    apply_chunk(
                        &mut builder,
                        pending_keep_chunk.take().unwrap(),
                        pending_ops_range.take().unwrap(),
                    );
                }
                BranchOp::KeepChunk(chunk) => {
                    let range = pending_keep_chunk.as_mut().unwrap();
                    let ops_range = pending_ops_range.as_mut().unwrap();
                    if range.end == chunk.start {
                        // KeepChunk that follow the pending chunk
                        range.end = chunk.end;
                        ops_range.end += 1;
                        i += 1;
                        continue;
                    } else {
                        // KeepChunk that doens't follow the pending chunk
                        apply_chunk(
                            &mut builder,
                            pending_keep_chunk.take().unwrap(),
                            pending_ops_range.take().unwrap(),
                        );
                    }
                }
                BranchOp::Update(pos, _) => {
                    let range = pending_keep_chunk.as_mut().unwrap();
                    let ops_range = pending_ops_range.as_mut().unwrap();
                    if range.end == *pos {
                        // Update that follow the pending chunk
                        range.end += 1;
                        ops_range.end += 1;
                        i += 1;
                        continue;
                    } else {
                        // Update that doens't follow the pending chunk
                        apply_chunk(
                            &mut builder,
                            pending_keep_chunk.take().unwrap(),
                            pending_ops_range.take().unwrap(),
                        );
                    }
                }
            }
        }

        match &ops[i] {
            BranchOp::Insert(key, pn) => {
                builder.push(*key, separator_len(key), pn.0);
                i += 1;
            }
            BranchOp::KeepChunk(chunk) => {
                pending_keep_chunk = Some(chunk.start..chunk.end);
                pending_ops_range = Some(i..i + 1);
                i += 1;
            }
            BranchOp::Update(pos, _) => {
                pending_keep_chunk = Some(*pos..*pos + 1);
                pending_ops_range = Some(i..i + 1);
                i += 1;
            }
        };
    }

    if let (Some(range), Some(ops_range)) = (pending_keep_chunk, pending_ops_range) {
        apply_chunk(&mut builder, range, ops_range);
    }

    builder.finish()
}

#[derive(Clone)]
pub struct BranchGauge {
    // key and length of the first separator if any
    first_separator: Option<(Key, usize)>,
    prefix_len: usize,
    // sum of all separator lengths (not including the first key).
    sum_separator_lengths: usize,
    // the number of items that are prefix compressed.`None` means everything will be compressed.
    pub prefix_compressed: Option<usize>,
    n: usize,
}

impl Default for BranchGauge {
    fn default() -> Self {
        BranchGauge {
            first_separator: None,
            prefix_len: 0,
            sum_separator_lengths: 0,
            prefix_compressed: None,
            n: 0,
        }
    }
}

impl BranchGauge {
    pub fn ingest_key(&mut self, key: Key, len: usize) {
        let Some((ref first, _)) = self.first_separator else {
            self.first_separator = Some((key, len));
            self.prefix_len = len;

            self.n = 1;
            return;
        };

        if self.prefix_compressed.is_none() {
            self.prefix_len = prefix_len(first, &key);
        }
        self.sum_separator_lengths += len;
        self.n += 1;
    }

    pub fn ingest_branch_op(&mut self, base: Option<&BaseBranch>, op: &BranchOp) {
        // UNWRAPs: `KeepChunk` and `Update` ops only exist when base is Some.
        match op {
            BranchOp::Update(pos, _) => {
                let key = get_key(&base.as_ref().unwrap().node, *pos);
                self.ingest_key(key, separator_len(&key));
            }
            BranchOp::KeepChunk(ref chunk) => {
                self.ingest_chunk(base.as_ref().unwrap(), chunk);
            }
            BranchOp::Insert(key, _) => {
                self.ingest_key(*key, separator_len(key));
            }
        }
    }

    pub fn ingest_chunk(&mut self, base: &BaseBranch, chunk: &KeepChunk) {
        if let Some((ref first, _)) = self.first_separator {
            if self.prefix_compressed.is_none() {
                let chunk_last_key = base.key(chunk.end - 1);
                self.prefix_len = prefix_len(first, &chunk_last_key);
            }
            self.sum_separator_lengths += chunk.sum_separator_lengths;
            self.n += chunk.len();
        } else {
            let chunk_first_key = base.key(chunk.start);
            let chunk_last_key = base.key(chunk.end - 1);
            let first_separator_len = separator_len(&chunk_first_key);

            self.prefix_len = prefix_len(&chunk_first_key, &chunk_last_key);
            self.first_separator = Some((chunk_first_key, first_separator_len));
            self.sum_separator_lengths = chunk.sum_separator_lengths - first_separator_len;
            self.n = chunk.len();
        };
    }

    pub fn stop_prefix_compression(&mut self) {
        assert!(self.prefix_compressed.is_none());
        self.prefix_compressed = Some(self.n);
    }

    fn prefix_compressed_items(&self) -> usize {
        self.prefix_compressed.unwrap_or(self.n)
    }

    fn total_separator_lengths(&self, prefix_len: usize) -> usize {
        match self.first_separator {
            Some((_, first_len)) => node::compressed_separator_range_size(
                first_len,
                self.prefix_compressed.unwrap_or(self.n),
                self.sum_separator_lengths,
                prefix_len,
            ),
            None => 0,
        }
    }

    pub fn body_size_after(&mut self, key: Key, len: usize) -> usize {
        let p;
        let t;
        if let Some((ref first, first_len)) = self.first_separator {
            if self.prefix_compressed.is_none() {
                p = prefix_len(first, &key);
            } else {
                p = self.prefix_len;
            }
            t = node::compressed_separator_range_size(
                first_len,
                self.prefix_compressed.unwrap_or(self.n + 1),
                self.sum_separator_lengths + len,
                p,
            );
        } else {
            t = 0;
            p = len;
        }

        branch_node::body_size(p, t, self.n + 1)
    }

    pub fn body_size_after_chunk(&self, base: &BaseBranch, chunk: &KeepChunk) -> usize {
        let p;
        let t;
        if let Some((ref first, first_len)) = self.first_separator {
            if self.prefix_compressed.is_none() {
                let chunk_last_key = base.key(chunk.end - 1);
                p = prefix_len(first, &chunk_last_key);
            } else {
                p = self.prefix_len;
            }
            t = node::compressed_separator_range_size(
                first_len,
                self.prefix_compressed.unwrap_or(self.n + chunk.len()),
                self.sum_separator_lengths + chunk.sum_separator_lengths,
                p,
            );
        } else {
            let chunk_first_key = base.key(chunk.start);
            let chunk_last_key = base.key(chunk.end - 1);
            let first_len = separator_len(&chunk_first_key);

            p = prefix_len(&chunk_first_key, &chunk_last_key);
            t = node::compressed_separator_range_size(
                first_len,
                self.n + chunk.len(),
                chunk.sum_separator_lengths - first_len,
                p,
            );
        };

        branch_node::body_size(p, t, self.n + chunk.len())
    }

    pub fn body_size(&self) -> usize {
        branch_node::body_size(
            self.prefix_len,
            self.total_separator_lengths(self.prefix_len),
            self.n,
        )
    }

    #[cfg(test)]
    pub fn n(&self) -> usize {
        self.n
    }
}

#[cfg(test)]
pub mod tests {
    use super::{
        get_key, prefix_len, Arc, BaseBranch, BranchGauge, BranchNode, BranchNodeBuilder,
        BranchUpdater, DigestResult, HandleNewBranch, Key, PageNumber, PagePool,
        BRANCH_MERGE_THRESHOLD, BRANCH_NODE_BODY_SIZE,
    };
    use crate::beatree::ops::bit_ops::separator_len;
    use std::collections::HashMap;

    lazy_static::lazy_static! {
        static ref PAGE_POOL: PagePool = PagePool::new();
    }

    #[derive(Default)]
    struct TestHandleNewBranch {
        inner: HashMap<Key, (BranchNode, Option<Key>)>,
    }

    impl HandleNewBranch for TestHandleNewBranch {
        fn handle_new_branch(
            &mut self,
            separator: Key,
            node: BranchNode,
            cutoff: Option<Key>,
        ) -> std::io::Result<()> {
            self.inner.insert(separator, (node, cutoff));
            Ok(())
        }
    }

    #[test]
    fn gauge_stop_uncompressed() {
        let mut gauge = BranchGauge::default();

        gauge.ingest_key([0; 32], 0);

        // push items with a long (16-byte) shared prefix until just before the halfway point.
        let mut items: Vec<Key> = (1..1000u16)
            .map(|i| {
                let mut key = [0; 32];
                key[16..18].copy_from_slice(&i.to_le_bytes());
                key
            })
            .collect();

        items.sort();

        for item in items {
            let len = separator_len(&item);
            if gauge.body_size_after(item, len) >= BRANCH_MERGE_THRESHOLD {
                break;
            }

            gauge.ingest_key(item, len);
        }

        assert!(gauge.body_size() < BRANCH_MERGE_THRESHOLD);

        // now insert an item that collapses the prefix, causing the previously underfull node to
        // become overfull.
        let unprefixed_key = [0xff; 32];
        assert!(gauge.body_size_after(unprefixed_key, 256) > BRANCH_NODE_BODY_SIZE);

        // stop compression. now we can accept more items without collapsing the prefix.
        gauge.stop_prefix_compression();
        assert!(gauge.body_size_after(unprefixed_key, 256) < BRANCH_NODE_BODY_SIZE);
    }

    pub fn prefixed_key(prefix_byte: u8, prefix_len: usize, i: usize) -> Key {
        let mut k = [0u8; 32];
        for x in k.iter_mut().take(prefix_len) {
            *x = prefix_byte;
        }
        k[prefix_len..prefix_len + 2].copy_from_slice(&(i as u16).to_be_bytes());
        k
    }

    fn make_raw_branch(vs: Vec<(Key, usize)>) -> BranchNode {
        let n = vs.len();
        let prefix_len = if vs.len() == 1 {
            separator_len(&vs[0].0)
        } else {
            prefix_len(&vs[0].0, &vs[vs.len() - 1].0)
        };

        let branch = BranchNode::new_in(&PAGE_POOL);
        let mut builder = BranchNodeBuilder::new(branch, n, n, prefix_len);
        for (k, pn) in vs {
            builder.push(k, separator_len(&k), pn as u32);
        }

        builder.finish()
    }

    pub fn make_branch(vs: Vec<(Key, usize)>) -> Arc<BranchNode> {
        Arc::new(make_raw_branch(vs))
    }

    pub fn make_branch_with_body_size_target(
        mut key: impl FnMut(usize) -> Key,
        mut body_size_predicate: impl FnMut(usize) -> bool,
    ) -> Arc<BranchNode> {
        let mut gauge = BranchGauge::default();
        let mut items = Vec::new();
        loop {
            let next_key = key(items.len());
            let s_len = separator_len(&next_key);

            let size = gauge.body_size_after(next_key, s_len);
            if !body_size_predicate(size) {
                break;
            }
            items.push((next_key, items.len()));
            gauge.ingest_key(next_key, s_len);
        }

        make_branch(items)
    }

    // Make a branch node with the specified bbn_pn.
    // Use keys until they are present in the iterator
    // or stop if the body size target is reached.
    pub fn make_branch_until(
        keys: &mut impl Iterator<Item = Key>,
        body_size_target: usize,
        bbn_pn: u32,
    ) -> Arc<BranchNode> {
        let mut gauge = BranchGauge::default();
        let mut items = Vec::new();
        loop {
            let Some(next_key) = keys.next() else {
                break;
            };

            let s_len = separator_len(&next_key);

            let size = gauge.body_size_after(next_key, s_len);
            if size >= body_size_target {
                break;
            }

            items.push((next_key, items.len()));
            gauge.ingest_key(next_key, s_len);
        }

        let mut branch_node = make_raw_branch(items);
        branch_node.set_bbn_pn(bbn_pn);
        Arc::new(branch_node)
    }

    #[test]
    fn is_in_scope() {
        let mut updater = BranchUpdater::new(PAGE_POOL.clone(), None, None);
        assert!(updater.is_in_scope(&[0xff; 32]));

        updater.reset_base(None, Some([0xfe; 32]));
        assert!(updater.is_in_scope(&[0xf0; 32]));
        assert!(updater.is_in_scope(&[0xfd; 32]));
        assert!(!updater.is_in_scope(&[0xfe; 32]));
        assert!(!updater.is_in_scope(&[0xff; 32]));
    }

    #[test]
    fn update() {
        let key = |i| prefixed_key(0x11, 5, i);
        let branch = make_branch((0..500).map(|i| (key(i), i)).collect());

        let mut updater = BranchUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseBranch {
                node: branch,
                low: 0,
            }),
            None,
        );
        let mut new_branches = TestHandleNewBranch::default();

        updater.ingest(key(250), Some(9999.into()));
        let DigestResult::Finished = updater.digest(&mut new_branches).unwrap() else {
            panic!()
        };

        let new_branch_entry = new_branches.inner.get(&key(0)).unwrap();

        let new_branch = &new_branch_entry.0;
        assert_eq!(new_branch.n(), 500);
        assert_eq!(new_branch.node_pointer(0), 0);
        assert_eq!(new_branch.node_pointer(499), 499);
        assert_eq!(new_branch.node_pointer(250), 9999);
    }

    #[test]
    fn insert_rightsized() {
        let key = |i| prefixed_key(0x11, 5, i);
        let branch = make_branch((0..500).map(|i| (key(i * 2), i)).collect());

        let mut updater = BranchUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseBranch {
                node: branch,
                low: 0,
            }),
            None,
        );
        let mut new_branches = TestHandleNewBranch::default();

        updater.ingest(key(251), Some(9999.into()));
        let DigestResult::Finished = updater.digest(&mut new_branches).unwrap() else {
            panic!()
        };

        let new_branch_entry = new_branches.inner.get(&key(0)).unwrap();

        let new_branch = &new_branch_entry.0;
        assert_eq!(new_branch.n(), 501);
        assert_eq!(new_branch.node_pointer(0), 0);
        assert_eq!(new_branch.node_pointer(500), 499);
        assert_eq!(new_branch.node_pointer(126), 9999);
    }

    #[test]
    fn insert_overflowing() {
        let key = |i| prefixed_key(0x11, 5, i);
        let branch = make_branch_with_body_size_target(key, |size| size <= BRANCH_NODE_BODY_SIZE);
        let n = branch.n() as usize;

        let mut updater = BranchUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseBranch {
                node: branch,
                low: 0,
            }),
            None,
        );
        let mut new_branches = TestHandleNewBranch::default();

        updater.ingest(key(n), Some(PageNumber(n as u32)));
        let DigestResult::Finished = updater.digest(&mut new_branches).unwrap() else {
            panic!()
        };

        let new_branch_entry_1 = new_branches.inner.get(&key(0)).unwrap();
        let new_branch_1 = &new_branch_entry_1.0;

        let new_branch_entry_2 = new_branches
            .inner
            .get(&key(new_branch_1.n() as usize))
            .unwrap();
        let new_branch_2 = &new_branch_entry_2.0;

        assert_eq!(new_branch_1.node_pointer(0), 0);
        assert_eq!(
            new_branch_2.node_pointer((new_branch_2.n() - 1) as usize),
            n as u32
        );
    }

    #[test]
    fn delete() {
        let key = |i| prefixed_key(0x11, 5, i);
        let branch = make_branch((0..500).map(|i| (key(i), i)).collect());

        let mut updater = BranchUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseBranch {
                node: branch,
                low: 0,
            }),
            None,
        );
        let mut new_branches = TestHandleNewBranch::default();

        updater.ingest(key(250), None);
        let DigestResult::Finished = updater.digest(&mut new_branches).unwrap() else {
            panic!()
        };

        let new_branch_entry = new_branches.inner.get(&key(0)).unwrap();

        let new_branch = &new_branch_entry.0;
        assert_eq!(new_branch.n(), 499);
        assert_eq!(new_branch.node_pointer(0), 0);
        assert_eq!(new_branch.node_pointer(498), 499);
    }

    #[test]
    fn delete_underflow_and_merge() {
        let key = |i| prefixed_key(0xff, 5, i);
        let key2 = |i| prefixed_key(0xff, 6, i);

        let mut rightsized = false;
        let branch = make_branch_with_body_size_target(key, |size| {
            let res = !rightsized;
            rightsized = size >= BRANCH_MERGE_THRESHOLD;
            res
        });
        rightsized = false;
        let branch2 = make_branch_with_body_size_target(key2, |size| {
            let res = !rightsized;
            rightsized = size >= BRANCH_MERGE_THRESHOLD;
            res
        });

        let n = branch.n() as usize;
        let n2 = branch2.n() as usize;

        let mut updater = BranchUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseBranch {
                node: branch,
                low: 0,
            }),
            Some(key2(0)),
        );
        let mut new_branches = TestHandleNewBranch::default();

        // delete all except the first
        for i in 1..n {
            updater.ingest(key(i), None);
        }
        let DigestResult::NeedsMerge(_) = updater.digest(&mut new_branches).unwrap() else {
            panic!()
        };

        updater.reset_base(
            Some(BaseBranch {
                node: branch2,
                low: 0,
            }),
            None,
        );
        let DigestResult::Finished = updater.digest(&mut new_branches).unwrap() else {
            panic!()
        };

        let new_branch_entry = new_branches.inner.get(&key(0)).unwrap();

        let new_branch = &new_branch_entry.0;
        assert_eq!(new_branch.n() as usize, n2 + 1);
        assert_eq!(new_branch.node_pointer(0), 0);
        assert_eq!(new_branch.node_pointer(n2), (n2 - 1) as u32);
    }

    #[test]
    fn delete_completely() {
        let key = |i| prefixed_key(0x11, 5, i);
        let branch = make_branch((0..500).map(|i| (key(i), i)).collect());

        let mut updater = BranchUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseBranch {
                node: branch,
                low: 0,
            }),
            None,
        );
        let mut new_branches = TestHandleNewBranch::default();

        for i in 0..500 {
            updater.ingest(key(i), None);
        }
        let DigestResult::Finished = updater.digest(&mut new_branches).unwrap() else {
            panic!()
        };

        assert!(new_branches.inner.get(&key(0)).is_none());
    }

    #[test]
    fn delete_underflow_rightmost() {
        let key = |i| prefixed_key(0x11, 5, i);
        let branch = make_branch((0..500).map(|i| (key(i), i)).collect());

        let mut updater = BranchUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseBranch {
                node: branch,
                low: 0,
            }),
            None,
        );
        let mut new_branches = TestHandleNewBranch::default();

        for i in 0..499 {
            updater.ingest(key(i), None);
        }
        let DigestResult::Finished = updater.digest(&mut new_branches).unwrap() else {
            panic!()
        };

        let new_branch_entry = new_branches.inner.get(&key(499)).unwrap();

        let new_branch = &new_branch_entry.0;
        assert_eq!(new_branch.n(), 1);
    }

    #[test]
    fn shared_prefix_collapse() {
        let key = |i| prefixed_key(0x00, 24, i);
        let key2 = |i| prefixed_key(0xff, 24, i);

        let mut rightsized = false;
        let branch = make_branch_with_body_size_target(key, |size| {
            let res = !rightsized;
            rightsized = size >= BRANCH_MERGE_THRESHOLD;
            res
        });
        rightsized = false;
        let branch2 = make_branch_with_body_size_target(key2, |size| {
            let res = !rightsized;
            rightsized = size >= BRANCH_MERGE_THRESHOLD;
            res
        });

        let n = branch.n() as usize;
        let n2 = branch2.n() as usize;

        let mut updater = BranchUpdater::new(
            PAGE_POOL.clone(),
            Some(BaseBranch {
                node: branch,
                low: 0,
            }),
            Some(key2(0)),
        );
        let mut new_branches = TestHandleNewBranch::default();

        // delete the last item, causing a situation where prefix compression needs to be
        // disabled.
        updater.ingest(key(n - 1), None);
        let DigestResult::NeedsMerge(_) = updater.digest(&mut new_branches).unwrap() else {
            panic!()
        };

        updater.reset_base(
            Some(BaseBranch {
                node: branch2,
                low: 0,
            }),
            None,
        );
        let DigestResult::Finished = updater.digest(&mut new_branches).unwrap() else {
            panic!()
        };

        let new_branch_entry_1 = new_branches.inner.get(&key(0)).unwrap();
        let new_branch_1 = &new_branch_entry_1.0;

        // first item has no shared prefix with any other key, causing the size to balloon.
        assert!(new_branch_1.prefix_compressed() != new_branch_1.n());

        assert_eq!(
            get_key(&new_branch_1, new_branch_1.n() as usize - 1),
            key2(0)
        );

        let branch_1_body_size = {
            let mut gauge = BranchGauge::default();
            for i in 0..new_branch_1.n() as usize {
                let key = get_key(&new_branch_1, i);
                gauge.ingest_key(key, separator_len(&key))
            }
            gauge.body_size()
        };
        assert!(branch_1_body_size >= BRANCH_MERGE_THRESHOLD);

        let new_branch_entry_2 = new_branches.inner.get(&key2(1)).unwrap();
        let new_branch_2 = &new_branch_entry_2.0;

        assert_eq!(new_branch_2.n() + new_branch_1.n(), (n + n2 - 1) as u16);
        assert_eq!(
            new_branch_2.node_pointer(new_branch_2.n() as usize - 1),
            n2 as u32 - 1
        );
    }

    #[test]
    fn split_left_node_needs_merge() {
        let compressed_key = |i| prefixed_key(0x00, 25, i);
        let uncompressed_key = |i| prefixed_key(0xFF, 25, i);

        let mut gauge = BranchGauge::default();
        let mut n_keys = 0;

        let mut updater = BranchUpdater::new(PAGE_POOL.clone(), None, None);
        let expected_cutoff = uncompressed_key(u16::MAX as usize);
        updater.reset_base(None, Some(expected_cutoff));

        // Ingesting keys up to a point where just one single key which doesn't share the
        // prefix makes the body size to exceed BRANCH_NODE_BODY_SIZE.
        loop {
            let key = compressed_key(n_keys);

            gauge.ingest_key(key, separator_len(&key));
            updater.ingest(key, Some(PageNumber(n_keys as u32)));

            let unc_key = uncompressed_key(n_keys + 1);
            if gauge.body_size_after(unc_key, separator_len(&unc_key)) > BRANCH_NODE_BODY_SIZE {
                updater.ingest(unc_key, Some(PageNumber((n_keys + 1) as u32)));
                n_keys += 2;
                break;
            }
            n_keys += 1;
        }

        // Digesting this will activate `stop_prefix_compression` and thus create space
        // for more than one single key requiring a merge.
        // This happens because the initial shared prefix is 25bytes and thus the difference
        // in body_size with and without stop_prefix_compression leave a lot of space for
        // non compressed keys.
        let mut new_branches = TestHandleNewBranch::default();
        let DigestResult::NeedsMerge(cutoff) = updater.digest(&mut new_branches).unwrap() else {
            panic!()
        };
        assert_eq!(cutoff, expected_cutoff);
        assert_eq!(new_branches.inner.len(), 0);
        let (ops, _) = updater.ops_tracker.extract_ops();
        assert_eq!(ops.len(), n_keys);
    }

    #[test]
    fn split_only_left_node_finishes() {
        let compressed_key = |i| prefixed_key(0x00, 5, i);
        let uncompressed_key = |i| prefixed_key(0xFF, 5, i);

        let mut gauge = BranchGauge::default();
        let mut n_keys = 0;

        let mut updater = BranchUpdater::new(PAGE_POOL.clone(), None, None);
        updater.reset_base(None, Some(uncompressed_key(u16::MAX as usize)));

        // Ingesting keys up to just before reaching BRANCH_MERGE_THRESHOLD
        loop {
            let key = compressed_key(n_keys);
            let len = separator_len(&key);
            if gauge.body_size_after(key, len) > BRANCH_MERGE_THRESHOLD {
                break;
            }

            gauge.ingest_key(key, len);
            updater.ingest(key, Some(PageNumber(n_keys as u32)));
            n_keys += 1;
        }

        // Ingesting a key which doesn't share the prefix
        updater.ingest(uncompressed_key(n_keys), Some(PageNumber(n_keys as u32)));

        // All ops are expected to perfectly fit in only the left
        let mut new_branches = TestHandleNewBranch::default();
        let DigestResult::Finished = updater.digest(&mut new_branches).unwrap() else {
            panic!()
        };
        assert_eq!(new_branches.inner.len(), 1);

        let (ops, _) = updater.ops_tracker.extract_ops();
        assert!(ops.is_empty());
    }

    #[test]
    fn split_only_left_node_finishes_without_cutoff() {
        let compressed_key = |i| prefixed_key(0x00, 25, i);
        let uncompressed_key = |i| prefixed_key(0xFF, 25, i);

        let mut gauge = BranchGauge::default();
        let mut n_keys = 0;

        let mut updater = BranchUpdater::new(PAGE_POOL.clone(), None, None);
        updater.reset_base(None, None);

        // Ingesting keys up to a point where just one single key which doesn't share the
        // prefix makes the body size to exceed BRANCH_NODE_BODY_SIZE.
        loop {
            let key = compressed_key(n_keys);

            gauge.ingest_key(key, separator_len(&key));
            updater.ingest(key, Some(PageNumber(n_keys as u32)));

            let unc_key = uncompressed_key(n_keys + 1);
            if gauge.body_size_after(unc_key, separator_len(&unc_key)) > BRANCH_NODE_BODY_SIZE {
                updater.ingest(unc_key, Some(PageNumber((n_keys + 1) as u32)));
                break;
            }
            n_keys += 1;
        }

        // This happens because the initial shared prefix is 25bytes and thus the difference
        // in body_size with and without stop_prefix_compression leave a lot of space for
        // non compressed keys to reach the half full requirement, but cutoff is none
        // and thus the node is constructed anyway.
        let mut new_branches = TestHandleNewBranch::default();
        let DigestResult::Finished = updater.digest(&mut new_branches).unwrap() else {
            panic!()
        };
        assert_eq!(new_branches.inner.len(), 1);
        let (ops, _) = updater.ops_tracker.extract_ops();
        assert!(ops.is_empty());
    }
}
