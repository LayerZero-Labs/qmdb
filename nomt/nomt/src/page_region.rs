//! Implements a regioning scheme for the page tree for use as a [`crate::rw_pass_cell::Region`].
//! This enables safe, sharded, mutable access to portions of the page tree.
//!
//! Each [`PageRegion`] spans an entire sub-section of the page tree for exclusive access.
//! This is either a page P and all of its possible descendants, or an inclusive range of a
//! page P's child pages and all of their descendants.
//!
//! In the first case, all of the P's ancestors are considered as non-exclusive, and in the second
//! case P and all of its ancestors are considered as non-exclusive.
//!
//! That is, a region either implies this:
//!
//! ```text
//!                             ┌─┐
//!                             └┼┘
//!                              │
//!            Shared Here & ── ┌▼┐
//!            Above            └┬┘
//!                              │
//!                             ┌▼┐ Exclusive Here & Below
//!                          ┌──┴┬┴──┐
//!                          │   │   │
//!                         ┌▼┐ ┌▼┐ ┌▼┐
//!                         └─┘ └─┘ └─┘
//! ```
//!
//! or this:
//!
//! ```text
//!                             ┌─┐
//!                             └┼┘
//!                              │
//!                             ┌▼┐   Shared Here &
//!                    ┌───┬───┬┴─┘   Above
//!                    │   │   │
//! Exclusive Here &  ┌▼┐ ┌▼┐ ┌▼┐ │ ┌─┐
//! Below             └─┘ └─┘ └─┘ │ └─┘
//!                               │
//! ```

use nomt_core::page_id::{ChildPageIndex, PageId, ROOT_PAGE_ID};

use crate::rw_pass_cell::{Region, RegionContains};

/// A region of pages for shared/exclusive access.
///
/// See module docs for more details.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct PageRegion {
    // `path` has different effects depending on whether `exclusive_min` is `Some` or `None`.
    // non-exclusive access: if min is `None` any page along the path to this one (excluding this).
    //                       otherwise, including this page.
    // exclusive access: if min is `None`, just to this page ID. otherwise none.
    path: PageId,
    exclusive_min: Option<PageId>,
    // exclusive access: to all pages within `min.or(self.path)` and this, inclusive
    exclusive_max: PageId,
}

impl PageRegion {
    /// Create a `PageRegion` which implies read access to all the pages along the path to
    /// the `page_id` and exclusive access to all pages descending from it.
    pub fn from_page_id(page_id: PageId) -> Self {
        PageRegion {
            exclusive_min: None,
            exclusive_max: page_id.max_descendant(),
            path: page_id,
        }
    }

    /// Create a `PageRegion` which implies read access to all the pages along the path to
    /// and including the `page_id`, and exclusive access to all pages between the `min` child of
    /// `page_id` and the `max` child of `page_id`, inclusive.
    ///
    /// # Panics
    ///
    /// Panics if min > max or the page ID is at the maximum depth.
    pub fn from_page_id_descendants(
        page_id: PageId,
        min: ChildPageIndex,
        max: ChildPageIndex,
    ) -> Self {
        assert!(min <= max);
        PageRegion {
            exclusive_min: Some(page_id.child_page_id(min).unwrap()),
            exclusive_max: page_id.child_page_id(max).unwrap().max_descendant(),
            path: page_id,
        }
    }

    /// The region encompassing the entire page tree.
    pub fn universe() -> Self {
        PageRegion::from_page_id(ROOT_PAGE_ID)
    }

    /// Whether the region contains a page ID exclusively.
    pub fn contains_exclusive(&self, page: &PageId) -> bool {
        &self.exclusive_min() <= page && &self.exclusive_max() >= page
    }

    /// Whether the region contains a page ID non-exclusively.
    pub fn contains_non_exclusive(&self, page: &PageId) -> bool {
        self.non_exclusive_max().map_or(false, |non_exclusive_max| {
            page == &non_exclusive_max || non_exclusive_max.is_descendant_of(page)
        })
    }

    /// Whether this region fully encompasses another.
    pub fn encompasses(&self, other: &PageRegion) -> bool {
        self.exclusive_min() <= other.exclusive_min()
            && self.exclusive_max() >= other.exclusive_max()
    }

    /// Whether this region has no exclusive access overlaps with another region.
    pub fn excludes_unique(&self, other: &PageRegion) -> bool {
        // 2 success cases for no overlap in exclusives
        //   - our max exclusive < their min exclusive
        //   - their max exclusive < our min exclusive page
        //
        // we don't need to do additional checks for shared/exclusive overlaps, but only because
        // a region ALWAYS spans an entire subtree (ensured by `max_descendant` calls). so there
        // is no way for another region to exist _below_ (>) our `exclusive_max` and therefore land
        // some of its non_exclusive set within our exclusive set.
        self.exclusive_max() < other.exclusive_min() || other.exclusive_max() < self.exclusive_min()
    }

    fn non_exclusive_max(&self) -> Option<PageId> {
        match self.exclusive_min {
            None if self.path == ROOT_PAGE_ID => None,
            None => Some(self.path.parent_page_id()),
            Some(_) => Some(self.path.clone()),
        }
    }

    /// Get the page ID which marks the beginning of the exclusive range.
    pub fn exclusive_min(&self) -> PageId {
        match self.exclusive_min {
            None => self.path.clone(),
            Some(ref min) => min.clone(),
        }
    }

    /// Get the page ID which marks the end of the exclusive range.
    pub fn exclusive_max(&self) -> PageId {
        self.exclusive_max.clone()
    }
}

/// SAFETY: Page ID has no interior mutability. We have upheld the contract of the trait.
impl Region for PageRegion {
    // SAFETY: when this returns true, this region contains every ID the other region does, for
    //         both shared and exclusive access.
    fn encompasses(&self, other: &PageRegion) -> bool {
        PageRegion::encompasses(self, other)
    }

    // SAFETY: this is commutative and ensures mutual exclusion.
    fn excludes_unique(&self, other: &PageRegion) -> bool {
        PageRegion::excludes_unique(self, other)
    }
}

/// SAFETY: Page ID has no interior mutability. We have upheld the contract of the trait.
unsafe impl RegionContains<PageId> for PageRegion {
    // SAFETY: This result is stable as long as `self` and `page_id` remain the same.
    fn contains(&self, page_id: &PageId) -> bool {
        PageRegion::contains_exclusive(self, page_id) || self.contains_non_exclusive(page_id)
    }

    fn contains_exclusive(&self, page_id: &PageId) -> bool {
        PageRegion::contains_exclusive(self, page_id)
    }
}

#[cfg(test)]
mod tests {
    use super::{ChildPageIndex, PageRegion, ROOT_PAGE_ID};
    use nomt_core::page_id::MAX_CHILD_INDEX;

    // test that exclusivity is bidirectional doesn't affect the result, then return the result.
    fn test_exclusion_both(left: &PageRegion, right: &PageRegion) -> bool {
        assert_eq!(left.excludes_unique(right), right.excludes_unique(left));
        left.excludes_unique(right)
    }

    #[test]
    fn universe_encompasses_all() {
        for i in 0..=MAX_CHILD_INDEX {
            let root_child_page = ROOT_PAGE_ID
                .child_page_id(ChildPageIndex::new(i).unwrap())
                .unwrap();
            let region = PageRegion::from_page_id(root_child_page);

            assert!(PageRegion::universe().encompasses(&region));
            assert!(!test_exclusion_both(&PageRegion::universe(), &region));
        }
    }

    #[test]
    fn mutually_exclude_sibling_regions() {
        let root_page = ROOT_PAGE_ID;
        let region_a = PageRegion::from_page_id_descendants(
            root_page.clone(),
            ChildPageIndex::new(0).unwrap(),
            ChildPageIndex::new(31).unwrap(),
        );

        let region_b = PageRegion::from_page_id_descendants(
            root_page.clone(),
            ChildPageIndex::new(32).unwrap(),
            ChildPageIndex::new(63).unwrap(),
        );

        assert!(PageRegion::universe().encompasses(&region_a));
        assert!(PageRegion::universe().encompasses(&region_b));
        assert!(test_exclusion_both(&region_a, &region_b));
    }

    #[test]
    fn overlap_not_mutually_exclusive() {
        let root_page = ROOT_PAGE_ID;

        let region = PageRegion::from_page_id_descendants(
            root_page.clone(),
            ChildPageIndex::new(1).unwrap(),
            ChildPageIndex::new(2).unwrap(),
        );

        let overlap_left = PageRegion::from_page_id_descendants(
            root_page.clone(),
            ChildPageIndex::new(0).unwrap(),
            ChildPageIndex::new(1).unwrap(),
        );

        let overlap_right = PageRegion::from_page_id_descendants(
            root_page.clone(),
            ChildPageIndex::new(2).unwrap(),
            ChildPageIndex::new(3).unwrap(),
        );

        assert!(PageRegion::universe().encompasses(&region));
        assert!(PageRegion::universe().encompasses(&overlap_left));
        assert!(PageRegion::universe().encompasses(&overlap_right));

        assert!(!test_exclusion_both(&region, &overlap_left));
        assert!(!test_exclusion_both(&region, &overlap_right));
    }

    #[test]
    fn child_not_mutually_exclusive() {
        let region_a_page = ROOT_PAGE_ID
            .child_page_id(ChildPageIndex::new(0).unwrap())
            .unwrap();

        let region_a = PageRegion::from_page_id(region_a_page.clone());

        let region_b_page = region_a_page
            .child_page_id(ChildPageIndex::new(0).unwrap())
            .unwrap();

        let region_b = PageRegion::from_page_id(region_b_page);

        assert!(PageRegion::universe().encompasses(&region_a));
        assert!(PageRegion::universe().encompasses(&region_b));
        assert!(region_a.encompasses(&region_b));
        assert!(!region_b.encompasses(&region_a));

        assert!(!test_exclusion_both(&region_a, &region_b));
    }
}
