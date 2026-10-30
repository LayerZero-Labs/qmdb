//! This module defines the types of a binary merkle trie, generalized over a 256 bit hash function.
//! All lookup paths in the trie are 256 bits.
//!
//! All nodes are 256 bits. There are three kinds of nodes.
//!   1. Internal nodes, which each have two children. The value of an internal node is
//!       given by hashing the concatenation of the two child nodes and setting the MSB to 0.
//!   2. Leaf nodes, which have zero children. The value of a leaf node is given by hashing
//!       the concatenation of the 256-bit lookup path and the hash of the value stored at the leaf,
//!       and setting the MSB to 1.
//!   3. [`TERMINATOR`] nodes, which have the special value of all 0s. These nodes have no children
//!      and serve as a stand-in for an empty sub-trie at any height. Terminator nodes enable the
//!      trie to be tractably represented.
//!
//! All node preimages are 512 bits.

use crate::hasher::NodeHasher;

/// A node in the binary trie. In this schema, it is always 256 bits and is the hash of either
/// an [`LeafData`] or [`InternalData`], or zeroed if it's a [`TERMINATOR`].
///
/// [`Node`]s are labeled by the [`NodeHasher`] used to indicate whether they are leaves or internal
/// nodes. Typically, this is done by setting the MSB.
pub type Node = [u8; 32];

/// The path to a key. All paths have a 256 bit fixed length.
pub type KeyPath = [u8; 32];

/// The hash of a value. In this schema, it is always 256 bits.
pub type ValueHash = [u8; 32];

/// The terminator hash is a special node hash value denoting an empty sub-tree.
/// Concretely, when this appears at a given location in the trie,
/// it implies that no key with a path beginning with the location has a value.
///
/// This value may appear at any height.
pub const TERMINATOR: Node = [0u8; 32];

/// Whether the node hash indicates the node is a leaf.
pub fn is_leaf<H: NodeHasher>(hash: &Node) -> bool {
    H::node_kind(hash) == NodeKind::Leaf
}

/// Whether the node hash indicates the node is an internal node.
pub fn is_internal<H: NodeHasher>(hash: &Node) -> bool {
    H::node_kind(hash) == NodeKind::Internal
}

/// Whether the node holds the special `EMPTY_SUBTREE` value.
pub fn is_terminator<H: NodeHasher>(hash: &Node) -> bool {
    H::node_kind(hash) == NodeKind::Terminator
}

/// The kind of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// A terminator node indicates an empty sub-trie.
    Terminator,
    /// A leaf node indicates a sub-trie with a single leaf.
    Leaf,
    /// An internal node indicates at least two values.
    Internal,
}

impl NodeKind {
    /// Get the kind of the provided node.
    pub fn of<H: NodeHasher>(node: &Node) -> Self {
        H::node_kind(node)
    }
}

/// The data of an internal (branch) node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalData {
    /// The hash of the left child of this node.
    pub left: Node,
    /// The hash of the right child of this node.
    pub right: Node,
}

/// The data of a leaf node.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "borsh",
    derive(borsh::BorshDeserialize, borsh::BorshSerialize)
)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LeafData {
    /// The total path to this value within the trie.
    ///
    /// The actual location of this node may be anywhere along this path, depending on the other
    /// data within the trie.
    pub key_path: KeyPath,
    /// The hash of the value carried in this leaf.
    pub value_hash: ValueHash,
}
