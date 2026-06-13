use crate::{
    page::DEPTH,
    page_id::{ChildPageIndex, PageId, ROOT_PAGE_ID},
    trie::KeyPath,
};
use alloc::fmt;
use bitvec::prelude::*;

/// Encapsulates logic for moving around in paged storage for a binary trie.
#[derive(Clone)]
#[cfg_attr(
    feature = "borsh",
    derive(borsh::BorshDeserialize, borsh::BorshSerialize)
)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TriePosition {
    // The bits after depth are irrelevant.
    path: [u8; 32],
    depth: u16,
    node_index: usize,
}

impl PartialEq for TriePosition {
    fn eq(&self, other: &Self) -> bool {
        self.path() == other.path()
    }
}

impl Eq for TriePosition {}

impl TriePosition {
    /// Create a new `TriePosition` at the root.
    pub fn new() -> Self {
        TriePosition {
            path: [0; 32],
            depth: 0,
            node_index: 0,
        }
    }

    /// Create a new `TriePosition` based on the first `depth` bits of `path`.
    ///
    /// Panics if depth is zero.
    pub fn from_path_and_depth(path: KeyPath, depth: u16) -> Self {
        assert_ne!(depth, 0, "depth must be non-zero");
        assert!(depth <= 256);
        let page_path = last_page_path(&path, depth);
        TriePosition {
            path,
            depth,
            node_index: node_index(&page_path),
        }
    }

    /// Create a new `TriePosition` based on a bitslice.
    pub fn from_bitslice(slice: &BitSlice<u8, Msb0>) -> Self {
        assert!(slice.len() <= 256);

        let mut path = [0; 32];
        path.view_bits_mut::<Msb0>()[..slice.len()].copy_from_bitslice(slice);
        Self::from_path_and_depth(path, slice.len() as u16)
    }

    /// Parse a `TriePosition` from a bit string.
    #[cfg(test)]
    pub fn from_str(s: &str) -> Self {
        let mut bitvec = BitVec::<u8, Msb0>::new();
        if s.len() > 256 {
            panic!("bit string too long");
        }
        for ch in s.chars() {
            match ch {
                '0' => bitvec.push(false),
                '1' => bitvec.push(true),
                _ => panic!("invalid character in bit string"),
            }
        }
        let node_index = node_index(&bitvec);
        let depth = bitvec.len() as u16;
        bitvec.resize(256, false);
        // Unwrap: resized to 256 bit, or 32 bytes, above.
        let path = bitvec.as_raw_slice().try_into().unwrap();
        Self {
            path,
            depth,
            node_index,
        }
    }

    /// Whether the position is at the root.
    pub fn is_root(&self) -> bool {
        self.depth == 0
    }

    /// Get the current `depth` of the position.
    pub fn depth(&self) -> u16 {
        self.depth
    }

    /// Get the path to the current position.
    pub fn path(&self) -> &BitSlice<u8, Msb0> {
        &self.path.view_bits::<Msb0>()[..self.depth as usize]
    }

    /// Get the raw key at the current position.
    ///
    /// Note that if you have called `up`, this might have bits beyond `depth` which are set.
    pub fn raw_path(&self) -> [u8; 32] {
        self.path
    }

    /// Move the position down by 1, towards either the left or right child.
    ///
    /// Panics on depth out of range.
    pub fn down(&mut self, bit: bool) {
        assert_ne!(self.depth, 256, "can't descend past 256 bits");
        if self.depth as usize % DEPTH == 0 {
            self.node_index = bit as usize;
        } else {
            let children = self.child_node_indices();
            self.node_index = if bit {
                children.right()
            } else {
                children.left()
            };
        }
        self.path
            .view_bits_mut::<Msb0>()
            .set(self.depth as usize, bit);
        self.depth += 1;
    }

    /// Move the position up by `d` bits.
    ///
    /// Panics if `d` is greater than the current depth.
    pub fn up(&mut self, d: u16) {
        let prev_depth = self.depth;
        let Some(new_depth) = self.depth.checked_sub(d) else {
            panic!("can't move up by {} bits from depth {}", d, prev_depth)
        };
        if new_depth == 0 {
            *self = TriePosition::new();
            return;
        }

        self.depth = new_depth;
        let prev_page_depth = (prev_depth as usize + DEPTH - 1) / DEPTH;
        let new_page_depth = (self.depth as usize + DEPTH - 1) / DEPTH;
        if prev_page_depth == new_page_depth {
            for _ in 0..d {
                self.node_index = parent_node_index(self.node_index);
            }
        } else {
            let path = last_page_path(&self.path, self.depth);
            self.node_index = node_index(path);
        }
    }

    /// Move the position to the sibling node.
    ///
    /// Panic if at the root.
    pub fn sibling(&mut self) {
        assert_ne!(self.depth, 0, "can't move to sibling of root node");
        let bits = self.path.view_bits_mut::<Msb0>();
        let i = self.depth as usize - 1;
        bits.set(i, !bits[i]);
        self.node_index = sibling_index(self.node_index);
    }

    /// Peek at the last bit of the path.
    ///
    /// Panics if at the root.
    pub fn peek_last_bit(&self) -> bool {
        assert_ne!(self.depth, 0, "can't peek at root node");
        let this_bit_idx = self.depth as usize - 1;
        // unwrap: depth != 0 above
        let bit = *self.path.view_bits::<Msb0>().get(this_bit_idx).unwrap();
        bit
    }

    /// Get the page ID this position lands in. Returns `None` at the root.
    pub fn page_id(&self) -> Option<PageId> {
        if self.is_root() {
            return None;
        }

        let mut page_id = ROOT_PAGE_ID;
        for (i, chunk) in self.path().chunks_exact(DEPTH).enumerate() {
            if (i + 1) * DEPTH == self.depth as usize {
                return Some(page_id);
            }

            // UNWRAP: 6 bits never overflows child page index
            let child_index = ChildPageIndex::new(chunk.load_be::<u8>()).unwrap();

            // UNWRAP: trie position never overflows page tree.
            page_id = page_id.child_page_id(child_index).unwrap();
        }

        Some(page_id)
    }

    /// Get the child page index, relative to the current page,
    /// where the children of the current node are stored.
    ///
    /// Panics if the position is not in the last layer of the page.
    pub fn child_page_index(&self) -> ChildPageIndex {
        assert!(self.node_index >= 62);
        ChildPageIndex::new(bottom_node_index(self.node_index)).unwrap()
    }

    /// Get the child page index, relative to the current page,
    /// where the children of the sibling node are stored.
    ///
    /// Panics if the position is not in the last layer of the page.
    pub fn sibling_child_page_index(&self) -> ChildPageIndex {
        ChildPageIndex::new(bottom_node_index(sibling_index(self.node_index))).unwrap()
    }

    /// Transform a bit-path to the index in a page corresponding to the child node indices.
    ///
    /// Panics if the node is not at a depth in the range 1..=5
    pub fn child_node_indices(&self) -> ChildNodeIndices {
        let depth = self.depth_in_page();
        if depth == 0 || depth > DEPTH - 1 {
            panic!("{depth} out of bounds 1..={}", DEPTH - 1);
        }
        let left = self.node_index * 2 + 2;
        ChildNodeIndices(left)
    }

    /// Get the index of the sibling node within a page.
    pub fn sibling_index(&self) -> usize {
        sibling_index(self.node_index)
    }

    /// Get the index of the current node within a page.
    pub fn node_index(&self) -> usize {
        self.node_index
    }

    /// Get the number of bits traversed in the current page.
    ///
    /// Note that every page has traversed at least 1 bit, therefore the return value would be
    /// between 1 and `DEPTH`, with the exception of the root node, which has traversed 0 bits.
    pub fn depth_in_page(&self) -> usize {
        if self.depth == 0 {
            0
        } else {
            self.depth as usize - ((self.depth as usize - 1) / DEPTH) * DEPTH
        }
    }

    /// Fast path for checking whether this is in the first layer in the page.
    pub fn is_first_layer_in_page(&self) -> bool {
        self.node_index & !1 == 0
    }

    /// Get the number of shared bits between this position and `other`.
    ///
    /// This is essentially the depth of a hypothetical internal node which both positions would
    /// descend from.
    pub fn shared_depth(&self, other: &Self) -> usize {
        crate::update::shared_bits(self.path(), other.path())
    }

    /// Whether the sub-trie indicated by this position would contain
    /// a given key-path.
    pub fn subtrie_contains(&self, path: &crate::trie::KeyPath) -> bool {
        path.view_bits::<Msb0>()
            .starts_with(&self.path.view_bits::<Msb0>()[..self.depth as usize])
    }
}

// extract the relevant portion of the key path to the last page. panics on empty path.
fn last_page_path(path: &[u8; 32], depth: u16) -> &BitSlice<u8, Msb0> {
    if depth == 0 {
        panic!();
    }
    let prev_page_end = ((depth as usize - 1) / DEPTH) * DEPTH;
    &path.view_bits::<Msb0>()[prev_page_end..depth as usize]
}

// Transform a bit-path to an index in a page.
//
// The expected length of the page path is between 1 and `DEPTH`, inclusive. A length of 0 returns
// 0 and all bits beyond `DEPTH` are ignored.
fn node_index(page_path: &BitSlice<u8, Msb0>) -> usize {
    let depth = core::cmp::min(DEPTH, page_path.len());

    if depth == 0 {
        0
    } else {
        // each node is stored at (2^depth - 2) + as_uint(path)
        (1 << depth) - 2 + page_path[..depth].load_be::<usize>()
    }
}

fn bottom_node_index(node_index: usize) -> u8 {
    node_index as u8 - 62
}

/// Given a node index, get the index of the sibling.
fn sibling_index(node_index: usize) -> usize {
    if node_index % 2 == 0 {
        node_index + 1
    } else {
        node_index - 1
    }
}

// Transform a node index to the index where the parent node is stored
// Id does not check for an overflow of the maximum valid node index
// and panics if the provided node_index is one of the first two
// nodes in a page, thus node_index 0 or 1
fn parent_node_index(node_index: usize) -> usize {
    (node_index - 2) / 2
}

impl fmt::Debug for TriePosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.depth == 0 {
            write!(f, "TriePosition(root)")
        } else {
            write!(f, "TriePosition({})", self.path(),)
        }
    }
}

/// A helper type representing two child node indices within a page.
#[derive(Debug, Clone, Copy)]
pub struct ChildNodeIndices(usize);

impl ChildNodeIndices {
    /// Create from a left child index.
    pub fn from_left(left: usize) -> Self {
        ChildNodeIndices(left)
    }

    /// Whether these are at the top of a page.
    pub fn in_next_page(&self) -> bool {
        self.0 == 0
    }

    /// Get the index of the left child.
    pub fn left(&self) -> usize {
        self.0
    }
    /// Get the index of the right child.
    pub fn right(&self) -> usize {
        self.0 + 1
    }
}

#[cfg(test)]
mod tests {
    use super::TriePosition;

    #[test]
    fn path_can_go_deeper_255_bit() {
        let mut p = TriePosition::from_str(
            "1010101010101010101010101010101010101010101010101010101010101010\
            1010101010101010101010101010101010101010101010101010101010101010\
            1010101010101010101010101010101010101010101010101010101010101010\
            101010101010101010101010101010101010101010101010101010101010101",
        );
        assert_eq!(p.depth as usize, 255);
        p.down(false);
    }
}
