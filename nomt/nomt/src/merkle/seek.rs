//! Multiplexer for page requests.

use crate::{
    beatree::{
        iterator::{IterOutput, NeededLeavesIter},
        AsyncLeafLoad, BeatreeIterator, LeafNodeRef, PageNumber, ReadTransaction as BeatreeReadTx,
        ValueChange,
    },
    io::{CompleteIo, FatPage, IoHandle},
    page_cache::{Page, PageCache, PageMut},
    store::{BucketIndex, PageLoad, PageLoader},
    HashAlgorithm,
};

use super::{
    page_set::{PageOrigin, PageSet},
    page_walker::PageSet as _,
    BucketInfo, LiveOverlay, PAGE_ELISION_THRESHOLD,
};

use nomt_core::{
    page::DEPTH,
    page_id::{PageId, ROOT_PAGE_ID},
    trie::{self, KeyPath, Node, ValueHash},
    trie_pos::TriePosition,
};

use std::cmp::Ordering;
use std::collections::{
    hash_map::{Entry, HashMap},
    VecDeque,
};

use bitvec::prelude::*;
use slab::Slab;

const MAX_INFLIGHT: usize = 1024;

struct SeekRequest {
    key: KeyPath,
    position: TriePosition,
    page_id: Option<PageId>,
    siblings: Vec<Node>,
    state: RequestState,
    ios: usize,
}

impl SeekRequest {
    fn new<H: HashAlgorithm>(
        read_transaction: &BeatreeReadTx,
        overlay: &LiveOverlay,
        key: KeyPath,
        root: Node,
    ) -> SeekRequest {
        let state = if trie::is_terminator::<H>(&root) {
            RequestState::Completed(None)
        } else if trie::is_leaf::<H>(&root) {
            RequestState::begin_leaf_fetch::<H>(read_transaction, overlay, &TriePosition::new())
        } else {
            RequestState::Seeking
        };

        let mut request = SeekRequest {
            key,
            position: TriePosition::new(),
            page_id: None,
            siblings: Vec::new(),
            state,
            ios: 0,
        };

        // The iterator must be advanced until it is blocked.
        if let RequestState::FetchingLeaf { .. } = request.state {
            request.continue_leaf_fetch::<H>(None)
        };

        request
    }

    fn note_io(&mut self) {
        self.ios += 1;
    }

    fn is_completed(&self) -> bool {
        match self.state {
            RequestState::Seeking => false,
            RequestState::FetchingLeaf { .. } => false,
            RequestState::FetchingLeaves { .. } => false,
            RequestState::Completed(_) => true,
        }
    }

    fn next_query(&mut self) -> Option<IoQuery> {
        match self.state {
            RequestState::Seeking => Some(IoQuery::MerklePage(match self.page_id {
                None => ROOT_PAGE_ID,
                // UNWRAP: all page IDs for key paths are in scope.
                Some(ref page_id) => page_id
                    .child_page_id(self.position.child_page_index())
                    .unwrap(),
            })),
            RequestState::FetchingLeaf {
                ref mut needed_leaves,
                ..
            } => needed_leaves.next().map(|pn| IoQuery::LeafPage(pn)),
            RequestState::FetchingLeaves {
                ref mut needed_leaves,
                ..
            } => needed_leaves.next().map(|pn| IoQuery::LeafPage(pn)),
            RequestState::Completed(_) => None,
        }
    }

    fn continue_seek<H: HashAlgorithm>(
        &mut self,
        read_transaction: &BeatreeReadTx,
        overlay: &LiveOverlay,
        page_id: PageId,
        page: &Page,
        record_siblings: bool,
        page_set: &mut PageSet,
    ) {
        let RequestState::Seeking = self.state else {
            panic!("seek past end")
        };

        // terminator should have yielded completion.
        assert!(self.position.depth() as usize % DEPTH == 0);

        self.page_id = Some(page_id.clone());

        // take enough bits to get us to the end of this page.
        let bits = self.key.view_bits::<Msb0>()[self.position.depth() as usize..]
            .iter()
            .by_vals()
            .take(DEPTH);

        for bit in bits {
            self.position.down(bit);

            let cur_node = page.node(self.position.node_index());
            if record_siblings {
                self.siblings.push(page.node(self.position.sibling_index()));
            }

            if trie::is_leaf::<H>(&cur_node) {
                self.state =
                    RequestState::begin_leaf_fetch::<H>(read_transaction, overlay, &self.position);
                if let RequestState::FetchingLeaf { .. } = self.state {
                    self.continue_leaf_fetch::<H>(None);
                }
                return;
            } else if trie::is_terminator::<H>(&cur_node) {
                self.state = RequestState::Completed(None);
                return;
            }
        }

        // The traversal reached the end of the current page, and possibly the next page is being elided.
        // This information resides in the page within the `elided_children` field.
        //
        // UNWRAP: `continue_seek` scans multiple `DEPTH` bits, so `position` must be at the last
        // layer of the current page.
        let child_page_id = page_id
            .child_page_id(self.position.child_page_index())
            .unwrap();

        if page
            .elided_children()
            .is_elided(self.position.child_page_index())
        {
            // Avoid reconstructing already reconstructed pages.
            if page_set.contains(&child_page_id) {
                return;
            }

            // Begin fetching the beatree leaves required for reconstructing the elided pages.
            //
            // NOTE: It could happen that two or more sequential requests end up in the same elided
            // subtree, thus, a leaves fetch process could begin while the previous SeekRequest has already
            // started the same process. This duplication of work is accepted because they will share the same
            // beatree leaves, and the requests will be woken one after the other when a needed leaf is received.
            // This is less consuming than the overhead introduced by making the second SeekRequest depend
            // on the first one or sharing some data between them.
            self.state = RequestState::begin_leaves_fetch::<H>(
                read_transaction,
                &self.position,
                page.clone(),
            );
            self.continue_leaves_fetch::<H>(page_set, overlay, None);
        }
    }

    fn continue_leaf_fetch<H: HashAlgorithm>(&mut self, leaf: Option<LeafNodeRef>) {
        let RequestState::FetchingLeaf {
            ref mut overlay_deletions,
            ref mut beatree_iterator,
            ..
        } = self.state
        else {
            panic!("called continue_leaf_fetch without active iterator");
        };

        if let Some(leaf) = leaf {
            beatree_iterator.provide_leaf(leaf);
        }

        let manage_deletions = |deletions: &[KeyPath], mut cur_idx: usize, key: &KeyPath| {
            while cur_idx < deletions.len() {
                match key.cmp(&deletions[cur_idx]) {
                    Ordering::Less => {
                        // key is before the deletion
                        break;
                    }
                    Ordering::Equal => {
                        cur_idx += 1;
                        return (cur_idx, true);
                    }
                    Ordering::Greater => {
                        // deletion is before the key. move on until we find a match or go past
                        cur_idx += 1;
                    }
                }
            }

            (cur_idx, false)
        };

        let mut deletions_idx = 0;
        let (key, value_hash) = loop {
            match beatree_iterator.next() {
                None => panic!("leaf must exist position={}", self.position.path()),
                Some(IterOutput::Blocked) => {
                    overlay_deletions.drain(..deletions_idx);
                    return;
                }
                Some(IterOutput::Item(key, value)) => {
                    let (new_d_idx, should_skip) =
                        manage_deletions(&overlay_deletions, deletions_idx, &key);
                    deletions_idx = new_d_idx;
                    if should_skip {
                        continue;
                    }

                    break (key, H::hash_value(&value));
                }
                Some(IterOutput::OverflowItem(key, value_hash, _)) => {
                    let (new_d_idx, should_skip) =
                        manage_deletions(&overlay_deletions, deletions_idx, &key);
                    deletions_idx = new_d_idx;
                    if should_skip {
                        continue;
                    }

                    break (key, value_hash);
                }
            }
        };

        self.state = RequestState::Completed(Some(trie::LeafData {
            key_path: key,
            value_hash,
        }));
    }

    fn continue_leaves_fetch<H: HashAlgorithm>(
        &mut self,
        page_set: &mut PageSet,
        overlay: &LiveOverlay,
        leaf: Option<LeafNodeRef>,
    ) {
        let RequestState::FetchingLeaves {
            ref page,
            ref range,
            ref mut beatree_iterator,
            ref mut collected_leaf_data,
            ..
        } = self.state
        else {
            panic!("called continue_leaves_fetch without active iterator");
        };

        if let Some(leaf) = leaf {
            beatree_iterator.provide_leaf(leaf);
        }

        // Collect all items in range from the beatree iterator.
        while let Some(iter_output) = beatree_iterator.next() {
            match iter_output {
                IterOutput::Blocked => return,
                IterOutput::Item(key, value) => {
                    collected_leaf_data.push((key, H::hash_value(&value)))
                }
                IterOutput::OverflowItem(key, value_hash, _) => {
                    collected_leaf_data.push((key, value_hash))
                }
            }
        }

        // All values in the range have been collected from the leaves,
        // now they need to be intersected with what resides in the overlay.
        let mut final_leaf_data_collection = Vec::with_capacity(collected_leaf_data.len());
        let overlay_leaves = overlay.value_iter(range.0, range.1);

        if collected_leaf_data.is_empty() {
            let leaves_data = overlay_leaves.filter_map(|(overlay_key, overlay_valuechange)| {
                let value_hash = match overlay_valuechange {
                    ValueChange::Insert(value) => H::hash_value(value),
                    ValueChange::InsertOverflow(_, value_hash) => *value_hash,
                    _ => {
                        // Overlays sometimes contain "naked deletions", i.e. deleted items that
                        // never existed in the beatree.
                        //
                        // Because `collected_leaf_data` is empty, we cannot have any true
                        // deletions, so we can safely ignore these.
                        return None;
                    }
                };
                Some((overlay_key, value_hash))
            });
            final_leaf_data_collection.extend(leaves_data);
        } else {
            // Both `collected_leaf_data` and `overlay_leaves` are ordered,
            // they need to be merged while updating the previous values,
            // taking into consideration overlay changes.

            let mut beatree_leaf_idx = 0;

            for (overlay_key, overlay_valuechange) in overlay.value_iter(range.0, range.1) {
                let start_idx = beatree_leaf_idx;
                while beatree_leaf_idx < collected_leaf_data.len()
                    && collected_leaf_data[beatree_leaf_idx].0 < overlay_key
                {
                    beatree_leaf_idx += 1;
                }

                final_leaf_data_collection
                    .extend_from_slice(&collected_leaf_data[start_idx..beatree_leaf_idx]);
                let key_path = collected_leaf_data
                    .get(beatree_leaf_idx)
                    .map(|(key_path, _)| key_path);

                let value_hash = match overlay_valuechange {
                    ValueChange::Insert(value) => H::hash_value(value),
                    ValueChange::InsertOverflow(_, value_hash) => *value_hash,
                    // Deleted key, skip it.
                    ValueChange::Delete if key_path == Some(&overlay_key) => {
                        beatree_leaf_idx += 1;
                        continue;
                    }
                    // Overlays can contain "naked deletions", i.e. deleted items that
                    // never existed in the beatree. Skip these as they have no effect.
                    ValueChange::Delete => continue,
                };

                if key_path == Some(&overlay_key) {
                    // The leaf data has been updated in the overlay.
                    beatree_leaf_idx += 1;
                }

                final_leaf_data_collection.push((overlay_key, value_hash));
            }

            final_leaf_data_collection.extend_from_slice(&collected_leaf_data[beatree_leaf_idx..]);
        }

        // UNWRAP: The `page_id` from where the leaves request started must be `Some`.
        let maybe_pages = super::page_walker::reconstruct_pages::<H>(
            page,
            self.page_id.clone().unwrap(),
            self.position.clone(),
            page_set,
            final_leaf_data_collection,
        );

        if let Some(pages) = maybe_pages {
            for (page_id, page, diff, page_leaves_counter, children_leaves_counter) in pages {
                page_set.insert(
                    page_id,
                    page,
                    PageOrigin::Reconstructed {
                        page_leaves_counter,
                        children_leaves_counter,
                        diff,
                    },
                );
            }
        }

        // Now that all pages are reconstructed, seeking can be resumed.
        self.state = RequestState::Seeking;
    }
}

enum RequestState {
    Seeking,
    // Fetching one leaf
    FetchingLeaf {
        overlay_deletions: Vec<KeyPath>,
        beatree_iterator: BeatreeIterator,
        needed_leaves: NeededLeavesIter,
    },
    // Fetching multiple leaves to construct elided pages.
    FetchingLeaves {
        page: Page,
        range: (KeyPath, Option<KeyPath>),
        beatree_iterator: BeatreeIterator,
        needed_leaves: NeededLeavesIter,
        collected_leaf_data: Vec<(KeyPath, ValueHash)>,
    },
    Completed(Option<trie::LeafData>),
}

impl RequestState {
    fn begin_leaf_fetch<H: HashAlgorithm>(
        read_transaction: &BeatreeReadTx,
        overlay: &LiveOverlay,
        pos: &TriePosition,
    ) -> Self {
        let (start, end) = range_bounds(pos.raw_path(), pos.depth() as usize);

        let overlay_items = overlay.value_iter(start, end);
        let mut overlay_deletions = vec![];

        for (key_path, overlay_change) in overlay_items {
            let value_hash = match overlay_change {
                ValueChange::Delete => {
                    // All deletes must be collected to filter out from the beatree iterator.
                    overlay_deletions.push(key_path);
                    continue;
                }
                // If an insertion is found within the overlay, it is expected to be
                // the item associated with the leaf that is being fetched.
                ValueChange::Insert(value) => H::hash_value(value),
                ValueChange::InsertOverflow(_, value_hash) => value_hash.clone(),
            };

            return RequestState::Completed(Some(trie::LeafData {
                key_path,
                value_hash,
            }));
        }

        // Fall back to iterator (most likely).
        let beatree_iterator = read_transaction.iterator(start, end);
        let needed_leaves = beatree_iterator.needed_leaves();
        RequestState::FetchingLeaf {
            overlay_deletions,
            beatree_iterator,
            needed_leaves,
        }
    }

    fn begin_leaves_fetch<H: HashAlgorithm>(
        read_transaction: &BeatreeReadTx,
        pos: &TriePosition,
        page: Page,
    ) -> Self {
        let (start, end) = range_bounds(pos.raw_path(), pos.depth() as usize);

        let beatree_iterator = read_transaction.iterator(start, end);
        let needed_leaves = beatree_iterator.needed_leaves();
        RequestState::FetchingLeaves {
            page,
            range: (start, end),
            beatree_iterator,
            needed_leaves,
            collected_leaf_data: Vec::with_capacity(PAGE_ELISION_THRESHOLD as usize),
        }
    }
}

fn range_bounds(raw_path: KeyPath, depth: usize) -> (KeyPath, Option<KeyPath>) {
    if depth == 0 {
        return (KeyPath::default(), None);
    }
    let start = raw_path;
    let mut end = start.clone();
    let mut depth = depth - 1;
    loop {
        let end_bits = end.view_bits_mut::<Msb0>();
        if !end_bits[depth] {
            end_bits.set(depth, true);
            break;
        } else {
            end_bits.set(depth, false);

            // this is only reached when the start position has the form `111111...`.
            if depth == 0 {
                return (start, None);
            } else {
                depth -= 1;
            }
        }
    }

    (start, Some(end))
}

#[derive(Hash, PartialEq, Eq)]
enum IoQuery {
    MerklePage(PageId),
    LeafPage(PageNumber),
}

enum IoRequest {
    Merkle(PageLoad),
    Leaf(AsyncLeafLoad),
}

/// The `Seeker` seeks for the terminal nodes of multiple keys simultaneously, multiplexing requests
/// over an I/O pool.
///
/// Advance the seeker, provide new keys, and handle completions. A key path
/// request is considered completed when the terminal node has been found, and all pages along the
/// path to the terminal node are in the page cache.
///
/// Requests are completed in the order they are submitted.
pub struct Seeker<H: HashAlgorithm> {
    root: Node,
    beatree_read_transaction: BeatreeReadTx,
    page_cache: PageCache,
    overlay: LiveOverlay,
    io_handle: IoHandle,
    page_loader: PageLoader,
    processed: usize,
    requests: VecDeque<SeekRequest>,
    io_waiters: HashMap<IoQuery, Vec<usize>>,
    io_slab: Slab<IoRequest>,
    /// FIFO, pushed onto back.
    idle_requests: VecDeque<usize>,
    /// FIFO, pushed onto back.
    idle_page_loads: VecDeque<usize>,
    record_siblings: bool,
    _marker: std::marker::PhantomData<H>,
}

impl<H: HashAlgorithm> Seeker<H> {
    /// Create a new `Seeker`.
    pub fn new(
        root: Node,
        beatree_read_transaction: BeatreeReadTx,
        page_cache: PageCache,
        overlay: LiveOverlay,
        io_handle: IoHandle,
        page_loader: PageLoader,
        record_siblings: bool,
    ) -> Self {
        Seeker {
            root,
            beatree_read_transaction,
            page_cache,
            overlay,
            io_handle,
            page_loader,
            processed: 0,
            requests: VecDeque::new(),
            io_waiters: HashMap::new(),
            io_slab: Slab::new(),
            idle_requests: VecDeque::new(),
            idle_page_loads: VecDeque::new(),
            record_siblings,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }

    pub fn has_room(&self) -> bool {
        self.io_waiters.len() < MAX_INFLIGHT
    }

    pub fn first_key(&self) -> Option<&KeyPath> {
        self.requests.front().map(|request| &request.key)
    }

    pub fn has_live_requests(&self) -> bool {
        self.idle_page_loads.len() < self.io_slab.len()
    }

    /// Try to submit as many requests as possible.
    pub fn submit_all(&mut self, page_set: &mut PageSet) {
        if !self.has_room() {
            return;
        }
        self.submit_idle_page_loads();
        self.submit_idle_key_path_requests(page_set);
    }

    /// Take the result of a complete request.
    pub fn take_completion(&mut self) -> Option<Seek> {
        if self.requests.front().map_or(false, |r| r.is_completed()) {
            // UNWRAP: just checked existence.
            let request = self.requests.pop_front().unwrap();
            // PANIC: checked above.
            let RequestState::Completed(terminal) = request.state else {
                unreachable!()
            };

            self.processed += 1;

            return Some(Seek {
                key: request.key,
                position: request.position,
                page_id: request.page_id,
                siblings: request.siblings,
                ios: request.ios,
                terminal,
            });
        }

        None
    }

    /// Try to process the next I/O. Does not block the current thread.
    pub fn try_recv_page(&mut self, page_set: &mut PageSet) -> std::io::Result<()> {
        if let Ok(io) = self.io_handle.try_recv() {
            self.handle_completion(page_set, io)?;
        }

        Ok(())
    }

    /// Block on processing the next I/O. Blocks the current thread.
    ///
    /// Panics if the I/O pool is down.
    pub fn recv_page(&mut self, page_set: &mut PageSet) -> std::io::Result<()> {
        // UNWRAP: I/O pool is not expected to hangup.
        let io = self.io_handle.recv().unwrap();
        self.handle_completion(page_set, io)?;
        Ok(())
    }

    /// Push a request for key path.
    pub fn push(&mut self, key: KeyPath) {
        let request_index = self.processed + self.requests.len();
        self.requests.push_back(SeekRequest::new::<H>(
            &self.beatree_read_transaction,
            &self.overlay,
            key,
            self.root,
        ));
        self.idle_requests.push_back(request_index);
    }

    // resubmit all idle page loads until no more remain.
    fn submit_idle_page_loads(&mut self) {
        while let Some(slab_index) = self.idle_page_loads.pop_front() {
            self.submit_idle_page_load(slab_index);
        }
    }

    // submit the next page for each idle key path request until backpressuring or no more progress
    // can be made.
    fn submit_idle_key_path_requests(&mut self, page_set: &mut PageSet) {
        while self.has_room() {
            match self.idle_requests.pop_front() {
                None => return,
                Some(request_index) => {
                    self.submit_key_path_request(page_set, request_index);
                }
            }
        }
    }

    // submit a page load which is currently in the slab, but idle.
    fn submit_idle_page_load(&mut self, slab_index: usize) {
        if let IoRequest::Merkle(ref mut page_load) = self.io_slab[slab_index] {
            if !self
                .page_loader
                .probe(page_load, &self.io_handle, slab_index as u64)
            {
                // PANIC: seek should really never reach fresh pages. we only request a page if
                // we seek to an internal node above it, and in that case the page really should
                // exist.
                //
                // This code path being reached indicates that the page definitely doesn't exist
                // within the hash-table.
                //
                // Maybe better to return an error here. it indicates some kind of DB corruption.
                unreachable!()
            }
        }
    }

    // submit the next page for this key path request.
    fn submit_key_path_request(&mut self, page_set: &mut PageSet, request_index: usize) {
        let i = if request_index < self.processed {
            return;
        } else {
            request_index - self.processed
        };

        let request = &mut self.requests[i];

        while let Some(query) = request.next_query() {
            match query {
                IoQuery::MerklePage(page_id) => {
                    // Attempt to fetch a page from the page_set. If retrieval from memory
                    // is required, then insert it with its origin into the page set.
                    let maybe_page =
                        page_set
                            .get(&page_id)
                            .map(|(page, _origin)| page)
                            .or_else(|| {
                                super::get_in_memory_page(&self.overlay, &self.page_cache, &page_id)
                                    .map(|(page, bucket_info)| {
                                        page_set.insert(
                                            page_id.clone(),
                                            page.clone(),
                                            PageOrigin::Persisted(bucket_info),
                                        );
                                        page
                                    })
                            });

                    if let Some(page) = maybe_page {
                        request.continue_seek::<H>(
                            &self.beatree_read_transaction,
                            &self.overlay,
                            page_id.clone(),
                            &page,
                            self.record_siblings,
                            page_set,
                        );
                        continue;
                    }

                    let vacant_entry =
                        match self.io_waiters.entry(IoQuery::MerklePage(page_id.clone())) {
                            Entry::Occupied(mut occupied) => {
                                assert!(!occupied.get().contains(&request_index));
                                occupied.get_mut().push(request_index);
                                break;
                            }
                            Entry::Vacant(vacant) => vacant,
                        };

                    request.note_io();

                    let load = self.page_loader.start_load(page_id.clone());
                    vacant_entry.insert(vec![request_index]);
                    let slab_index = self.io_slab.insert(IoRequest::Merkle(load));
                    self.submit_idle_page_load(slab_index);
                    return;
                }
                IoQuery::LeafPage(page_number) => {
                    let vacant_entry = match self.io_waiters.entry(IoQuery::LeafPage(page_number)) {
                        Entry::Occupied(mut occupied) => {
                            assert!(!occupied.get().contains(&request_index));
                            occupied.get_mut().push(request_index);
                            break;
                        }
                        Entry::Vacant(vacant) => vacant,
                    };

                    let slab_index = self.io_slab.vacant_key();
                    match self.beatree_read_transaction.load_leaf_async(
                        page_number,
                        &self.io_handle,
                        slab_index as u64,
                    ) {
                        Ok(leaf) => {
                            match request.state {
                                RequestState::FetchingLeaf { .. } => {
                                    request.continue_leaf_fetch::<H>(Some(leaf))
                                }
                                RequestState::FetchingLeaves { .. } => request
                                    .continue_leaves_fetch::<H>(
                                        page_set,
                                        &self.overlay,
                                        Some(leaf),
                                    ),
                                // PANIC: The state cannot be `RequestState::Seeking`, we would have entered
                                // the other match arm otherwise. `request.next_query()` has already checked for it.
                                _ => unreachable!(),
                            }
                            continue;
                        }
                        Err(leaf_load) => {
                            request.note_io();

                            vacant_entry.insert(vec![request_index]);
                            assert_eq!(slab_index, self.io_slab.insert(IoRequest::Leaf(leaf_load)));
                            return;
                        }
                    }
                }
            }
        }
    }

    fn handle_completion(&mut self, page_set: &mut PageSet, io: CompleteIo) -> std::io::Result<()> {
        io.result?;
        let slab_index = io.command.user_data as usize;

        // UNWRAP: requests are submitted with slab indices that are populated and never cleared
        // until this point is reached.
        match self.io_slab.get_mut(slab_index).unwrap() {
            IoRequest::Merkle(merkle_load) => {
                // UNWRAP: page loader always submits a `Read` command that yields a fat page.
                let page = io.command.kind.unwrap_buf();
                match merkle_load.try_complete(page) {
                    Some((page, bucket)) => {
                        self.handle_merkle_page_and_continue(page_set, slab_index, page, bucket)
                    }
                    None => self.idle_page_loads.push_back(slab_index),
                };
            }
            IoRequest::Leaf(_) => {
                // UNWRAP: read transaction always submits a `Read` command that yields a fat page.
                self.handle_leaf_page_and_continue(
                    slab_index,
                    io.command.kind.unwrap_buf(),
                    page_set,
                );
            }
        }

        Ok(())
    }

    fn handle_merkle_page_and_continue(
        &mut self,
        page_set: &mut PageSet,
        slab_index: usize,
        page_data: FatPage,
        bucket_index: BucketIndex,
    ) {
        let IoRequest::Merkle(page_load) = self.io_slab.remove(slab_index) else {
            panic!()
        };

        let page = PageMut::pristine_with_data(page_data).freeze();

        // insert the page into the page cache and working page set.
        let page = self
            .page_cache
            .insert(page_load.page_id().clone(), page.clone(), bucket_index);
        page_set.insert(
            page_load.page_id().clone(),
            page.clone(),
            PageOrigin::Persisted(BucketInfo::Known(bucket_index)),
        );

        for waiting_request in self
            .io_waiters
            .remove(&IoQuery::MerklePage(page_load.page_id().clone()))
            .into_iter()
            .flatten()
        {
            if waiting_request < self.processed {
                continue;
            }
            let idx = waiting_request - self.processed;
            let request = &mut self.requests[idx];
            assert!(!request.is_completed());

            request.continue_seek::<H>(
                &self.beatree_read_transaction,
                &self.overlay,
                page_load.page_id().clone(),
                &page,
                self.record_siblings,
                page_set,
            );

            if !request.is_completed() {
                self.idle_requests.push_back(waiting_request);
            }
        }
    }

    fn handle_leaf_page_and_continue(
        &mut self,
        slab_index: usize,
        page: FatPage,
        page_set: &mut PageSet,
    ) {
        let IoRequest::Leaf(leaf_load) = self.io_slab.remove(slab_index) else {
            panic!()
        };

        let page_number = leaf_load.page_number();
        let leaf = leaf_load.finish(page);
        for waiting_request in self
            .io_waiters
            .remove(&IoQuery::LeafPage(page_number))
            .into_iter()
            .flatten()
        {
            if waiting_request < self.processed {
                continue;
            }
            let idx = waiting_request - self.processed;
            let request = &mut self.requests[idx];
            assert!(!request.is_completed());

            match request.state {
                RequestState::FetchingLeaf { .. } => {
                    request.continue_leaf_fetch::<H>(Some(leaf.clone()));
                }
                RequestState::FetchingLeaves { .. } => {
                    request.continue_leaves_fetch::<H>(page_set, &self.overlay, Some(leaf.clone()));
                }
                // PANIC: This is strictly called only when `IoRequest::Leaf` is received, thus
                // no `RequestState::Seeking` is expected.
                _ => unreachable!(),
            }

            if !request.is_completed() {
                self.idle_requests.push_back(waiting_request);
            }
        }
    }
}

/// The result of a seek.
#[derive(Clone)]
pub struct Seek {
    /// The key being sought.
    #[allow(dead_code)]
    pub key: KeyPath,
    /// The position in the trie where the terminal node was found.
    pub position: TriePosition,
    /// The page ID where the terminal node was found. `None` at root.
    pub page_id: Option<PageId>,
    /// The siblings along the path to the terminal, including the terminal's sibling.
    /// Empty if the seeker wasn't configured to record siblings.
    pub siblings: Vec<Node>,
    /// The terminal node encountered.
    pub terminal: Option<trie::LeafData>,
    /// The number of I/Os loaded uniquely for this `Seek`.
    /// This does not include pages loaded from the cache, or pages which were already requested
    /// for another seek.
    #[allow(dead_code)]
    pub ios: usize,
}

#[cfg(test)]
mod tests {
    use bitvec::prelude::*;
    use nomt_core::trie::KeyPath;

    use crate::merkle::seek::range_bounds;

    #[test]
    fn key_range() {
        fn make_path(path: BitVec<u8, Msb0>) -> KeyPath {
            let mut k = KeyPath::default();
            k.view_bits_mut::<Msb0>()[..path.len()].copy_from_bitslice(&path);
            k
        }

        let key_a = make_path(bitvec![u8, Msb0; 0, 0, 0, 0]);
        assert_eq!(range_bounds(key_a, 0).1, None);
        assert_eq!(
            range_bounds(key_a, 1).1,
            Some(make_path(bitvec![u8, Msb0; 1]))
        );
        assert_eq!(
            range_bounds(key_a, 2).1,
            Some(make_path(bitvec![u8, Msb0; 0, 1]))
        );
        assert_eq!(
            range_bounds(key_a, 3).1,
            Some(make_path(bitvec![u8, Msb0; 0, 0, 1]))
        );
        assert_eq!(
            range_bounds(key_a, 4).1,
            Some(make_path(bitvec![u8, Msb0; 0, 0, 0, 1]))
        );

        let key_b = make_path(bitvec![u8, Msb0; 1, 1, 1, 1]);
        assert_eq!(range_bounds(key_b, 0).1, None);
        assert_eq!(range_bounds(key_b, 1).1, None);
        assert_eq!(range_bounds(key_b, 2).1, None);
        assert_eq!(range_bounds(key_b, 3).1, None);
        assert_eq!(range_bounds(key_b, 4).1, None);

        let key_c = make_path(bitvec![u8, Msb0; 0, 1, 0]);
        assert_eq!(
            range_bounds(key_c, 3).1,
            Some(make_path(bitvec![u8, Msb0; 0, 1, 1]))
        );
        let key_d = make_path(bitvec![u8, Msb0; 0, 1, 0, 1]);
        assert_eq!(
            range_bounds(key_d, 4).1,
            Some(make_path(bitvec![u8, Msb0; 0, 1, 1, 0]))
        );
    }
}
