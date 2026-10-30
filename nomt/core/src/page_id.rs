//! This module contains all the relevant methods to work with PageIds.
//!
//! A PageId is an unique identifier for a Page in a tree of pages with branching factor 2^6 and
//! a maximum depth of 42, with the root page counted as depth 0.
//!
//! Each PageId consists of a list of numbers between 0 and 2^6 - 1, which encodes a path through
//! the tree. The list may have between 0 and 42 (inclusive) items.
//!
//! Page IDs also have a disambiguated 256-bit representation which is given by starting with a
//! blank bit pattern, and then repeatedly shifting it to the left by 6 bits, then adding the next
//! child index, then adding 1. This disambiguated representation uniquely encodes all the page IDs
//! in a fixed-width bit pattern as, essentially, a base-64 integer.

use crate::{page::DEPTH, trie::KeyPath};
use arrayvec::ArrayVec;
use bitvec::prelude::*;
use ruint::Uint;

// The encoded representation of the highest valid page ID: the highest one at layer 42.
const HIGHEST_ENCODED_42: Uint<256, 4> = Uint::from_be_bytes([
    16, 65, 4, 16, 65, 4, 16, 65, 4, 16, 65, 4, 16, 65, 4, 16, 65, 4, 16, 65, 4, 16, 65, 4, 16, 65,
    4, 16, 65, 4, 16, 64,
]);

pub const MAX_PAGE_DEPTH: usize = 42;

/// A unique ID for a page.
///
/// # Ordering
///
/// Page IDs are ordered "depth-first" such that:
///  - An ID is always less than its child IDs.
///  - An ID's child IDs are ordered ascending by child index.
///  - An ID's child IDs are always less than any sibling IDs to the right of the ID.
///
/// This property lets us refer to sub-trees cleanly with simple ordering statements.
#[derive(Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct PageId {
    path: ArrayVec<u8, MAX_PAGE_DEPTH>,
}

/// The root page is the one containing the sub-trie directly descending from the root node.
pub const ROOT_PAGE_ID: PageId = PageId {
    path: ArrayVec::new_const(),
};

pub const MAX_CHILD_INDEX: u8 = (1 << DEPTH) - 1;

/// The number of children each Page ID has.
pub const NUM_CHILDREN: usize = MAX_CHILD_INDEX as usize + 1;

/// The index of a children of a page.
///
/// Each page can be thought of a root-less binary tree. The leaves of that tree are roots of
/// subtrees stored in subsequent pages. There are 64 (2^[`DEPTH`]) children in each page.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct ChildPageIndex(u8);

impl ChildPageIndex {
    pub fn new(index: u8) -> Option<Self> {
        if index > MAX_CHILD_INDEX {
            return None;
        }
        Some(Self(index))
    }

    pub fn to_u8(self) -> u8 {
        self.0
    }
}

impl Clone for PageId {
    fn clone(&self) -> Self {
        let mut new_path = ArrayVec::new();
        unsafe {
            // SAFETY: values are POD and lengths match.
            new_path.set_len(self.path.len());
        }

        new_path.copy_from_slice(&self.path);
        PageId { path: new_path }
    }
}

impl PageId {
    /// Decode a page ID from its disambiguated representation.
    ///
    /// This can fall out of bounds.
    pub fn decode(bytes: [u8; 32]) -> Result<Self, InvalidPageIdBytes> {
        let mut uint = Uint::from_be_bytes(bytes);

        if uint > HIGHEST_ENCODED_42 {
            return Err(InvalidPageIdBytes);
        }

        let leading_zeros = uint.leading_zeros();
        let bit_count = 256 - leading_zeros;
        let sextets = (bit_count + 5) / 6;

        if bit_count == 0 {
            return Ok(ROOT_PAGE_ID);
        }

        // we iterate the sextets from least significant to most significant, subtracting out
        // 1 from each sextet. if the last sextet is zero after this operation, we skip it.
        let mut path = ArrayVec::new();
        for _ in 0..sextets - 1 {
            uint -= Uint::<256, 4>::from(1);
            let x = uint & Uint::from(0b111111);
            path.push(x.to::<u8>());
            uint >>= DEPTH;
        }
        if uint.byte(0) != 0 {
            uint -= Uint::<256, 4>::from(1);
            path.push(uint.byte(0));
        }
        path.reverse();

        Ok(PageId { path })
    }

    /// Get the child index of the page at the given depth.
    ///
    /// This panics if the depth of the page is not at least `depth + 1`.
    pub fn child_index_at_level(&self, depth: usize) -> ChildPageIndex {
        ChildPageIndex(self.path[depth])
    }

    /// Encode this page ID to its disambiguated (fixed-width) representation.
    pub fn encode(&self) -> [u8; 32] {
        if self.path.len() < 10 {
            // 7 + 6*9 = 61 - max bit-width of a page with depth 9.
            let mut word: u64 = 0;
            for limb in &self.path {
                word += (limb + 1) as u64;
                word <<= 6;
            }

            let mut buf = [0u8; 32];
            let word_bytes = word.to_be_bytes();
            buf[24..32].copy_from_slice(&word_bytes);
            buf
        } else {
            let mut uint = Uint::<256, 4>::from(0);
            for limb in &self.path {
                uint += Uint::from(limb + 1);
                uint <<= 6;
            }

            uint.to_be_bytes::<32>()
        }
    }

    /// Get a length-dependent representation of the page id.
    pub fn length_dependent_encoding(&self) -> &[u8] {
        &self.path[..]
    }

    /// Get the depth of the page ID. The depth of the [`ROOT_PAGE_ID`] is 0.
    pub fn depth(&self) -> usize {
        self.path.len()
    }

    /// Construct the Child PageId given the previous PageId and the child index.
    ///
    /// Child index must be a 6 bit integer, two most significant bits must be zero.
    /// Passed PageId must be a valid PageId and be located in a layer below 42 otherwise
    /// `PageIdOverflow` will be returned.
    pub fn child_page_id(&self, child_index: ChildPageIndex) -> Result<Self, ChildPageIdError> {
        if self.path.len() >= MAX_PAGE_DEPTH {
            return Err(ChildPageIdError::PageIdOverflow);
        }

        let mut path = self.path.clone();
        path.push(child_index.0);
        Ok(PageId { path })
    }

    /// Extract the Parent PageId given a PageId.
    ///
    /// If the provided PageId is the one pointing to the root,
    /// then itself is returned.
    pub fn parent_page_id(&self) -> Self {
        if *self == ROOT_PAGE_ID {
            return ROOT_PAGE_ID;
        }

        let mut path = self.path.clone();
        let _ = path.pop();
        PageId { path }
    }

    /// Whether this page is a descendant of the other.
    pub fn is_descendant_of(&self, other: &PageId) -> bool {
        self.path.starts_with(&other.path)
    }

    /// Get the maximum descendant of this page.
    pub fn max_descendant(&self) -> PageId {
        let mut page_id = self.clone();
        while page_id.path.len() < MAX_PAGE_DEPTH {
            page_id.path.push(MAX_CHILD_INDEX);
        }

        page_id
    }

    /// Get the minimum key-path which could land in this page.
    pub fn min_key_path(&self) -> KeyPath {
        let mut path = KeyPath::default();
        for (i, child_index) in self.path.iter().enumerate() {
            let bit_start = i * 6;
            let bit_end = bit_start + 6;
            let child_bits = &child_index.view_bits::<Msb0>()[2..8];
            path.view_bits_mut::<Msb0>()[bit_start..bit_end].copy_from_bitslice(child_bits);
        }

        for i in (6 * self.path.len())..256 {
            path.view_bits_mut::<Msb0>().set(i, false);
        }

        path
    }

    /// Get the maximum key-path which could land in this page.
    pub fn max_key_path(&self) -> KeyPath {
        let mut path = KeyPath::default();
        for (i, child_index) in self.path.iter().enumerate() {
            let bit_start = i * 6;
            let bit_end = bit_start + 6;
            let child_bits = &child_index.view_bits::<Msb0>()[2..8];
            path.view_bits_mut::<Msb0>()[bit_start..bit_end].copy_from_bitslice(child_bits);
        }

        for i in (6 * self.path.len())..256 {
            path.view_bits_mut::<Msb0>().set(i, true);
        }

        path
    }
}

/// The bytes cannot form a valid PageId because they define
/// a PageId bigger than the biggest valid one, the rightmost Page in the last layer.
#[derive(Debug, PartialEq)]
pub struct InvalidPageIdBytes;

/// Errors related to the construction of a Child PageId
#[derive(Debug, PartialEq)]
pub enum ChildPageIdError {
    /// PageId was at the last layer of the page tree
    /// or it was too big to represent a valid page
    PageIdOverflow,
}

/// Iterator of PageIds over a KeyPath,
/// PageIds will be lazily constructed as needed
pub struct PageIdsIterator {
    key_path: Uint<256, 4>,
    page_id: Option<PageId>,
}

impl PageIdsIterator {
    /// Create a PageIds Iterator over a KeyPath
    pub fn new(key_path: KeyPath) -> Self {
        Self {
            key_path: Uint::from_be_bytes(key_path),
            page_id: Some(ROOT_PAGE_ID),
        }
    }
}

impl Iterator for PageIdsIterator {
    type Item = PageId;

    fn next(&mut self) -> Option<Self::Item> {
        let prev = self.page_id.take()?;

        // unwrap: `new` can't return an error because the key_path is shifted.
        let child_index = ChildPageIndex::new(self.key_path.byte(31) >> 2).unwrap();
        self.key_path <<= 6;
        self.page_id = prev.child_page_id(child_index).ok();
        Some(prev)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChildPageIdError, ChildPageIndex, InvalidPageIdBytes, Msb0, PageId, PageIdsIterator, Uint,
        HIGHEST_ENCODED_42, MAX_CHILD_INDEX, ROOT_PAGE_ID,
    };
    use bitvec::prelude::*;

    const LOWEST_ENCODED_42: Uint<256, 4> = Uint::from_be_bytes([
        0, 65, 4, 16, 65, 4, 16, 65, 4, 16, 65, 4, 16, 65, 4, 16, 65, 4, 16, 65, 4, 16, 65, 4, 16,
        65, 4, 16, 65, 4, 16, 65,
    ]);

    fn child_page_id(page_id: &PageId, child_index: u8) -> Result<PageId, ChildPageIdError> {
        page_id.child_page_id(ChildPageIndex::new(child_index).unwrap())
    }

    #[test]
    fn test_child_and_parent_page_id() {
        let mut page_id_1 = [0u8; 32]; // child index 6
        page_id_1[31] = 0b00000111;
        let page_id_1 = PageId::decode(page_id_1).unwrap();

        assert_eq!(Ok(page_id_1.clone()), child_page_id(&ROOT_PAGE_ID, 6));
        assert_eq!(ROOT_PAGE_ID, page_id_1.parent_page_id());

        let mut page_id_2 = [0u8; 32]; // child index 4
        page_id_2[31] = 0b11000101;
        page_id_2[30] = 0b00000001;
        let page_id_2 = PageId::decode(page_id_2).unwrap();

        assert_eq!(Ok(page_id_2.clone()), child_page_id(&page_id_1, 4));
        assert_eq!(page_id_1, page_id_2.parent_page_id());

        let mut page_id_3 = [0u8; 32]; // child index 63
        page_id_3[31] = 0b10000000;
        page_id_3[30] = 0b01110001;
        let page_id_3 = PageId::decode(page_id_3).unwrap();

        assert_eq!(
            Ok(page_id_3.clone()),
            child_page_id(&page_id_2, MAX_CHILD_INDEX),
        );
        assert_eq!(page_id_2, page_id_3.parent_page_id());
    }

    #[test]
    fn test_page_ids_iterator() {
        // key_path = 0b000001|000010|0...
        let mut key_path = [0u8; 32];
        key_path[0] = 0b00000100;
        key_path[1] = 0b00100000;

        let mut page_id_1 = [0u8; 32];
        page_id_1[31] = 0b00000010; // 0b000001 + 1
        let page_id_1 = PageId::decode(page_id_1).unwrap();
        let mut page_id_2 = [0u8; 32];
        page_id_2[31] = 0b10000011; // (0b000001 + 1 << 6) + 0b000010 + 1
        let page_id_2 = PageId::decode(page_id_2).unwrap();

        let mut page_ids = PageIdsIterator::new(key_path);
        assert_eq!(page_ids.next(), Some(ROOT_PAGE_ID));
        assert_eq!(page_ids.next(), Some(page_id_1));
        assert_eq!(page_ids.next(), Some(page_id_2));

        // key_path = 0b000010|111111|0...
        let mut key_path = [0u8; 32];
        key_path[0] = 0b00001011;
        key_path[1] = 0b11110000;

        let mut page_id_1 = [0u8; 32];
        page_id_1[31] = 0b00000011; // 0b000010 + 1
        let page_id_1 = PageId::decode(page_id_1).unwrap();
        let mut page_id_2 = [0u8; 32];
        page_id_2[31] = 0b0000000;
        page_id_2[30] = 0b0000001; // (0b00000011 << 6) + 0b111111 + 1 = (0b00000011 + 1) << 6
        let page_id_2 = PageId::decode(page_id_2).unwrap();

        let mut page_ids = PageIdsIterator::new(key_path);
        assert_eq!(page_ids.next(), Some(ROOT_PAGE_ID));
        assert_eq!(page_ids.next(), Some(page_id_1));
        assert_eq!(page_ids.next(), Some(page_id_2));
    }

    #[test]
    fn invalid_child_index() {
        assert_eq!(None, ChildPageIndex::new(0b01010000));
        assert_eq!(None, ChildPageIndex::new(0b10000100));
        assert_eq!(None, ChildPageIndex::new(0b11000101));
    }

    #[test]
    fn test_invalid_page_id() {
        // position 255
        let mut page_id = [0u8; 32];
        page_id[0] = 128;
        assert_eq!(Err(InvalidPageIdBytes), PageId::decode(page_id));

        // position 252
        let mut page_id = [0u8; 32];
        page_id[0] = 128;
        assert_eq!(Err(InvalidPageIdBytes), PageId::decode(page_id));
    }

    #[test]
    fn test_page_id_overflow() {
        let first_page_last_layer = PageIdsIterator::new([0u8; 32]).last().unwrap();
        let last_page_last_layer = PageIdsIterator::new([255; 32]).last().unwrap();
        assert_eq!(
            Err(ChildPageIdError::PageIdOverflow),
            child_page_id(&first_page_last_layer, 0),
        );
        assert_eq!(
            Err(ChildPageIdError::PageIdOverflow),
            child_page_id(&last_page_last_layer, 0),
        );

        // position 255
        let page_id = PageId::decode(HIGHEST_ENCODED_42.to_be_bytes()).unwrap();
        assert_eq!(
            Err(ChildPageIdError::PageIdOverflow),
            child_page_id(&page_id, 0),
        );

        // any PageId bigger than LOWEST_42 must overflow
        let mut page_id = LOWEST_ENCODED_42.to_be_bytes();
        page_id[31] = 255;
        let page_id = PageId::decode(page_id).unwrap();
        assert_eq!(
            Err(ChildPageIdError::PageIdOverflow),
            child_page_id(&page_id, 0),
        );

        // position 245
        let mut page_id = [0u8; 32];
        page_id[1] = 32;
        let page_id = PageId::decode(page_id).unwrap();
        assert!(child_page_id(&page_id, 0).is_ok());

        // neither of those two have to panic if called at most 41 times
        let mut low = ROOT_PAGE_ID;
        let mut high = ROOT_PAGE_ID;
        for _ in 0..42 {
            low = child_page_id(&low, 0).unwrap();
            high = child_page_id(&high, MAX_CHILD_INDEX).unwrap();
        }
    }

    #[test]
    fn page_id_sibling_order() {
        let root_page = ROOT_PAGE_ID;
        let mut last_child = None;
        for i in 0..=MAX_CHILD_INDEX {
            let child = root_page.child_page_id(ChildPageIndex(i)).unwrap();
            assert!(child > root_page);

            if let Some(last) = last_child.take() {
                assert!(child > last);
            }
            last_child = Some(child);
        }
    }

    #[test]
    fn page_max_descendants_all_less_than_right_sibling() {
        let sibling_left = ROOT_PAGE_ID.child_page_id(ChildPageIndex(0)).unwrap();
        let sibling_right = ROOT_PAGE_ID.child_page_id(ChildPageIndex(1)).unwrap();

        let mut left_descendant = sibling_left.clone();
        loop {
            left_descendant = match left_descendant.child_page_id(ChildPageIndex(MAX_CHILD_INDEX)) {
                Err(_) => break,
                Ok(d) => d,
            };

            assert!(left_descendant < sibling_right);
        }
    }

    #[test]
    fn page_min_descendants_all_greater_than_left_sibling() {
        let sibling_left = ROOT_PAGE_ID.child_page_id(ChildPageIndex(0)).unwrap();
        let sibling_right = ROOT_PAGE_ID.child_page_id(ChildPageIndex(1)).unwrap();

        let mut right_descendant = sibling_right.clone();
        loop {
            right_descendant = match right_descendant.child_page_id(ChildPageIndex(0)) {
                Err(_) => break,
                Ok(d) => d,
            };

            assert!(right_descendant > sibling_left);
        }
    }

    #[test]
    fn root_min_key_path() {
        assert_eq!(ROOT_PAGE_ID.min_key_path(), [0; 32]);
    }

    #[test]
    fn root_max_key_path() {
        assert_eq!(ROOT_PAGE_ID.max_key_path(), [255; 32]);
    }

    #[test]
    fn page_min_key_path() {
        let min_page = ROOT_PAGE_ID.child_page_id(ChildPageIndex(0)).unwrap();
        let max_page = ROOT_PAGE_ID
            .child_page_id(ChildPageIndex(MAX_CHILD_INDEX))
            .unwrap();

        assert_eq!(min_page.min_key_path(), [0; 32]);
        let mut key_path = [0; 32];
        for i in 0..6 {
            key_path.view_bits_mut::<Msb0>().set(i, true);
        }
        assert_eq!(max_page.min_key_path(), key_path);
    }

    #[test]
    fn page_max_key_path() {
        let min_page = ROOT_PAGE_ID.child_page_id(ChildPageIndex(0)).unwrap();
        let max_page = ROOT_PAGE_ID
            .child_page_id(ChildPageIndex(MAX_CHILD_INDEX))
            .unwrap();

        assert_eq!(max_page.max_key_path(), [255; 32]);
        let mut key_path = [255; 32];
        for i in 0..6 {
            key_path.view_bits_mut::<Msb0>().set(i, false);
        }
        assert_eq!(min_page.max_key_path(), key_path);
    }
}
