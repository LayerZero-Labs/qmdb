//! A set of pages that the page walker draws upon and which is filled by `Seek`ing.

use nomt_core::page_id::PageId;
use std::{collections::HashMap, sync::Arc};

use super::BucketInfo;
use crate::{
    io::PagePool,
    page_cache::{Page, PageMut},
    page_diff::PageDiff,
};

/// A page in the [`PageSet`] can have two different origins.
#[derive(Clone)]
pub enum PageOrigin {
    /// It could have been fetched from the hash table, thereby having an associated `BucketInfo`.
    Persisted(BucketInfo),
    /// It could have been reconstructed on the fly without being stored anywhere.
    /// It keeps track of the total number of leaves contained within the page,
    /// in child pages, and which nodes in the page have been reconstructed.
    Reconstructed {
        page_leaves_counter: u64,
        children_leaves_counter: u64,
        diff: PageDiff,
    },
}

impl PageOrigin {
    /// Extract `BucketInfo` from [`PageOrigin::Persisted`] variant.
    pub fn bucket_info(self) -> Option<BucketInfo> {
        match self {
            PageOrigin::Persisted(bucket_info) => Some(bucket_info),
            PageOrigin::Reconstructed { .. } => None,
        }
    }

    /// Extract the number of leaves in the children from [`PageOrigin::Reconstructed`] variant.
    pub fn children_leaves_counter(&self) -> Option<u64> {
        match self {
            PageOrigin::Reconstructed {
                children_leaves_counter,
                ..
            } => Some(*children_leaves_counter),
            PageOrigin::Persisted(_) => None,
        }
    }

    /// Extract the number of leaves in the page from [`PageOrigin::Reconstructed`] variant.
    pub fn page_leaves_counter(&self) -> Option<u64> {
        match self {
            PageOrigin::Reconstructed {
                page_leaves_counter,
                ..
            } => Some(*page_leaves_counter),
            PageOrigin::Persisted(_) => None,
        }
    }

    /// Extract the [`PageDiff`] from [`PageOrigin::Reconstructed`] variant.
    pub fn page_diff(&self) -> Option<&PageDiff> {
        match self {
            PageOrigin::Reconstructed { diff, .. } => Some(diff),
            PageOrigin::Persisted(_) => None,
        }
    }
}

pub struct PageSet {
    map: HashMap<PageId, (Page, PageOrigin)>,
    warm_up_map: Option<Arc<HashMap<PageId, (Page, PageOrigin)>>>,
    page_pool: PagePool,
}

impl PageSet {
    pub fn new(page_pool: PagePool, warmed_up: Option<FrozenSharedPageSet>) -> Self {
        PageSet {
            map: HashMap::new(),
            page_pool,
            warm_up_map: warmed_up.map(|x| x.0),
        }
    }

    /// Freeze this page-set and make a shareable version of it. This returns a frozen page set
    /// containing all insertions into this map.
    pub fn freeze(self) -> FrozenSharedPageSet {
        FrozenSharedPageSet(Arc::new(self.map))
    }

    fn get_warmed_up(&self, page_id: &PageId) -> Option<(Page, PageOrigin)> {
        self.warm_up_map
            .as_ref()
            .and_then(|m| m.get(page_id))
            .map(|(p, b)| (p.clone(), b.clone()))
    }
}

impl super::page_walker::PageSet for PageSet {
    fn fresh(&self, page_id: &PageId) -> PageMut {
        let page = PageMut::pristine_empty(&self.page_pool, &page_id);
        page
    }

    fn contains(&self, page_id: &PageId) -> bool {
        self.map.contains_key(&page_id)
    }

    fn get(&self, page_id: &PageId) -> Option<(Page, PageOrigin)> {
        self.map
            .get(&page_id)
            .map(|(p, page_origin)| (p.clone(), page_origin.clone()))
            .or_else(|| self.get_warmed_up(page_id))
    }

    fn insert(&mut self, page_id: PageId, page: Page, page_origin: PageOrigin) {
        self.map.insert(page_id, (page, page_origin));
    }
}

/// A frozen, shared page set. This is cheap to clone.
#[derive(Clone)]
pub struct FrozenSharedPageSet(Arc<HashMap<PageId, (Page, PageOrigin)>>);
