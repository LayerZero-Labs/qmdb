//! A utility for managing in-memory overlays.
//!
//! This module exposes two types. The user-facing [`Overlay`] is opaque and frozen. The internal
//! [`LiveOverlay`] is meant to be used within a session only.
//!
//! Overlays contain weak references to all their ancestors. This allows ancestors to be dropped or
//! committed during the lifetime of the overlay. Importantly, this means that memory is cleaned
//! up gracefully as overlays are dropped and committed.
//!
//! However, creating a new [`LiveOverlay`] requires the user to provide strong references to each
//! of the ancestors which are still alive. It is still the user's responsibility to ensure
//! that all live ancestors are provided, or else data will go silently missing.
//!
//! Looking up a value in an overlay does not have a varying cost. First there is a look-up in an
//! index to see which ancestor has the data, and then a query on the ancestor's storage is done.
//!
//! Creating a new overlay is an O(n) operation in the amount of changes relative to the parent,
//! both in terms of new changes and outdated ancestors.

use crate::{beatree::ValueChange, store::DirtyPage, Root};
use nomt_core::{
    page_id::PageId,
    trie::{KeyPath, Node},
};

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Weak,
};

/// An in-memory overlay of merkle tree and b-tree changes.
pub struct Overlay {
    inner: Arc<OverlayInner>,
}

impl Overlay {
    /// Get the merkle root at this overlay.
    pub fn root(&self) -> Root {
        Root(self.inner.root)
    }

    /// Get the previous root of this overlay.
    pub(super) fn prev_root(&self) -> Root {
        Root(self.inner.prev_root)
    }

    /// Check whether the parent of this overlay matches the provided marker.
    /// If the provided marker is `None`, then this checks that this overlay doesn't have a parent.
    pub(super) fn parent_matches_marker(&self, marker: Option<&OverlayMarker>) -> bool {
        match (self.inner.data.parent_status.as_ref(), marker) {
            (None, _) => true,
            (Some(parent), Some(marker)) => parent.ptr_eq(&marker.0),
            _ => false,
        }
    }

    /// Get the merkle page changes associated uniquely with this overlay.
    pub(super) fn page_changes(&self) -> &HashMap<PageId, DirtyPage> {
        &self.inner.data.pages
    }

    /// Get the merkle page changes associated uniquely with this overlay.
    pub(super) fn value_changes(&self) -> &HashMap<KeyPath, ValueChange> {
        &self.inner.data.values
    }

    /// Get the rollback delta associated uniquely with this overlay.
    pub(super) fn rollback_delta(&self) -> Option<&crate::rollback::Delta> {
        self.inner.rollback_delta.as_ref()
    }

    /// Mark the overlay as committed and return a marker.
    pub(super) fn mark_committed(&self) -> OverlayMarker {
        let status = self.inner.data.status.clone();
        status.commit();
        OverlayMarker(status)
    }
}

struct OverlayInner {
    prev_root: Node,
    root: Node,
    index: Index,
    data: Arc<Data>,
    seqn: u64,
    // ordered by recency.
    ancestor_data: Vec<Weak<Data>>,
    rollback_delta: Option<crate::rollback::Delta>,
}

/// A marker indicating the overlay uniquely, until dropped. Used to enforce commit order.
pub(super) struct OverlayMarker(OverlayStatus);

#[derive(Clone)]
struct OverlayStatus(Arc<AtomicUsize>);

impl OverlayStatus {
    const LIVE: usize = 0;
    const DROPPED: usize = 1;
    const COMMITTED: usize = 2;

    fn new_live() -> Self {
        OverlayStatus(Arc::new(AtomicUsize::new(Self::LIVE)))
    }

    fn commit(&self) {
        self.0.store(Self::COMMITTED, Ordering::Relaxed);
    }

    fn drop(&self) {
        // If the overlay has not been committed, then we will mark it as dead.
        let _ = self.0.compare_exchange(
            Self::LIVE,
            Self::DROPPED,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    fn is_committed(&self) -> bool {
        self.0.load(Ordering::Relaxed) == Self::COMMITTED
    }

    fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    #[cfg(test)]
    fn is_dropped(&self) -> bool {
        self.0.load(Ordering::Relaxed) == Self::DROPPED
    }
}

// Maps changes to sequence number.
#[derive(Default, Clone)]
struct Index {
    pages: imbl::HashMap<PageId, u64>,
    values: imbl::OrdMap<KeyPath, u64>,

    // sorted ascending by seqn.
    pages_by_seqn: imbl::Vector<(u64, PageId)>,
    values_by_seqn: imbl::Vector<(u64, KeyPath)>,
}

impl Index {
    // Prune all items with a sequence number less than the minimum.
    // O(n) in number of pruned items.
    fn prune_below(&mut self, min: u64) {
        loop {
            match self.pages_by_seqn.pop_front() {
                None => break,
                Some((seqn, key)) if seqn >= min => {
                    self.pages_by_seqn.push_front((seqn, key));
                    break;
                }
                Some((seqn, page_id)) => {
                    if let Some(got_seqn) = self
                        .pages
                        .remove(&page_id)
                        .filter(|&got_seqn| got_seqn != seqn && got_seqn >= min)
                    {
                        // page_id has been updated since this point. reinsert.
                        self.pages.insert(page_id, got_seqn);
                    }
                }
            }
        }

        loop {
            match self.values_by_seqn.pop_front() {
                None => break,
                Some((seqn, key)) if seqn >= min => {
                    self.values_by_seqn.push_front((seqn, key));
                    break;
                }
                Some((seqn, key)) => {
                    if let Some(got_seqn) = self
                        .values
                        .remove(&key)
                        .filter(|&got_seqn| got_seqn != seqn && got_seqn >= min)
                    {
                        // key has been updated since this point. reinsert.
                        self.values.insert(key, got_seqn);
                    }
                }
            }
        }
    }

    /// Insert all the value keys in the iterator with the given sequence number.
    ///
    /// The sequence number is assumed to be greater than or equal to the maximum in the vector.
    fn insert_pages(&mut self, seqn: u64, page_ids: impl IntoIterator<Item = PageId>) {
        for page_id in page_ids {
            self.pages_by_seqn.push_back((seqn, page_id.clone()));
            self.pages.insert(page_id, seqn);
        }
    }

    /// Insert all the value keys in the iterator with the given sequence number.
    ///
    /// The sequence number is assumed to be greater than or equal to the maximum in the vector.
    fn insert_values(&mut self, seqn: u64, value_keys: impl IntoIterator<Item = KeyPath>) {
        for key in value_keys {
            self.values_by_seqn.push_back((seqn, key));
            self.values.insert(key, seqn);
        }
    }
}

/// Data associated with a single overlay.
struct Data {
    pages: HashMap<PageId, DirtyPage>,
    values: HashMap<KeyPath, ValueChange>,
    status: OverlayStatus,
    parent_status: Option<OverlayStatus>,
}

impl Drop for Data {
    fn drop(&mut self) {
        self.status.drop();
    }
}

/// An error type indicating that the ancestors provided did not match.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InvalidAncestors {
    /// One of the provided ancestors was not actually an ancestor.
    NotAncestor,
    /// The ancestor chain is incomplete.
    Incomplete,
}

/// A live overlay which is being used as a child. This can be queried for all value/page changes in
/// any of the relevant ancestors.
#[derive(Clone)]
pub(super) struct LiveOverlay {
    parent: Option<Arc<OverlayInner>>,
    ancestor_data: Vec<Arc<Data>>,
    min_seqn: u64,
}

impl LiveOverlay {
    /// Create a new live overlay based on this iterator of ancestors.
    pub(super) fn new<'a>(
        live_ancestors: impl IntoIterator<Item = &'a Overlay>,
    ) -> Result<Self, InvalidAncestors> {
        let mut live_ancestors = live_ancestors.into_iter();
        let Some(parent) = live_ancestors.next().map(|p| p.inner.clone()) else {
            return Ok(LiveOverlay {
                parent: None,
                ancestor_data: Vec::new(),
                min_seqn: 0,
            });
        };

        let mut ancestor_data = Vec::new();
        for (supposed_ancestor, actual_ancestor) in live_ancestors.zip(parent.ancestor_data.iter())
        {
            let Some(actual_ancestor) = actual_ancestor.upgrade() else {
                return Err(InvalidAncestors::Incomplete);
            };

            if !Arc::ptr_eq(&supposed_ancestor.inner.data, &actual_ancestor) {
                return Err(InvalidAncestors::NotAncestor);
            }

            ancestor_data.push(actual_ancestor);
        }

        // verify that the chain is complete. The last ancestor's parent must either be `None` or
        // committed.
        if ancestor_data
            .last()
            .unwrap_or(&parent.data)
            .parent_status
            .as_ref()
            .map_or(false, |status| !status.is_committed())
        {
            return Err(InvalidAncestors::Incomplete);
        }

        let min_seqn = parent.seqn - ancestor_data.len() as u64;

        Ok(LiveOverlay {
            parent: Some(parent),
            ancestor_data,
            min_seqn,
        })
    }

    /// Get a page by ID.
    ///
    /// `None` indicates that the page is not present in the overlay, not that the page doesn't
    /// exist.
    pub(super) fn page(&self, page_id: &PageId) -> Option<&DirtyPage> {
        self.parent
            .as_ref()
            .and_then(|parent| parent.index.pages.get(&page_id))
            .and_then(|seqn| seqn.checked_sub(self.min_seqn))
            .map(|seqn_diff| {
                if seqn_diff as usize == self.ancestor_data.len() {
                    self.parent
                        .as_ref()
                        .unwrap() // UNWRAP: parent existence checked above
                        .data
                        .pages
                        .get(page_id)
                        .unwrap() // UNWRAP: index indicates that data exists.
                } else {
                    self.ancestor_data[self.ancestor_data.len() - seqn_diff as usize - 1]
                        .pages
                        .get(page_id)
                        .unwrap() // UNWRAP: index indicates that data exists.
                }
            })
    }

    /// Get a value change by ID.
    ///
    /// `None` indicates that the value has not changed in the overlay, not that the value doesn't
    /// exist.
    pub(super) fn value(&self, key: &KeyPath) -> Option<ValueChange> {
        self.parent
            .as_ref()
            .and_then(|parent| parent.index.values.get(key))
            .and_then(|seqn| seqn.checked_sub(self.min_seqn))
            .map(|seqn_diff| self.value_inner(key, seqn_diff))
            .cloned()
    }

    fn value_inner(&self, key: &KeyPath, seqn_diff: u64) -> &ValueChange {
        if seqn_diff as usize == self.ancestor_data.len() {
            // UNWRAP: parent existence checked above
            // UNWRAP: index indicates that data exists.
            self.parent.as_ref().unwrap().data.values.get(key).unwrap()
        } else {
            // UNWRAP: index indicates that data exists.
            self.ancestor_data[self.ancestor_data.len() - seqn_diff as usize - 1]
                .values
                .get(key)
                .unwrap()
        }
    }

    /// Iterate all value changes within the given key bounds.
    pub(super) fn value_iter<'a>(
        &'a self,
        start: KeyPath,
        end: Option<KeyPath>,
    ) -> impl Iterator<Item = (KeyPath, &'a ValueChange)> {
        self.parent
            .as_ref()
            .map(move |parent| {
                parent
                    .index
                    .values
                    .range(start..)
                    .take_while(move |(k, _)| end.as_ref().map_or(true, |end| end > k))
                    .filter_map(|(k, seqn)| seqn.checked_sub(self.min_seqn).map(|s| (k, s)))
                    .map(|(k, seqn_diff)| (*k, self.value_inner(k, seqn_diff)))
            })
            .into_iter()
            .flatten()
    }

    /// Finish this overlay and transform it into a frozen [`Overlay`].
    pub(super) fn finish(
        self,
        prev_root: Node,
        root: Node,
        page_changes: HashMap<PageId, DirtyPage>,
        value_changes: HashMap<KeyPath, ValueChange>,
        rollback_delta: Option<crate::rollback::Delta>,
    ) -> Overlay {
        let new_seqn = self.parent.as_ref().map_or(0, |p| p.seqn + 1);

        // rebuild the index, including the new stuff, and excluding stuff from dead overlays.
        let mut index = self
            .parent
            .as_ref()
            .map_or_else(Default::default, |p| p.index.clone());
        index.prune_below(self.min_seqn);

        index.insert_pages(new_seqn, page_changes.keys().cloned());
        index.insert_values(new_seqn, value_changes.keys().cloned());

        let parent_status = self.parent.as_ref().map(|p| p.data.status.clone());
        let ancestor_data = self
            .parent
            .map(|parent| {
                std::iter::once(Arc::downgrade(&parent.data))
                    .chain(self.ancestor_data.into_iter().map(|d| Arc::downgrade(&d)))
                    .collect()
            })
            .unwrap_or_default();

        Overlay {
            inner: Arc::new(OverlayInner {
                index,
                prev_root,
                root,
                data: Arc::new(Data {
                    pages: page_changes,
                    values: value_changes,
                    status: OverlayStatus::new_live(),
                    parent_status,
                }),
                seqn: new_seqn,
                ancestor_data,
                rollback_delta,
            }),
        }
    }

    /// Get the overlay's root. If this is an empty overlay, returns `None`.
    pub(super) fn parent_root(&self) -> Option<Node> {
        self.parent.as_ref().map(|p| p.root)
    }
}

#[cfg(test)]
mod tests {
    use lazy_static::lazy_static;
    use nomt_core::page_id::{ChildPageIndex, PageId, ROOT_PAGE_ID};

    use crate::{
        beatree::ValueChange,
        io::PagePool,
        page_cache::PageMut,
        page_diff::PageDiff,
        store::{BucketIndex, BucketInfo, DirtyPage, SharedMaybeBucketIndex},
    };

    use super::{InvalidAncestors, LiveOverlay};
    use std::collections::{HashMap, VecDeque};

    lazy_static! {
        static ref PAGE_POOL: PagePool = PagePool::new();
    }
    fn dummy_page(page_id: PageId, value: u8, bucket_info: BucketInfo) -> DirtyPage {
        let mut page = PageMut::pristine_empty(&PAGE_POOL, &page_id);
        page.set_node(0, [value; 32]);
        DirtyPage {
            page: page.freeze(),
            diff: PageDiff::default(),
            bucket: bucket_info,
        }
    }

    #[test]
    fn blank_overlay_ok() {
        assert!(LiveOverlay::new(None).is_ok());
    }

    #[test]
    fn not_ancestor() {
        let a = LiveOverlay::new(None).unwrap().finish(
            [0; 32],
            [1; 32],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        let a1 = LiveOverlay::new(None).unwrap().finish(
            [1; 32],
            [2; 32],
            HashMap::new(),
            HashMap::new(),
            None,
        );

        let mut ancestors = VecDeque::new();
        ancestors.push_front(a);
        let b = LiveOverlay::new(&ancestors).unwrap().finish(
            [2; 32],
            [3; 32],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        ancestors.push_front(b);

        let _a = std::mem::replace(&mut ancestors[1], a1);
        assert!(matches!(
            LiveOverlay::new(&ancestors),
            Err(InvalidAncestors::NotAncestor)
        ));
    }

    #[test]
    fn incomplete_ancestors() {
        let a = LiveOverlay::new(None).unwrap().finish(
            [0; 32],
            [1; 32],
            HashMap::new(),
            HashMap::new(),
            None,
        );

        let mut ancestors = VecDeque::new();
        ancestors.push_front(a);
        let b = LiveOverlay::new(&ancestors).unwrap().finish(
            [1; 32],
            [2; 32],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        ancestors.push_front(b);
        let c = LiveOverlay::new(&ancestors).unwrap().finish(
            [2; 32],
            [3; 32],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        ancestors.push_front(c);

        for overlay in ancestors.iter().take(2) {
            assert!(!overlay
                .inner
                .data
                .parent_status
                .as_ref()
                .unwrap()
                .is_committed());
        }
        assert!(matches!(
            LiveOverlay::new(ancestors.iter().take(1)),
            Err(InvalidAncestors::Incomplete)
        ));
        assert!(matches!(
            LiveOverlay::new(ancestors.iter().take(2)),
            Err(InvalidAncestors::Incomplete)
        ));
        assert!(matches!(LiveOverlay::new(ancestors.iter().take(3)), Ok(_)));
    }

    #[test]
    fn drop_propagation() {
        let a = LiveOverlay::new(None).unwrap().finish(
            [0; 32],
            [1; 32],
            HashMap::new(),
            HashMap::new(),
            None,
        );

        let mut ancestors = VecDeque::new();
        ancestors.push_front(a);
        let b = LiveOverlay::new(&ancestors).unwrap().finish(
            [1; 32],
            [2; 32],
            HashMap::new(),
            HashMap::new(),
            None,
        );

        drop(ancestors);

        assert!(b.inner.data.parent_status.as_ref().unwrap().is_dropped());
    }

    #[test]
    fn commit_overrides_drop_propagation() {
        let a = LiveOverlay::new(None).unwrap().finish(
            [0; 32],
            [1; 32],
            HashMap::new(),
            HashMap::new(),
            None,
        );

        let mut ancestors = VecDeque::new();
        ancestors.push_front(a);
        let b = LiveOverlay::new(&ancestors).unwrap().finish(
            [1; 32],
            [2; 32],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        ancestors[0].inner.data.status.commit();
        drop(ancestors);

        assert!(!b.inner.data.parent_status.as_ref().unwrap().is_dropped());
        assert!(b.inner.data.parent_status.as_ref().unwrap().is_committed());
    }

    #[test]
    fn committed_ancestors_considered_complete() {
        let a = LiveOverlay::new(None).unwrap().finish(
            [0; 32],
            [1; 32],
            HashMap::new(),
            HashMap::new(),
            None,
        );

        let mut ancestors = VecDeque::new();
        ancestors.push_front(a);
        let b = LiveOverlay::new(&ancestors).unwrap().finish(
            [1; 32],
            [2; 32],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        ancestors.push_front(b);
        let c = LiveOverlay::new(&ancestors).unwrap().finish(
            [2; 32],
            [3; 32],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        ancestors.push_front(c);

        let _ = ancestors.pop_back().unwrap().mark_committed();

        assert!(matches!(
            LiveOverlay::new(ancestors.iter().take(1)),
            Err(InvalidAncestors::Incomplete)
        ));
        assert!(matches!(LiveOverlay::new(ancestors.iter().take(2)), Ok(_)));
    }

    #[test]
    fn new_overlay_contains_all_new() {
        let page1a = dummy_page(
            ROOT_PAGE_ID,
            1,
            BucketInfo::FreshOrDependent(SharedMaybeBucketIndex::new(None)),
        );
        let page1b = dummy_page(
            ROOT_PAGE_ID,
            2,
            BucketInfo::FreshOrDependent(SharedMaybeBucketIndex::new(None)),
        );

        let key1 = [1; 32];
        let value1a = ValueChange::Insert(vec![1, 2, 3]);
        let value1b = ValueChange::Insert(vec![4, 5, 6]);

        let page_map = vec![(ROOT_PAGE_ID, page1a)].into_iter().collect();
        let value_map = vec![(key1, value1a)].into_iter().collect();
        let a = LiveOverlay::new(None)
            .unwrap()
            .finish([0; 32], [1; 32], page_map, value_map, None);

        let page_map = vec![(ROOT_PAGE_ID, page1b)].into_iter().collect();
        let value_map = vec![(key1, value1b)].into_iter().collect();
        let b = LiveOverlay::new(Some(&a))
            .unwrap()
            .finish([1; 32], [2; 32], page_map, value_map, None);

        let c = LiveOverlay::new([&b, &a]).unwrap();

        assert_eq!(c.page(&ROOT_PAGE_ID).unwrap().page.node(0), [2; 32]);
        assert_eq!(c.value(&key1).unwrap(), ValueChange::Insert(vec![4, 5, 6]));
    }

    #[test]
    fn access_prior_overlay_data() {
        let page_id_1 = ROOT_PAGE_ID;
        let page_id_2 = page_id_1
            .child_page_id(ChildPageIndex::new(0).unwrap())
            .unwrap();
        let page_id_3 = page_id_1
            .child_page_id(ChildPageIndex::new(1).unwrap())
            .unwrap();

        let page_1 = dummy_page(
            page_id_1.clone(),
            1,
            BucketInfo::FreshOrDependent(SharedMaybeBucketIndex::new(None)),
        );
        let page_2 = dummy_page(
            page_id_2.clone(),
            2,
            BucketInfo::FreshOrDependent(SharedMaybeBucketIndex::new(None)),
        );
        let page_3 = dummy_page(
            page_id_3.clone(),
            3,
            BucketInfo::FreshOrDependent(SharedMaybeBucketIndex::new(None)),
        );

        let key_1 = [1; 32];
        let key_2 = [2; 32];
        let key_3 = [3; 32];

        let value_1 = ValueChange::Insert(vec![1, 2, 3]);
        let value_2 = ValueChange::Insert(vec![4, 5, 6]);
        let value_3 = ValueChange::Insert(vec![7, 8, 9]);

        let pages = [
            (page_id_1.clone(), page_1),
            (page_id_2.clone(), page_2),
            (page_id_3.clone(), page_3),
        ];
        let values = [
            (key_1, value_1.clone()),
            (key_2, value_2.clone()),
            (key_3, value_3.clone()),
        ];

        // build a chain of 3 overlays, each with one unique key and value.
        let mut ancestors = VecDeque::new();
        for ((page_id, page), (key, value)) in pages.into_iter().zip(values) {
            let page_map = [(page_id, page)].into_iter().collect();
            let value_map = [(key, value)].into_iter().collect();
            let overlay = LiveOverlay::new(&ancestors)
                .unwrap()
                .finish([0; 32], [1; 32], page_map, value_map, None);
            ancestors.push_front(overlay);
        }

        // ensure they can be accessed from an overlay that descends from that chain.
        let overlay = LiveOverlay::new(&ancestors).unwrap();

        assert_eq!(overlay.page(&page_id_1).unwrap().page.node(0), [1; 32]);
        assert_eq!(overlay.page(&page_id_2).unwrap().page.node(0), [2; 32]);
        assert_eq!(overlay.page(&page_id_3).unwrap().page.node(0), [3; 32]);

        assert_eq!(
            overlay.value(&key_1).unwrap(),
            ValueChange::Insert(vec![1, 2, 3])
        );
        assert_eq!(
            overlay.value(&key_2).unwrap(),
            ValueChange::Insert(vec![4, 5, 6])
        );
        assert_eq!(
            overlay.value(&key_3).unwrap(),
            ValueChange::Insert(vec![7, 8, 9])
        );
    }

    #[test]
    fn fresh_or_dependent_propagates_correctly() {
        let maybe_bucket = SharedMaybeBucketIndex::new(None);
        let page = dummy_page(
            ROOT_PAGE_ID,
            1,
            BucketInfo::FreshOrDependent(maybe_bucket.clone()),
        );
        let page2 = dummy_page(
            ROOT_PAGE_ID,
            2,
            BucketInfo::FreshOrDependent(maybe_bucket.clone()),
        );

        let a = LiveOverlay::new(None).unwrap().finish(
            [0; 32],
            [1; 32],
            vec![(ROOT_PAGE_ID, page)].into_iter().collect(),
            HashMap::new(),
            None,
        );
        let b = LiveOverlay::new([&a]).unwrap().finish(
            [1; 32],
            [2; 32],
            vec![(ROOT_PAGE_ID, page2)].into_iter().collect(),
            HashMap::new(),
            None,
        );
        a.mark_committed();

        let bucket = BucketIndex::new(69);
        maybe_bucket.set(bucket);

        let c = LiveOverlay::new([&b]).unwrap();

        let BucketInfo::FreshOrDependent(ref b) = c.page(&ROOT_PAGE_ID).unwrap().bucket else {
            panic!()
        };

        assert_eq!(b.get(), Some(bucket));
    }

    #[test]
    fn prune_below_works() {
        let page_id_1 = ROOT_PAGE_ID;
        let page_1 = dummy_page(
            page_id_1.clone(),
            1,
            BucketInfo::FreshOrDependent(SharedMaybeBucketIndex::new(None)),
        );

        let page_id_2 = ROOT_PAGE_ID
            .child_page_id(ChildPageIndex::new(1).unwrap())
            .unwrap();
        let page_2 = dummy_page(
            page_id_2.clone(),
            2,
            BucketInfo::FreshOrDependent(SharedMaybeBucketIndex::new(None)),
        );
        let page_2b = dummy_page(
            page_id_2.clone(),
            3,
            BucketInfo::FreshOrDependent(SharedMaybeBucketIndex::new(None)),
        );

        let key_1 = [1; 32];
        let val_1 = ValueChange::Insert(vec![1, 2, 3]);

        let key_2 = [2; 32];
        let val_2 = ValueChange::Insert(vec![4, 5, 6]);
        let val_2b = ValueChange::Insert(vec![7, 8, 9]);

        // create 2 overlays. The first stores page1/page2 , val1/val2. The second descends and
        // stores page2b/val2b. Pruning the first should remove '1' but preserve 2b.

        let page_map = vec![(page_id_1.clone(), page_1), (page_id_2.clone(), page_2)]
            .into_iter()
            .collect();
        let value_map = vec![(key_1, val_1.clone()), (key_2, val_2.clone())]
            .into_iter()
            .collect();
        let a = LiveOverlay::new(None)
            .unwrap()
            .finish([0; 32], [1; 32], page_map, value_map, None);

        let page_map = vec![(page_id_2.clone(), page_2b)].into_iter().collect();
        let value_map = vec![(key_2, val_2b.clone())].into_iter().collect();
        let b = LiveOverlay::new([&a])
            .unwrap()
            .finish([0; 32], [1; 32], page_map, value_map, None);

        a.mark_committed();

        let c = LiveOverlay::new([&b]).unwrap().finish(
            [0; 32],
            [1; 32],
            HashMap::new(),
            HashMap::new(),
            None,
        );

        // ensure everything from seqn 0 has been pruned.
        assert_eq!(c.inner.index.pages_by_seqn[0].0, 1);
        assert_eq!(c.inner.index.values_by_seqn[0].0, 1);

        let d = LiveOverlay::new([&c, &b]).unwrap();

        // pruned stuff is gone.
        assert!(d.page(&page_id_1).is_none());
        assert!(d.value(&key_1).is_none());

        // page2b/val2b present.
        assert_eq!(d.page(&page_id_2).unwrap().page.node(0), [3; 32]);
        assert_eq!(d.value(&key_2).unwrap(), ValueChange::Insert(vec![7, 8, 9]));
    }
}
