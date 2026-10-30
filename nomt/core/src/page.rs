//! Pages: efficient node storage.
//!
//! Because each node in the trie is exactly 32 bytes, we can easily pack groups of nodes into
//! a predictable paged representation regardless of the information in the trie.
//!
//! Each page is 4096 bytes and stores up to 126 nodes plus a unique 32-byte page identifier,
//! with 32 bytes left over.
//!
//! A page stores a rootless sub-tree with depth 6: that is, it stores up to
//! 2 + 4 + 8 + 16 + 32 + 64 nodes at known positions.
//! Semantically, all nodes within the page should descend from the layer above, and the
//! top two nodes are expected to be siblings. Each page logically has up to 64 child pages, which
//! correspond to the rootless sub-tree descending from each of the 64 child nodes on the bottom
//! layer.
//!
//! Every page is referred to by a unique ID, given by `parent_id * 2^6 + child_index + 1`, where
//! the root page has ID `0x00..00`. The child index ranges from 0 to 63 and therefore can be
//! represented as a 6 bit string. This module exposes functions for manipulating page IDs.
//!
//! The [`RawPage`] structure wraps a borrowed slice of 32-byte data and treats it as a page.

/// Depth of the rootless sub-binary tree stored in a page
pub const DEPTH: usize = 6;

// Total number of nodes stored in one Page. It depends on the `DEPTH`
// of the rootless sub-binary tree stored in a page following this formula:
// (2^(DEPTH + 1)) - 2
pub const NODES_PER_PAGE: usize = (1 << DEPTH + 1) - 2;

/// A raw, unsized page data slice.
pub type RawPage = [[u8; 32]];
