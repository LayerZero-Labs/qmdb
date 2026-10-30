//! Trie update logic helpers.

use crate::hasher::NodeHasher;
use crate::trie::{self, KeyPath, LeafData, Node, ValueHash};

use bitvec::prelude::*;

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

// TODO: feels extremely out of place.
pub(crate) fn shared_bits(a: &BitSlice<u8, Msb0>, b: &BitSlice<u8, Msb0>) -> usize {
    a.iter().zip(b.iter()).take_while(|(a, b)| a == b).count()
}

/// Creates an iterator of all provided operations, with the leaf value spliced in if its key
/// does not appear in the original ops list. Then filters out all `None`s.
pub fn leaf_ops_spliced(
    leaf: Option<LeafData>,
    ops: &[(KeyPath, Option<ValueHash>)],
) -> impl Iterator<Item = (KeyPath, ValueHash)> + Clone + '_ {
    let splice_index = leaf
        .as_ref()
        .and_then(|leaf| ops.binary_search_by_key(&leaf.key_path, |x| x.0).err());
    let preserve_value = splice_index
        .zip(leaf)
        .map(|(_, leaf)| (leaf.key_path, Some(leaf.value_hash)));
    let splice_index = splice_index.unwrap_or(0);

    // splice: before / item / after
    // skip deleted.
    ops[..splice_index]
        .into_iter()
        .cloned()
        .chain(preserve_value)
        .chain(ops[splice_index..].into_iter().cloned())
        .filter_map(|(k, o)| o.map(move |value| (k, value)))
}

pub enum WriteNode<'a> {
    Leaf {
        up: bool,
        down: &'a BitSlice<u8, Msb0>,
        leaf_data: LeafData,
        node: Node,
    },
    Internal {
        internal_data: trie::InternalData,
        node: Node,
    },
    Terminator,
}

impl<'a> WriteNode<'a> {
    /// Whether to move up a step before writing the node.
    pub fn up(&self) -> bool {
        match self {
            WriteNode::Leaf { up, .. } => *up,
            WriteNode::Internal { .. } => true,
            WriteNode::Terminator => false,
        }
    }

    /// What path to follow down (after going up) before writing the node.
    pub fn down(&self) -> &BitSlice<u8, Msb0> {
        match self {
            WriteNode::Leaf { down, .. } => down,
            _ => BitSlice::empty(),
        }
    }

    /// The node itself.
    pub fn node(&self) -> Node {
        match self {
            WriteNode::Leaf { node, .. } => *node,
            WriteNode::Internal { node, .. } => *node,
            WriteNode::Terminator => trie::TERMINATOR,
        }
    }
}

// Build a trie out of the given prior terminal and operations. Operations should all start
// with the same prefix of len `skip` and be ordered lexicographically. The root node of the
// generated trie is the one residing at path `prefix[..skip]`. When skip=0, this is the actual
// root.
//
// Provide a visitor which will be called for each computed node of the trie.
//
// The visitor is assumed to have a default position at the root of the trie and from
// there will be controlled with `WriteNode`. The changes to the position before writing the node
// can be extracted from the command.
// The root is always visited at the end. If the written node is a leaf, the leaf-data preimage
// will be provided.
pub fn build_trie<H: NodeHasher>(
    skip: usize,
    ops: impl IntoIterator<Item = (KeyPath, ValueHash)>,
    mut visit: impl FnMut(WriteNode),
) -> Node {
    // we build a compact addressable sub-trie in-place based on the given set of ordered keys,
    // ignoring deletions as they are implicit in a fresh sub-trie.
    //
    // an algorithm for building the compact sub-trie follows:
    //
    // consider any three leaves, A, B, C in sorted order by key, with different keys.
    // A and B have some number of shared bits n1
    // B and C have some number of shared bits n2
    //
    // We can make an accurate statement about the position of B regardless of what other material
    // appears in the trie, as long as there is no A' s.t. A < A' < B and no C' s.t. B < C' < C.
    //
    // A is a leaf somewhere to the left of B, which is in turn somewhere to the left of C
    // A and B share an internal node at depth n1, while B and C share an internal node at depth n2.
    // n1 cannot equal n2, as there are only 2 keys with shared prefix n and a != b != c.
    // If n1 is less than n2, then B is a leaf at depth n2+1 along its path (always left)
    // If n2 is less than n1, then B is a leaf at depth n1+1 along its path (always right)
    // QED
    //
    // A similar process applies to the first leaf in the list: it is a leaf on the left of an
    // internal node at depth n, where n is the number of shared bits with the following key.
    //
    // Same for the last leaf in the list: it is on the right of an internal node at depth n,
    // where n is the number of shared bits with the previous key.
    //
    // If the list has a single item, the sub-trie is a single leaf.
    // And if the list is empty, the sub-trie is a terminator.

    // A left-frontier: all modified nodes are to the left of
    // `b`, so this stores their layers.
    let mut pending_siblings: Vec<(Node, usize)> = Vec::new();

    let mut leaf_ops = ops.into_iter();

    let mut a = None;
    let mut b = leaf_ops.next();
    let mut c = leaf_ops.next();

    match (b, c) {
        (None, _) => {
            // fast path: delete single node.
            visit(WriteNode::Terminator);
            return trie::TERMINATOR;
        }
        (Some((ref k, ref v)), None) => {
            // fast path: place single leaf.
            let leaf_data = trie::LeafData {
                key_path: *k,
                value_hash: *v,
            };
            let leaf = H::hash_leaf(&leaf_data);
            visit(WriteNode::Leaf {
                up: false,
                down: BitSlice::empty(),
                leaf_data,
                node: leaf,
            });

            return leaf;
        }
        _ => {}
    }

    let common_after_prefix = |k1: &KeyPath, k2: &KeyPath| {
        let x = &k1.view_bits::<Msb0>()[skip..];
        let y = &k2.view_bits::<Msb0>()[skip..];
        shared_bits(x, y)
    };

    while let Some((this_key, this_val)) = b {
        let n1 = a.as_ref().map(|(k, _)| common_after_prefix(k, &this_key));
        let n2 = c.as_ref().map(|(k, _)| common_after_prefix(k, &this_key));

        let leaf_data = trie::LeafData {
            key_path: this_key,
            value_hash: this_val,
        };
        let leaf = H::hash_leaf(&leaf_data);
        let (leaf_depth, hash_up_layers) = match (n1, n2) {
            (None, None) => {
                // single value - no hashing required.
                (0, 0)
            }
            (None, Some(n2)) => {
                // first value, n2 ancestor will be affected by next.
                (n2 + 1, 0)
            }
            (Some(n1), None) => {
                // last value, hash up to sub-trie root.
                (n1 + 1, n1 + 1)
            }
            (Some(n1), Some(n2)) => {
                // middle value, hash up to incoming ancestor + 1.
                (core::cmp::max(n1, n2) + 1, n1.saturating_sub(n2))
            }
        };

        let mut layer = leaf_depth;
        let mut last_node = leaf;
        let down_start = skip + n1.unwrap_or(0);
        let leaf_end_bit = skip + leaf_depth;

        visit(WriteNode::Leaf {
            up: n1.is_some(), // previous iterations always get to current layer + 1
            down: &this_key.view_bits::<Msb0>()[down_start..leaf_end_bit],
            node: leaf,
            leaf_data,
        });

        for bit in this_key.view_bits::<Msb0>()[skip..leaf_end_bit]
            .iter()
            .by_vals()
            .rev()
            .take(hash_up_layers)
        {
            layer -= 1;

            let sibling = if pending_siblings.last().map_or(false, |l| l.1 == layer + 1) {
                // unwrap: just checked
                pending_siblings.pop().unwrap().0
            } else {
                trie::TERMINATOR
            };

            let internal_data = if bit {
                trie::InternalData {
                    left: sibling,
                    right: last_node,
                }
            } else {
                trie::InternalData {
                    left: last_node,
                    right: sibling,
                }
            };

            last_node = H::hash_internal(&internal_data);
            visit(WriteNode::Internal {
                internal_data,
                node: last_node,
            });
        }
        pending_siblings.push((last_node, layer));

        a = Some((this_key, this_val));
        b = c;
        c = leaf_ops.next();
    }

    let new_root = pending_siblings
        .pop()
        .map(|n| n.0)
        .unwrap_or(trie::TERMINATOR);
    new_root
}

#[cfg(test)]
mod tests {
    use crate::trie::{NodeKind, TERMINATOR};

    use super::{bitvec, build_trie, trie, BitVec, LeafData, Msb0, Node, NodeHasher, WriteNode};

    struct DummyNodeHasher;

    impl NodeHasher for DummyNodeHasher {
        fn hash_leaf(data: &trie::LeafData) -> [u8; 32] {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&data.key_path);
            hasher.update(&data.value_hash);
            let mut hash: [u8; 32] = hasher.finalize().into();

            // Label with MSB
            hash[0] |= 0b10000000;
            hash
        }

        fn hash_internal(data: &trie::InternalData) -> [u8; 32] {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&data.left);
            hasher.update(&data.right);
            let mut hash: [u8; 32] = hasher.finalize().into();

            // Label with MSB
            hash[0] &= 0b01111111;
            hash
        }

        fn node_kind(node: &Node) -> NodeKind {
            if node[0] >> 7 == 1 {
                NodeKind::Leaf
            } else if node == &TERMINATOR {
                NodeKind::Terminator
            } else {
                NodeKind::Internal
            }
        }
    }

    fn leaf(key: u8) -> (LeafData, [u8; 32]) {
        let key = [key; 32];
        let leaf = trie::LeafData {
            key_path: key.clone(),
            value_hash: key.clone(),
        };

        let hash = DummyNodeHasher::hash_leaf(&leaf);
        (leaf, hash)
    }

    fn branch_hash(left: [u8; 32], right: [u8; 32]) -> [u8; 32] {
        let data = trie::InternalData { left, right };

        let hash = DummyNodeHasher::hash_internal(&data);
        hash
    }

    #[derive(Default)]
    struct Visited {
        key: BitVec<u8, Msb0>,
        visited: Vec<(BitVec<u8, Msb0>, Node)>,
    }

    impl Visited {
        fn at(key: BitVec<u8, Msb0>) -> Self {
            Visited {
                key,
                visited: Vec::new(),
            }
        }

        fn visit(&mut self, control: WriteNode) {
            let n = self.key.len() - control.up() as usize;
            self.key.truncate(n);
            self.key.extend_from_bitslice(control.down());
            self.visited.push((self.key.clone(), control.node()));
        }
    }

    #[test]
    fn build_empty_trie() {
        let mut visited = Visited::default();
        let root = build_trie::<DummyNodeHasher>(0, vec![], |control| visited.visit(control));

        let visited = visited.visited;

        assert_eq!(visited, vec![(bitvec![u8, Msb0;], [0u8; 32]),],);

        assert_eq!(root, [0u8; 32]);
    }

    #[test]
    fn build_single_value_trie() {
        let mut visited = Visited::default();

        let (leaf, leaf_hash) = leaf(0xff);
        let root =
            build_trie::<DummyNodeHasher>(0, vec![(leaf.key_path, leaf.value_hash)], |control| {
                visited.visit(control)
            });

        let visited = visited.visited;

        assert_eq!(visited, vec![(bitvec![u8, Msb0;], leaf_hash),],);

        assert_eq!(root, leaf_hash);
    }

    #[test]
    fn sub_trie() {
        let (leaf_a, leaf_hash_a) = leaf(0b0001_0001);
        let (leaf_b, leaf_hash_b) = leaf(0b0001_0010);
        let (leaf_c, leaf_hash_c) = leaf(0b0001_0100);

        let mut visited = Visited::at(bitvec![u8, Msb0; 0, 0, 0, 1]);

        let ops = [leaf_a, leaf_b, leaf_c]
            .iter()
            .map(|l| (l.key_path, l.value_hash))
            .collect::<Vec<_>>();

        let root = build_trie::<DummyNodeHasher>(4, ops, |control| visited.visit(control));

        let visited = visited.visited;

        let branch_ab_hash = branch_hash(leaf_hash_a, leaf_hash_b);
        let branch_abc_hash = branch_hash(branch_ab_hash, leaf_hash_c);
        let root_branch_hash = branch_hash(branch_abc_hash, [0u8; 32]);

        assert_eq!(
            visited,
            vec![
                (bitvec![u8, Msb0; 0, 0, 0, 1, 0, 0, 0], leaf_hash_a),
                (bitvec![u8, Msb0; 0, 0, 0, 1, 0, 0, 1], leaf_hash_b),
                (bitvec![u8, Msb0; 0, 0, 0, 1, 0, 0], branch_ab_hash),
                (bitvec![u8, Msb0; 0, 0, 0, 1, 0, 1], leaf_hash_c),
                (bitvec![u8, Msb0; 0, 0, 0, 1, 0], branch_abc_hash),
                (bitvec![u8, Msb0; 0, 0, 0, 1], root_branch_hash),
            ],
        );

        assert_eq!(root, root_branch_hash);
    }

    #[test]
    fn multi_value() {
        let (leaf_a, leaf_hash_a) = leaf(0b0001_0000);
        let (leaf_b, leaf_hash_b) = leaf(0b0010_0000);
        let (leaf_c, leaf_hash_c) = leaf(0b0100_0000);
        let (leaf_d, leaf_hash_d) = leaf(0b1010_0000);
        let (leaf_e, leaf_hash_e) = leaf(0b1011_0000);

        let mut visited = Visited::default();

        let ops = [leaf_a, leaf_b, leaf_c, leaf_d, leaf_e]
            .iter()
            .map(|l| (l.key_path, l.value_hash))
            .collect::<Vec<_>>();

        let root = build_trie::<DummyNodeHasher>(0, ops, |control| visited.visit(control));

        let visited = visited.visited;

        let branch_ab_hash = branch_hash(leaf_hash_a, leaf_hash_b);
        let branch_abc_hash = branch_hash(branch_ab_hash, leaf_hash_c);

        let branch_de_hash_1 = branch_hash(leaf_hash_d, leaf_hash_e);
        let branch_de_hash_2 = branch_hash([0u8; 32], branch_de_hash_1);
        let branch_de_hash_3 = branch_hash(branch_de_hash_2, [0u8; 32]);

        let branch_abc_de_hash = branch_hash(branch_abc_hash, branch_de_hash_3);

        assert_eq!(
            visited,
            vec![
                (bitvec![u8, Msb0; 0, 0, 0], leaf_hash_a),
                (bitvec![u8, Msb0; 0, 0, 1], leaf_hash_b),
                (bitvec![u8, Msb0; 0, 0], branch_ab_hash),
                (bitvec![u8, Msb0; 0, 1], leaf_hash_c),
                (bitvec![u8, Msb0; 0], branch_abc_hash),
                (bitvec![u8, Msb0; 1, 0, 1, 0], leaf_hash_d),
                (bitvec![u8, Msb0; 1, 0, 1, 1], leaf_hash_e),
                (bitvec![u8, Msb0; 1, 0, 1], branch_de_hash_1),
                (bitvec![u8, Msb0; 1, 0], branch_de_hash_2),
                (bitvec![u8, Msb0; 1], branch_de_hash_3),
                (bitvec![u8, Msb0;], branch_abc_de_hash),
            ],
        );

        assert_eq!(root, branch_abc_de_hash);
    }
}
