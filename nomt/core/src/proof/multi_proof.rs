//! Generate a multiproof from a vector of path proofs.
//! The multiproof will contain the minimum information needed to verify
//! the inclusion of all provided path proofs.

use crate::{
    hasher::NodeHasher,
    proof::{
        path_proof::{hash_path, shared_bits},
        KeyOutOfScope, PathProof, PathProofTerminal,
    },
    trie::{InternalData, KeyPath, LeafData, Node, NodeKind, ValueHash, TERMINATOR},
};

#[cfg(not(feature = "std"))]
use alloc::{vec, vec::Vec};

use bitvec::prelude::*;
use core::{cmp::Ordering, ops::Range};

/// This struct includes the terminal node and its depth
#[derive(Debug, Clone)]
#[cfg_attr(
    feature = "borsh",
    derive(borsh::BorshDeserialize, borsh::BorshSerialize)
)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MultiPathProof {
    /// Terminal node
    pub terminal: PathProofTerminal,
    /// Depth of the terminal node
    pub depth: usize,
}

/// A proof of multiple paths through the trie.
#[derive(Debug, Clone)]
#[cfg_attr(
    feature = "borsh",
    derive(borsh::BorshDeserialize, borsh::BorshSerialize)
)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MultiProof {
    /// List of all provable paths. These are sorted in ascending order by bit-path
    pub paths: Vec<MultiPathProof>,
    /// Vector containing the minimum number of nodes required
    /// to reconstruct all other nodes later.
    ///
    /// The format is a recursive bisection:
    /// [upper_siblings ++ left_siblings ++ right_siblings]
    ///
    /// upper_siblings could be:
    /// + siblings shared among all paths in each bisection
    /// + unique siblings associated with a terminal node
    ///
    /// In the latter case, both left and right bisections will be empty
    ///
    /// left_siblings is the same format, but applied to all the nodes in the left bisection.
    /// right_siblings is the same format, but applied to all the nodes in the right bisection.
    pub siblings: Vec<Node>,
}

// Given a vector of PathProofs ordered by the key_path,
// `PathProofRange` represent a range of that vector
// with key_paths that have common bits up to path_bit_index
struct PathProofRange {
    // lower bound of the range
    lower: usize,
    // upper bound of the range
    upper: usize,
    // bit index in the key path where this struct is pointing at,
    // this index will be increased or used to create a new bisection
    path_bit_index: usize,
}

enum PathProofRangeStep {
    Bisect {
        left: PathProofRange,
        right: PathProofRange,
    },
    Advance {
        sibling: Node,
    },
}

impl PathProofRange {
    fn prove_unique_path_remainder(
        &self,
        path_proofs: &[PathProof],
    ) -> Option<(MultiPathProof, Vec<Node>)> {
        // If PathProofRange contains only one path_proofs
        // return a MultiPathProof with all its unique siblings
        if self.lower != self.upper - 1 {
            return None;
        }

        let path_proof = &path_proofs[self.lower];
        let unique_siblings: Vec<Node> = path_proof
            .siblings
            .iter()
            .skip(self.path_bit_index)
            .copied()
            .collect();

        Some((
            MultiPathProof {
                terminal: path_proof.terminal.clone(),
                depth: self.path_bit_index + unique_siblings.len(),
            },
            unique_siblings,
        ))
    }

    fn step(&mut self, path_proofs: &[PathProof]) -> PathProofRangeStep {
        // check if at least two key_path in the path_proofs range
        // has two different bits in position path_bit_index
        //
        // they are ordered and all the bits up to path_bit_index
        // are shared so we can just check if the first and the last one differs
        let path_lower = path_proofs[self.lower].terminal.path();
        let path_upper = path_proofs[self.upper - 1].terminal.path();

        if path_lower[self.path_bit_index] != path_upper[self.path_bit_index] {
            // if they differ we can skip their siblings but we need to bisect the slice
            //
            // binary search between key_paths in the slice to see where to
            // perform the bisection
            //
            // UNWRAP: We have just checked that path_lower and path_upper differ at path_bit_index.
            // Therefore, with the vector of path_proofs ordered, we can be sure that there is at least
            // one path with its key_path containing a value of 1 at path_bit_index.
            // Since there is at least one 1 bit and Ordering::Equal is never returned,
            // the method binary_search_by will always return an Error containing the index
            // of the first occurrence of one in the key_path.
            let mid = self.lower
                + path_proofs[self.lower..self.upper]
                    .binary_search_by(|path_proof| {
                        if !path_proof.terminal.path()[self.path_bit_index] {
                            core::cmp::Ordering::Less
                        } else {
                            core::cmp::Ordering::Greater
                        }
                    })
                    .unwrap_err();

            let left = PathProofRange {
                path_bit_index: self.path_bit_index + 1,
                lower: self.lower,
                upper: mid,
            };

            let right = PathProofRange {
                path_bit_index: self.path_bit_index + 1,
                lower: mid,
                upper: self.upper,
            };

            PathProofRangeStep::Bisect { left, right }
        } else {
            // if they don't differ, we need their sibling
            let sibling = path_proofs[self.lower].siblings[self.path_bit_index];
            self.path_bit_index += 1;
            PathProofRangeStep::Advance { sibling }
        }
    }
}

impl MultiProof {
    /// Construct a MultiProof from a vector of *ordered* PathProof.
    ///
    /// Note that the path proofs produced within a [`crate::witness::Witness`] are not guaranteed
    /// to be ordered, so the input should be sorted lexicographically by the terminal path prior
    /// to calling this function.
    pub fn from_path_proofs(path_proofs: Vec<PathProof>) -> Self {
        // A multi-proof can be viewed by associating each terminal node
        // with its first n uniquely related siblings from its path proof,
        // followed by all necessary siblings that are not derivable from
        // the previously mentioned siblings.

        // The goal is to traverse the entire tree
        // formed by all path proofs and only collect the siblings
        // that cannot be reconstructed in the future
        //
        // The traversal does not occur on the siblings themselves,
        // but on the ordered set of key_paths within PathProofs.
        //
        // For example, take two key_paths starting from the first bit.
        // If two keys share bits up to index i, they will have the same sibling at index i.
        // If the bit at index i differs, their paths diverge, and no sibling is needed at that index
        // because it can be reconstructed from siblings collected later in the other key.
        // When a differing bit is found, each key_path needs separate scanning.
        //
        // When there is a single key_path, all siblings are necessary.
        //
        // When there are more than two key_paths, the same logic is applied,
        // but if two key_paths have different bits at an index, the key_paths are split
        // based on the value at that index.
        //
        // If all key_paths share a bit at an index, that sibling is required and it is one
        // of the siblings mentioned earlier.
        // If at least two key_paths differ, no sibling is needed, but the key_paths must be divided
        // based on having bit 0 or 1 at that index.
        //
        // Iterate this algorithm on the bisection to determine the minimum necessary siblings.
        //
        // `siblings` will follow this structure for each bisection
        // |common siblings| ext siblings in the left bisection | ext siblings in the right bisection |

        if path_proofs.is_empty() {
            return MultiProof {
                paths: Vec::new(),
                siblings: Vec::new(),
            };
        }

        let mut paths: Vec<MultiPathProof> = vec![];
        let mut siblings: Vec<Node> = vec![];

        // initially we're looking at all the path_proofs
        let mut proof_range = PathProofRange {
            path_bit_index: 0,
            lower: 0,
            upper: path_proofs.len(),
        };

        // Common siblings encountered while stepping through PathProofRange
        let mut common_siblings: Vec<Node> = vec![];

        // stack used to handle bfs through PathProofRanges
        let mut stack: Vec<PathProofRange> = vec![];

        loop {
            // check if proof_range represents a unique path proof
            if let Some((sub_path_proof, unique_siblings)) =
                proof_range.prove_unique_path_remainder(&path_proofs)
            {
                paths.push(sub_path_proof);
                siblings.extend(unique_siblings);

                // sub_path_proof always immediately follows a bisection in a well-formed trie
                assert!(common_siblings.is_empty());

                // skip to the next bisection in the stack, if empty we're finished
                proof_range = match stack.pop() {
                    Some(v) => v,
                    None => break,
                };
                continue;
            }

            // Step through the proof_range, it could result in a bisection,
            // or the index of the key_path is moved forward producing a new
            // sibling of the current sub tree
            match proof_range.step(&path_proofs) {
                PathProofRangeStep::Bisect { left, right } => {
                    // insert collected common siblings
                    siblings.extend(common_siblings.drain(..));

                    // push into the stack the right Bisection and work on the left one
                    proof_range = left;
                    stack.push(right);
                }
                PathProofRangeStep::Advance { sibling } => common_siblings.push(sibling),
            };
        }

        Self { paths, siblings }
    }
}

/// Errors in multi-proof verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiProofVerificationError {
    /// Root hash mismatched at the end of the verification.
    RootMismatch,
    /// Multi-proof paths were provided out of order.
    PathsOutOfOrder,
    /// Extra siblings were provided.
    TooManySiblings,
}

#[derive(Debug, Clone)]
struct VerifiedMultiPath {
    terminal: PathProofTerminal,
    depth: usize,
    unique_siblings: Range<usize>,
}

// indicates a bisection which started at a given depth and covers these common siblings.
#[derive(Debug, Clone)]
struct VerifiedBisection {
    start_depth: usize,
    common_siblings: Range<usize>,
}

/// A verified multi-proof.
#[derive(Debug, Clone)]
#[must_use = "VerifiedMultiProof only checks the consistency of the trie, not the values"]
pub struct VerifiedMultiProof {
    inner: Vec<VerifiedMultiPath>,
    bisections: Vec<VerifiedBisection>,
    siblings: Vec<Node>,
    root: Node,
}

impl VerifiedMultiProof {
    /// Find the index of the path contained in this multi-proof, if any, which would prove
    /// the given key.
    ///
    /// Runtime is O(log(n)) in the number of paths this multi-proof contains.
    pub fn find_index_for(&self, key_path: &KeyPath) -> Result<usize, KeyOutOfScope> {
        let search_result = self.inner.binary_search_by(|v| {
            v.terminal.path()[..v.depth].cmp(&key_path.view_bits::<Msb0>()[..v.depth])
        });

        search_result.map_err(|_| KeyOutOfScope)
    }

    /// Check whether this proves that a key has no value in the trie.
    /// Runtime is O(log(n)) in the number of paths this multi-proof contains.
    ///
    /// A return value of `Ok(true)` confirms that the key indeed has no value in the trie.
    /// A return value of `Ok(false)` means that the key definitely exists within the trie.
    ///
    /// Fails if the key is out of the scope of this proof.
    pub fn confirm_nonexistence(&self, key_path: &KeyPath) -> Result<bool, KeyOutOfScope> {
        let index = self.find_index_for(key_path)?;
        Ok(self.confirm_nonexistence_inner(key_path, index))
    }

    /// Check whether this proves that a key has no value in the trie.
    /// Runtime is O(log(n)) in the number of paths this multi-proof contains.
    ///
    /// A return value of `Ok(true)` confirms that this key indeed has this value in the trie.
    /// A return value of `Ok(false)` means that this key has a different value or does not exist.
    ///
    /// Fails if the key is out of the scope of this proof.
    pub fn confirm_value(&self, expected_leaf: &LeafData) -> Result<bool, KeyOutOfScope> {
        let index = self.find_index_for(&expected_leaf.key_path)?;
        Ok(self.confirm_value_inner(&expected_leaf, index))
    }

    /// Check whether the specific path with index `index` proves that a key has no value in
    /// the trie.
    /// Runtime is O(1).
    ///
    /// A return value of `Ok(true)` confirms that the key indeed has no value in the trie.
    /// A return value of `Ok(false)` means that the key definitely exists within the trie.
    ///
    /// Fails if the key is out of the scope of this path.
    ///
    /// # Panics
    ///
    /// Panics if the index is out-of-bounds.
    pub fn confirm_nonexistence_with_index(
        &self,
        key_path: &KeyPath,
        index: usize,
    ) -> Result<bool, KeyOutOfScope> {
        let path = &self.inner[index];
        let depth = path.depth;
        let in_scope = path.terminal.path()[..depth] == key_path.view_bits::<Msb0>()[..depth];

        if in_scope {
            Ok(self.confirm_nonexistence_inner(key_path, index))
        } else {
            Err(KeyOutOfScope)
        }
    }

    /// Check whether this proves that a key has no value in the trie.
    /// Runtime is O(1).
    ///
    /// A return value of `Ok(true)` confirms that this key indeed has this value in the trie.
    /// A return value of `Ok(false)` means that this key has a different value or does not exist
    /// in the trie.
    ///
    /// Fails if the key is out of the scope of this path.
    ///
    /// # Panics
    ///
    /// Panics if the index is out-of-bounds.
    pub fn confirm_value_with_index(
        &self,
        expected_leaf: &LeafData,
        index: usize,
    ) -> Result<bool, KeyOutOfScope> {
        let path = &self.inner[index];
        let depth = path.depth;
        let in_scope =
            path.terminal.path()[..depth] == expected_leaf.key_path.view_bits::<Msb0>()[..depth];

        if in_scope {
            Ok(self.confirm_value_inner(&expected_leaf, index))
        } else {
            Err(KeyOutOfScope)
        }
    }

    // assume in-scope
    fn confirm_nonexistence_inner(&self, key_path: &KeyPath, index: usize) -> bool {
        match self.inner[index].terminal {
            PathProofTerminal::Terminator(_) => true,
            PathProofTerminal::Leaf(ref leaf_data) => &leaf_data.key_path != key_path,
        }
    }

    // assume in-scope
    fn confirm_value_inner(&self, expected_leaf: &LeafData, index: usize) -> bool {
        match self.inner[index].terminal {
            PathProofTerminal::Terminator(_) => false,
            PathProofTerminal::Leaf(ref leaf_data) => leaf_data == expected_leaf,
        }
    }
}

/// Verify a multi-proof against an expected root. This ONLY checks the consistency of the trie.
///
/// You MUST use `confirm_value` or `confirm_nonexistence` to check the values in the verified
/// multi-proof.
pub fn verify<H: NodeHasher>(
    multi_proof: &MultiProof,
    root: Node,
) -> Result<VerifiedMultiProof, MultiProofVerificationError> {
    let mut verified_paths = Vec::with_capacity(multi_proof.paths.len());
    let mut verified_bisections = Vec::new();
    for i in 0..multi_proof.paths.len() {
        let path = &multi_proof.paths[i];
        if i > 0 {
            if path.terminal.path() <= multi_proof.paths[i - 1].terminal.path() {
                return Err(MultiProofVerificationError::PathsOutOfOrder);
            }
        }
    }

    let (new_root, siblings_used) = verify_range::<H>(
        0,
        &multi_proof.paths,
        &multi_proof.siblings,
        0,
        &mut verified_paths,
        &mut verified_bisections,
    )?;

    if root != new_root {
        return Err(MultiProofVerificationError::RootMismatch);
    }

    if siblings_used != multi_proof.siblings.len() {
        return Err(MultiProofVerificationError::TooManySiblings);
    }

    Ok(VerifiedMultiProof {
        inner: verified_paths,
        bisections: verified_bisections,
        siblings: multi_proof.siblings.clone(),
        root: root,
    })
}

// returns the node made by verifying this range along with the number of siblings used.
fn verify_range<H: NodeHasher>(
    start_depth: usize,
    paths: &[MultiPathProof],
    siblings: &[Node],
    sibling_offset: usize,
    verified_paths: &mut Vec<VerifiedMultiPath>,
    verified_bisections: &mut Vec<VerifiedBisection>,
) -> Result<(Node, usize), MultiProofVerificationError> {
    // the range should never be empty except in the first call, if the entire multi-proof is
    // empty.
    if paths.is_empty() {
        verified_paths.push(VerifiedMultiPath {
            terminal: PathProofTerminal::Terminator(crate::trie_pos::TriePosition::new()),
            depth: 0,
            unique_siblings: Range { start: 0, end: 0 },
        });
        return Ok((TERMINATOR, 0));
    }
    if paths.len() == 1 {
        // at a terminal node, 'siblings' will contain all unique
        // nodes, hash them up, and return that
        let terminal_path = &paths[0];
        let unique_len = terminal_path.depth - start_depth;

        let node = hash_path::<H>(
            terminal_path.terminal.node::<H>(),
            &terminal_path.terminal.path()[start_depth..start_depth + unique_len],
            siblings[..unique_len].iter().rev().copied(),
        );

        verified_paths.push(VerifiedMultiPath {
            terminal: terminal_path.terminal.clone(),
            depth: terminal_path.depth,
            unique_siblings: Range {
                start: sibling_offset,
                end: sibling_offset + unique_len,
            },
        });

        return Ok((node, unique_len));
    }

    let start_path = &paths[0];
    let end_path = &paths[paths.len() - 1];

    let common_bits = shared_bits(
        &start_path.terminal.path()[start_depth..],
        &end_path.terminal.path()[start_depth..],
    );

    let common_len = start_depth + common_bits;
    // TODO: if `common_len` == 256 the multi-proof is malformed. error

    let uncommon_start_len = common_len + 1;

    // bisect `paths` by finding the first path which starts with the right bit set.
    let search_result = paths.binary_search_by(|item| {
        if !item.terminal.path()[uncommon_start_len - 1] {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    });

    // UNWRAP: always `Err` because we never return Ordering::Equal
    // index always in-bounds because `end_path` has the significant bit set to 1
    // furthermore, the left and right slices must be non-empty because start/end exist and the
    // bisection is based off of them.
    let bisect_idx = search_result.unwrap_err();

    if common_bits > 0 {
        verified_bisections.push(VerifiedBisection {
            start_depth,
            common_siblings: Range {
                start: sibling_offset,
                end: sibling_offset + common_bits,
            },
        });
    }

    // recurse into the left bisection.
    let (left_node, left_siblings_used) = verify_range::<H>(
        uncommon_start_len,
        &paths[..bisect_idx],
        &siblings[common_bits..],
        sibling_offset + common_bits,
        verified_paths,
        verified_bisections,
    )?;

    // now that we know how many siblings were used on the left, we can recurse into the right.
    let (right_node, right_siblings_used) = verify_range::<H>(
        uncommon_start_len,
        &paths[bisect_idx..],
        &siblings[common_bits + left_siblings_used..],
        sibling_offset + common_bits + left_siblings_used,
        verified_paths,
        verified_bisections,
    )?;

    let total_siblings_used = common_bits + left_siblings_used + right_siblings_used;
    // hash up the internal node composed of left/right, then repeatedly apply common siblings.
    let node = hash_path::<H>(
        H::hash_internal(&InternalData {
            left: left_node,
            right: right_node,
        }),
        &start_path.terminal.path()[start_depth..common_len], // == last_path.same...
        siblings[..common_bits].iter().rev().copied(),
    );
    Ok((node, total_siblings_used))
}

/// Errors that can occur when verifying an update against a [`VerifiedMultiProof`].
#[derive(Debug, Clone, Copy)]
pub enum MultiVerifyUpdateError {
    /// The operations on the trie were provided out-of-order by [`KeyPath`].
    OpsOutOfOrder,
    /// An operation was out of scope for the [`VerifiedMultiProof`]
    OpOutOfScope,
    /// Paths were verified against different state-roots.
    RootMismatch,
    /// Two terminal paths were provided, where one is a prefix of another.
    PathPrefixOfAnother,
}

fn terminal_contains(terminal: &VerifiedMultiPath, key_path: &KeyPath) -> bool {
    key_path.view_bits::<Msb0>()[..terminal.depth] == terminal.terminal.path()[..terminal.depth]
}

// walks a multiproof left-to-right and keeps track of a stack of all siblings, based on
// bisections.
//
// when this has ingested all paths up to and including X, the stack will represent all non-unique
// siblings for X.
#[derive(Debug)]
struct CommonSiblings {
    bisection_stack: Vec<VerifiedBisection>,
    stack: Vec<(usize, Node)>,
    taken_siblings: usize,
    terminal_index: usize,
    bisection_index: usize,
}

impl CommonSiblings {
    fn new() -> Self {
        CommonSiblings {
            bisection_stack: Vec::new(),
            stack: Vec::new(),
            taken_siblings: 0,
            terminal_index: 0,
            bisection_index: 0,
        }
    }

    fn advance(&mut self, proof: &VerifiedMultiProof) {
        let next_terminal = &proof.inner[self.terminal_index];

        let mut prune = true;
        while next_terminal.unique_siblings.start != self.taken_siblings {
            let next_bisection = &proof.bisections[self.bisection_index];
            self.bisection_index += 1;

            assert_eq!(next_bisection.common_siblings.start, self.taken_siblings);
            if prune {
                self.pop_to(next_bisection.start_depth);
                prune = false;
            }

            // a bisection at depth N involves siblings starting at N+1
            self.extend(
                next_bisection.start_depth + 1,
                next_bisection.common_siblings.end,
                &proof.siblings,
                false,
            );
            self.bisection_stack.push(next_bisection.clone());
        }

        let terminal_n = next_terminal.unique_siblings.end - next_terminal.unique_siblings.start;
        self.extend(
            next_terminal.depth - terminal_n + 1,
            next_terminal.unique_siblings.end,
            &proof.siblings,
            true,
        );
        self.terminal_index += 1;
    }

    fn pop_to(&mut self, depth: usize) {
        while self
            .bisection_stack
            .last()
            .map_or(false, |b| b.start_depth >= depth)
        {
            let _ = self.bisection_stack.pop();
        }

        while self.stack.last().map_or(false, |(d, _)| *d >= depth) {
            let _ = self.stack.pop();
        }
    }

    fn extend(&mut self, start_depth: usize, end: usize, siblings: &[Node], reverse: bool) {
        if reverse {
            for (i, sibling) in siblings[self.taken_siblings..end].iter().rev().enumerate() {
                self.stack.push((start_depth + i, *sibling))
            }
        } else {
            for (i, sibling) in siblings[self.taken_siblings..end].iter().enumerate() {
                self.stack.push((start_depth + i, *sibling))
            }
        }

        self.taken_siblings = end;
    }

    fn pop_if_at_depth(&mut self, depth: usize) -> Option<Node> {
        if self.stack.last().map_or(false, |(d, _)| *d == depth) {
            self.stack.pop().map(|(_, n)| n)
        } else {
            None
        }
    }
}

/// Verify an update operation against a verified multi-proof. This follows a similar algorithm to
/// the multi-item update, but without altering any backing storage.
///
/// `ops` should contain all updates to be processed. It should be sorted (ascending) by keypath,
/// without duplicates.
///
/// All provided operations should have a key-path which is in scope for the multi proof.
///
/// Returns the root of the trie obtained after application of the given updates in the `paths`
/// vector. In case the `paths` is empty, `prev_root` is returned.
pub fn verify_update<H: NodeHasher>(
    proof: &VerifiedMultiProof,
    ops: Vec<(KeyPath, Option<ValueHash>)>,
) -> Result<Node, MultiVerifyUpdateError> {
    if ops.is_empty() {
        return Ok(proof.root);
    }

    // left frontier
    let mut pending_siblings: Vec<(Node, usize)> = Vec::new();

    let mut last_key = None;
    let mut last_terminal_index = None;
    let mut next_pending_terminal_index = None;

    let mut working_ops = Vec::new();

    let mut common_siblings = CommonSiblings::new();
    let ops_len = ops.len();

    // chain with dummy item for handling the last batch.
    for (i, (key, op)) in ops.into_iter().chain(Some(([0u8; 32], None))).enumerate() {
        let is_last = i == ops_len;

        if is_last {
            let updated_terminal_index = last_terminal_index.unwrap_or(0);
            let start = next_pending_terminal_index.unwrap_or(0);

            // ingest all terminals from the next one needing an update to the end.
            for terminal_index in start..proof.inner.len() {
                let next = if terminal_index == proof.inner.len() - 1 {
                    None
                } else {
                    Some(terminal_index + 1)
                };

                let terminal = &proof.inner[terminal_index];
                let next_terminal = next.map(|n| &proof.inner[n]);

                let ops = if terminal_index == updated_terminal_index {
                    &working_ops[..]
                } else {
                    &[]
                };

                common_siblings.advance(&proof);
                hash_and_compact_terminal::<H>(
                    &mut pending_siblings,
                    terminal,
                    next_terminal,
                    &mut common_siblings,
                    ops,
                )?;
            }
        } else {
            // enforce key ordering.
            if let Some(last_key) = last_key {
                if key <= last_key {
                    return Err(MultiVerifyUpdateError::OpsOutOfOrder);
                }
            }
            last_key = Some(key);

            // find terminal index for the operation, erroring if out of scope.
            let mut next_terminal_index = last_terminal_index.unwrap_or(0);
            if proof.inner.len() <= next_terminal_index {
                return Err(MultiVerifyUpdateError::OpOutOfScope);
            }

            while !terminal_contains(&proof.inner[next_terminal_index], &key) {
                next_terminal_index += 1;
                if proof.inner.len() <= next_terminal_index {
                    return Err(MultiVerifyUpdateError::OpOutOfScope);
                }
            }

            // if this is either the first op or this has the same terminal as the previous op...
            if last_terminal_index.map_or(true, |x| x == next_terminal_index) {
                last_terminal_index = Some(next_terminal_index);
                working_ops.push((key, op));
                continue;
            }

            // UNWRAP: guaranteed by above.
            let updated_index = last_terminal_index.unwrap();
            last_terminal_index = Some(next_terminal_index);

            // ingest all terminals up to current.
            let start = next_pending_terminal_index.unwrap_or(0);

            for terminal_index in start..updated_index {
                let terminal = &proof.inner[terminal_index];
                let next_terminal = Some(&proof.inner[terminal_index + 1]);

                common_siblings.advance(&proof);
                hash_and_compact_terminal::<H>(
                    &mut pending_siblings,
                    terminal,
                    next_terminal,
                    &mut common_siblings,
                    &[],
                )?;
            }

            // ingest the currently updated terminal.
            let ops = core::mem::replace(&mut working_ops, Vec::new());
            working_ops.push((key, op));

            let terminal = &proof.inner[updated_index];
            let next_terminal = proof.inner.get(updated_index + 1);
            common_siblings.advance(&proof);

            hash_and_compact_terminal::<H>(
                &mut pending_siblings,
                terminal,
                next_terminal,
                &mut common_siblings,
                &ops,
            )?;

            next_pending_terminal_index = Some(updated_index + 1);
        };
    }

    // UNWRAP: This is always full unless the update is empty
    Ok(pending_siblings.pop().map(|n| n.0).unwrap_or(proof.root))
}

fn hash_and_compact_terminal<H: NodeHasher>(
    pending_siblings: &mut Vec<(Node, usize)>,
    terminal: &VerifiedMultiPath,
    next_terminal: Option<&VerifiedMultiPath>,
    common_siblings: &mut CommonSiblings,
    ops: &[(KeyPath, Option<ValueHash>)],
) -> Result<(), MultiVerifyUpdateError> {
    let leaf = terminal.terminal.as_leaf_option();
    let skip = terminal.depth;

    let up_layers = if let Some(next_terminal) = next_terminal {
        let n = shared_bits(terminal.terminal.path(), next_terminal.terminal.path());

        // SANITY: this is impossible in a well-formed input but we catch it as an error.
        // The reason this is impossible is because no terminal should be a prefix of another
        // terminal (by definition)
        if n == skip {
            return Err(MultiVerifyUpdateError::PathPrefixOfAnother);
        }

        // n always < skip
        // we want to end at layer n + 1
        skip - (n + 1)
    } else {
        skip // go to root
    };

    let ops = crate::update::leaf_ops_spliced(leaf, &ops);
    let sub_root = crate::update::build_trie::<H>(skip, ops, |_| {});

    let mut cur_node = sub_root;
    let mut cur_layer = skip;
    let end_layer = skip - up_layers;

    // iterate siblings up to the point of collision with next path, replacing with pending
    // siblings, and compacting where possible.
    // push (node, end_layer) to pending siblings when done.
    for bit in terminal.terminal.path()[..terminal.depth]
        .iter()
        .by_vals()
        .rev()
        .take(up_layers)
    {
        let sibling = if pending_siblings.last().map_or(false, |p| p.1 == cur_layer) {
            // is this even possible? maybe not. but being extra cautious...
            let _ = common_siblings.pop_if_at_depth(cur_layer);
            // UNWRAP: guaranteed to exist.
            pending_siblings.pop().unwrap().0
        } else {
            // UNWRAP: `common_siblings` holds everything which isn't computed dynamically from branch
            // to branch. basically, it's the inverse of `pending_siblings`. so if the sibling isn't
            // in pending_siblings, it's in here.
            common_siblings.pop_if_at_depth(cur_layer).unwrap()
        };

        match (NodeKind::of::<H>(&cur_node), NodeKind::of::<H>(&sibling)) {
            (NodeKind::Terminator, NodeKind::Terminator) => {}
            (NodeKind::Leaf, NodeKind::Terminator) => {}
            (NodeKind::Terminator, NodeKind::Leaf) => {
                // relocate sibling upwards.
                cur_node = sibling;
            }
            _ => {
                // otherwise, internal
                let node_data = if bit {
                    InternalData {
                        left: sibling,
                        right: cur_node,
                    }
                } else {
                    InternalData {
                        left: cur_node,
                        right: sibling,
                    }
                };
                cur_node = H::hash_internal(&node_data);
            }
        }

        cur_layer -= 1;
    }

    pending_siblings.push((cur_node, end_layer));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{verify, verify_update, MultiProof};

    use crate::proof::multi_proof::{
        MultiVerifyUpdateError, VerifiedMultiPath, VerifiedMultiProof,
    };
    use crate::{
        hasher::{Blake3Hasher, NodeHasher},
        proof::{PathProof, PathProofTerminal},
        trie::{InternalData, KeyPath, LeafData, ValueHash, TERMINATOR},
        trie_pos::TriePosition,
        update::build_trie,
    };
    use bitvec::prelude::*;

    #[test]
    pub fn test_multiproof_creation_single_path_proof() {
        let mut key_path = [0; 32];
        key_path[0] = 0b10000000;
        let sibling1 = [1; 32];
        let sibling2 = [2; 32];
        let path_proof = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path, 256,
            )),
            siblings: vec![sibling1, sibling2],
        };

        let multi_proof = MultiProof::from_path_proofs(vec![path_proof]);
        assert_eq!(multi_proof.paths.len(), 1);
        assert_eq!(
            multi_proof.paths[0].terminal,
            PathProofTerminal::Terminator(TriePosition::from_path_and_depth(key_path, 256))
        );
        assert_eq!(multi_proof.paths[0].depth, 2);
        assert_eq!(multi_proof.siblings.len(), 2);
        assert_eq!(multi_proof.siblings, vec![sibling1, sibling2]);
    }

    #[test]
    pub fn test_multiproof_creation_two_path_proofs() {
        let mut key_path_1 = [0; 32];
        key_path_1[0] = 0b00000000;

        let mut key_path_2 = [0; 32];
        key_path_2[0] = 0b00111000;

        let sibling1 = [1; 32];
        let sibling2 = [2; 32];
        let sibling3 = [3; 32];
        let sibling4 = [4; 32];
        let sibling5 = [5; 32];
        let sibling6 = [6; 32];

        let sibling_x = [b'x'; 32];

        let path_proof_1 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_1, 256,
            )),
            siblings: vec![sibling1, sibling2, sibling_x, sibling3, sibling4],
        };
        let path_proof_2 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_2, 256,
            )),
            siblings: vec![sibling1, sibling2, sibling_x, sibling5, sibling6],
        };

        let multi_proof = MultiProof::from_path_proofs(vec![path_proof_1, path_proof_2]);

        assert_eq!(multi_proof.paths.len(), 2);
        assert_eq!(multi_proof.siblings.len(), 6);

        assert_eq!(
            multi_proof.paths[0].terminal,
            PathProofTerminal::Terminator(TriePosition::from_path_and_depth(key_path_1, 256))
        );
        assert_eq!(
            multi_proof.paths[1].terminal,
            PathProofTerminal::Terminator(TriePosition::from_path_and_depth(key_path_2, 256))
        );

        assert_eq!(multi_proof.paths[0].depth, 5);
        assert_eq!(multi_proof.paths[1].depth, 5);

        assert_eq!(
            multi_proof.siblings,
            vec![sibling1, sibling2, sibling3, sibling4, sibling5, sibling6]
        );
    }

    #[test]
    pub fn test_multiproof_creation_two_path_proofs_256_depth() {
        let mut key_path_1 = [0; 32];
        key_path_1[31] = 0b00000000;

        let mut key_path_2 = [0; 32];
        key_path_2[31] = 0b00000001;

        let mut siblings_1: Vec<[u8; 32]> = (0..255).map(|i| [i; 32]).collect();
        let mut siblings_2 = siblings_1.clone();
        siblings_1.push([b'2'; 32]);
        siblings_2.push([b'1'; 32]);

        let path_proof_1 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_1, 256,
            )),
            siblings: siblings_1.clone(),
        };
        let path_proof_2 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_2, 256,
            )),
            siblings: siblings_2,
        };

        let multi_proof = MultiProof::from_path_proofs(vec![path_proof_1, path_proof_2]);

        assert_eq!(multi_proof.paths.len(), 2);
        assert_eq!(multi_proof.siblings.len(), 255);

        assert_eq!(
            multi_proof.paths[0].terminal,
            PathProofTerminal::Terminator(TriePosition::from_path_and_depth(key_path_1, 256))
        );
        assert_eq!(
            multi_proof.paths[1].terminal,
            PathProofTerminal::Terminator(TriePosition::from_path_and_depth(key_path_2, 256))
        );

        assert_eq!(multi_proof.paths[0].depth, 256);
        assert_eq!(multi_proof.paths[1].depth, 256);

        siblings_1.pop();
        assert_eq!(multi_proof.siblings, siblings_1);
    }

    #[test]
    pub fn test_multiproof_creation_multiple_path_proofs() {
        let mut key_path_1 = [0; 32];
        key_path_1[0] = 0b00000000;

        let mut key_path_2 = [0; 32];
        key_path_2[0] = 0b01000000;

        let mut key_path_3 = [0; 32];
        key_path_3[0] = 0b01001100;

        let mut key_path_4 = [0; 32];
        key_path_4[0] = 0b11101100;

        let mut key_path_5 = [0; 32];
        key_path_5[0] = 0b11110100;

        let mut key_path_6 = [0; 32];
        key_path_6[0] = 0b11111000;

        let sibling1 = [1; 32];
        let sibling2 = [2; 32];
        let sibling3 = [3; 32];
        let sibling4 = [4; 32];
        let sibling5 = [5; 32];
        let sibling6 = [6; 32];
        let sibling7 = [7; 32];
        let sibling8 = [8; 32];
        let sibling9 = [9; 32];
        let sibling10 = [10; 32];
        let sibling11 = [11; 32];
        let sibling12 = [12; 32];
        let sibling13 = [13; 32];
        let sibling14 = [14; 32];
        let sibling15 = [15; 32];
        let sibling16 = [16; 32];
        let sibling17 = [17; 32];
        let sibling18 = [18; 32];
        let sibling19 = [19; 32];

        let path_proof_1 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_1, 256,
            )),
            siblings: vec![sibling1, sibling2],
        };

        let path_proof_2 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_2, 256,
            )),
            siblings: vec![sibling1, sibling3, sibling4, sibling5, sibling6, sibling7],
        };

        let path_proof_3 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_3, 256,
            )),
            siblings: vec![sibling1, sibling3, sibling4, sibling5, sibling8, sibling9],
        };

        let path_proof_4 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_4, 256,
            )),
            siblings: vec![
                sibling10, sibling11, sibling12, sibling13, sibling14, sibling15,
            ],
        };

        let path_proof_5 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_5, 256,
            )),
            siblings: vec![
                sibling10, sibling11, sibling12, sibling16, sibling17, sibling18,
            ],
        };

        let path_proof_6 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_6, 256,
            )),
            siblings: vec![sibling10, sibling11, sibling12, sibling16, sibling19],
        };

        let multi_proof = MultiProof::from_path_proofs(vec![
            path_proof_1,
            path_proof_2,
            path_proof_3,
            path_proof_4,
            path_proof_5,
            path_proof_6,
        ]);

        assert_eq!(multi_proof.paths.len(), 6);
        assert_eq!(multi_proof.siblings.len(), 9);

        assert_eq!(
            multi_proof.paths[0].terminal,
            PathProofTerminal::Terminator(TriePosition::from_path_and_depth(key_path_1, 256))
        );
        assert_eq!(
            multi_proof.paths[1].terminal,
            PathProofTerminal::Terminator(TriePosition::from_path_and_depth(key_path_2, 256))
        );
        assert_eq!(
            multi_proof.paths[2].terminal,
            PathProofTerminal::Terminator(TriePosition::from_path_and_depth(key_path_3, 256))
        );
        assert_eq!(
            multi_proof.paths[3].terminal,
            PathProofTerminal::Terminator(TriePosition::from_path_and_depth(key_path_4, 256))
        );
        assert_eq!(
            multi_proof.paths[4].terminal,
            PathProofTerminal::Terminator(TriePosition::from_path_and_depth(key_path_5, 256))
        );
        assert_eq!(
            multi_proof.paths[5].terminal,
            PathProofTerminal::Terminator(TriePosition::from_path_and_depth(key_path_6, 256))
        );

        assert_eq!(multi_proof.paths[0].depth, 2);
        assert_eq!(multi_proof.paths[1].depth, 6);
        assert_eq!(multi_proof.paths[2].depth, 6);
        assert_eq!(multi_proof.paths[3].depth, 6);
        assert_eq!(multi_proof.paths[4].depth, 6);
        assert_eq!(multi_proof.paths[5].depth, 5);

        assert_eq!(
            multi_proof.siblings,
            vec![
                sibling4, sibling5, sibling7, sibling9, sibling11, sibling12, sibling14, sibling15,
                sibling18
            ]
        );
    }

    #[test]
    pub fn test_multiproof_creation_ext_siblings_order() {
        let mut key_path_0 = [0; 32];
        key_path_0[0] = 0b00001000;

        let mut key_path_1 = [0; 32];
        key_path_1[0] = 0b00010000;

        let mut key_path_2 = [0; 32];
        key_path_2[0] = 0b10000000;

        let mut key_path_3 = [0; 32];
        key_path_3[0] = 0b10000010;

        let mut key_path_4 = [0; 32];
        key_path_4[0] = 0b10010001;

        let mut key_path_5 = [0; 32];
        key_path_5[0] = 0b10010011;

        let sibling1 = [1; 32];
        let sibling2 = [2; 32];
        let sibling3 = [3; 32];
        let sibling4 = [4; 32];
        let sibling5 = [5; 32];
        let sibling6 = [6; 32];
        let sibling7 = [7; 32];
        let sibling8 = [8; 32];
        let sibling9 = [9; 32];
        let sibling10 = [10; 32];
        let sibling11 = [11; 32];
        let sibling12 = [12; 32];
        let sibling13 = [13; 32];
        let sibling14 = [14; 32];
        let sibling15 = [15; 32];
        let sibling16 = [16; 32];
        let sibling17 = [17; 32];
        let sibling18 = [18; 32];
        let sibling19 = [19; 32];
        let sibling20 = [20; 32];
        let sibling21 = [21; 32];
        let sibling22 = [22; 32];
        let sibling23 = [23; 32];
        let sibling24 = [24; 32];

        let path_proof_0 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_0, 256,
            )),
            siblings: vec![sibling1, sibling2, sibling3, sibling4, sibling5],
        };

        let path_proof_1 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_1, 256,
            )),
            siblings: vec![sibling1, sibling2, sibling3, sibling6, sibling7],
        };

        let path_proof_2 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_2, 256,
            )),
            siblings: vec![
                sibling8, sibling9, sibling10, sibling11, sibling12, sibling13, sibling14,
                sibling15,
            ],
        };
        let path_proof_3 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_3, 256,
            )),
            siblings: vec![
                sibling8, sibling9, sibling10, sibling11, sibling12, sibling13, sibling16,
                sibling17,
            ],
        };

        let path_proof_4 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_4, 256,
            )),
            siblings: vec![
                sibling8, sibling9, sibling10, sibling18, sibling19, sibling20, sibling21,
                sibling22,
            ],
        };

        let path_proof_5 = PathProof {
            terminal: PathProofTerminal::Terminator(TriePosition::from_path_and_depth(
                key_path_5, 256,
            )),
            siblings: vec![
                sibling8, sibling9, sibling10, sibling18, sibling19, sibling20, sibling23,
                sibling24,
            ],
        };

        let multi_proof = MultiProof::from_path_proofs(vec![
            path_proof_0,
            path_proof_1,
            path_proof_2,
            path_proof_3,
            path_proof_4,
            path_proof_5,
        ]);

        assert_eq!(multi_proof.paths.len(), 6);
        assert_eq!(multi_proof.siblings.len(), 14);

        assert_eq!(
            multi_proof.siblings,
            vec![
                sibling2, sibling3, sibling5, sibling7, sibling9, sibling10, sibling12, sibling13,
                sibling15, sibling17, sibling19, sibling20, sibling22, sibling24
            ]
        );
    }

    #[test]
    fn multi_proof_failure_empty_witness() {
        let multi_proof = MultiProof::from_path_proofs(Vec::new());

        let _verified_multi_proof = verify::<Blake3Hasher>(&multi_proof, TERMINATOR).unwrap();
    }

    #[test]
    fn multi_proof_verify_empty() {
        let multi_proof = MultiProof::from_path_proofs(Vec::new());

        let verified_multi_proof = verify::<Blake3Hasher>(&multi_proof, TERMINATOR).unwrap();

        assert_eq!(
            verify_update::<Blake3Hasher>(&verified_multi_proof, Vec::new()).unwrap(),
            TERMINATOR,
        );
    }

    #[test]
    fn multi_proof_verify_empty_with_provided_updates() {
        let multi_proof = MultiProof::from_path_proofs(Vec::new());

        let verified_multi_proof = verify::<Blake3Hasher>(&multi_proof, TERMINATOR).unwrap();

        let mut key_path_0 = [0; 32];
        key_path_0[0] = 0b00001000;

        let mut key_path_1 = [0; 32];
        key_path_1[0] = 0b00010000;

        let mut key_path_2 = [0; 32];
        key_path_2[0] = 0b10000000;

        let ops = vec![
            (key_path_0, Some([1; 32])),
            (key_path_1, Some([1; 32])),
            (key_path_2, Some([1; 32])),
        ];

        let expected_root = build_trie::<Blake3Hasher>(
            0,
            ops.clone().into_iter().map(|(k, v)| (k, v.unwrap())),
            |_| {},
        );

        assert_eq!(
            verify_update::<Blake3Hasher>(&verified_multi_proof, ops).unwrap(),
            expected_root,
        );
    }

    #[test]
    pub fn test_verify_multiproof_two_leafs() {
        //     root
        //     /  \
        //    s3   v1
        //   / \
        //  v0  v2

        let mut key_path_0 = [0; 32];
        key_path_0[0] = 0b00000000;

        let mut key_path_1 = [0; 32];
        key_path_1[0] = 0b10000000;

        let mut key_path_2 = [0; 32];
        key_path_2[0] = 0b01000000;

        let leaf_0 = LeafData {
            key_path: key_path_0,
            value_hash: [0; 32],
        };

        let leaf_1 = LeafData {
            key_path: key_path_1,
            value_hash: [1; 32],
        };

        let leaf_2 = LeafData {
            key_path: key_path_2,
            value_hash: [2; 32],
        };

        // this is the
        let v0 = Blake3Hasher::hash_leaf(&leaf_0);
        let v1 = Blake3Hasher::hash_leaf(&leaf_1);
        let v2 = Blake3Hasher::hash_leaf(&leaf_2);
        let s3 = Blake3Hasher::hash_internal(&InternalData {
            left: v0.clone(),
            right: v2,
        });
        let root = Blake3Hasher::hash_internal(&InternalData {
            left: s3,
            right: v1,
        });

        let path_proof_0 = PathProof {
            terminal: PathProofTerminal::Leaf(leaf_0.clone()),
            siblings: vec![v1, v2],
        };
        let path_proof_1 = PathProof {
            terminal: PathProofTerminal::Leaf(leaf_1.clone()),
            siblings: vec![s3],
        };

        let multi_proof =
            MultiProof::from_path_proofs(vec![path_proof_0.clone(), path_proof_1.clone()]);

        let verified = verify::<Blake3Hasher>(&multi_proof, root).unwrap();

        assert!(verified.confirm_value(&leaf_0).unwrap());
        assert!(verified.confirm_value(&leaf_1).unwrap());
    }

    #[test]
    fn multi_proof_verify_2_leaves_with_provided_updates() {
        //     root
        //     /  \
        //    s3   v1
        //   / \
        //  v0  v2

        let mut key_path_0 = [0; 32];
        key_path_0[0] = 0b00000000;

        let mut key_path_1 = [0; 32];
        key_path_1[0] = 0b10000000;

        let mut key_path_2 = [0; 32];
        key_path_2[0] = 0b01000000;

        let leaf_0 = LeafData {
            key_path: key_path_0,
            value_hash: [0; 32],
        };

        let leaf_1 = LeafData {
            key_path: key_path_1,
            value_hash: [1; 32],
        };

        let leaf_2 = LeafData {
            key_path: key_path_2,
            value_hash: [2; 32],
        };

        // this is the
        let v0 = Blake3Hasher::hash_leaf(&leaf_0);
        let v1 = Blake3Hasher::hash_leaf(&leaf_1);
        let v2 = Blake3Hasher::hash_leaf(&leaf_2);
        let s3 = Blake3Hasher::hash_internal(&InternalData {
            left: v0.clone(),
            right: v2,
        });
        let root = Blake3Hasher::hash_internal(&InternalData {
            left: s3,
            right: v1,
        });

        let path_proof_0 = PathProof {
            terminal: PathProofTerminal::Leaf(leaf_0.clone()),
            siblings: vec![v1, v2],
        };
        let path_proof_1 = PathProof {
            terminal: PathProofTerminal::Leaf(leaf_1.clone()),
            siblings: vec![s3],
        };

        let multi_proof =
            MultiProof::from_path_proofs(vec![path_proof_0.clone(), path_proof_1.clone()]);

        let verified = verify::<Blake3Hasher>(&multi_proof, root).unwrap();

        let mut key_path_3 = key_path_1;
        key_path_3[0] = 0b10100000;

        let mut key_path_4 = key_path_0;
        key_path_4[0] = 0b00000100;

        let ops = vec![
            (key_path_0, Some([2; 32])),
            (key_path_4, Some([1; 32])),
            (key_path_1, None),
            (key_path_3, Some([1; 32])),
        ];

        let final_state = vec![
            (key_path_0, [2; 32]),
            (key_path_4, [1; 32]),
            (key_path_2, [2; 32]),
            (key_path_3, [1; 32]),
        ];

        let expected_root = build_trie::<Blake3Hasher>(0, final_state, |_| {});

        assert_eq!(
            verify_update::<Blake3Hasher>(&verified, ops).unwrap(),
            expected_root,
        );
    }

    #[test]
    fn multi_proof_verify_4_leaves_with_long_bisections() {
        //              R
        //              i1
        //              i2
        //              i3
        //              i4
        //           i5a    i5a
        //           i6a    i6b
        //           i7a    i7b
        //         l8a l8b l8c l8d

        let make_leaf = |key_path, value_byte| {
            let leaf_data = LeafData {
                key_path,
                value_hash: [value_byte; 32],
            };

            let hash = Blake3Hasher::hash_leaf(&leaf_data);
            (leaf_data, hash)
        };
        let internal_hash =
            |left, right| Blake3Hasher::hash_internal(&InternalData { left, right });

        let mut key_path_0 = [0; 32];
        key_path_0[0] = 0b00000000;

        let mut key_path_1 = [0; 32];
        key_path_1[0] = 0b00000001;

        let mut key_path_2 = [0; 32];
        key_path_2[0] = 0b00001000;

        let mut key_path_3 = [0; 32];
        key_path_3[0] = 0b00001001;

        let (leaf_a, l8a) = make_leaf(key_path_0, 1);
        let (leaf_b, l8b) = make_leaf(key_path_1, 1);
        let (leaf_c, l8c) = make_leaf(key_path_2, 1);
        let (leaf_d, l8d) = make_leaf(key_path_3, 1);

        let i7a = internal_hash(l8a, l8b);
        let i7b = internal_hash(l8c, l8d);

        let i6a = internal_hash(i7a, [7; 32]);
        let i6b = internal_hash(i7b, [7; 32]);

        let i5a = internal_hash(i6a, [6; 32]);
        let i5b = internal_hash(i6b, [6; 32]);

        let i4 = internal_hash(i5a, i5b);
        let i3 = internal_hash(i4, [4; 32]);
        let i2 = internal_hash(i3, [3; 32]);
        let i1 = internal_hash(i2, [2; 32]);
        let root = internal_hash(i1, [1; 32]);

        let path_proof_a = PathProof {
            terminal: PathProofTerminal::Leaf(leaf_a.clone()),
            siblings: vec![
                [1; 32], [2; 32], [3; 32], [4; 32], i5b, [6; 32], [7; 32], l8b,
            ],
        };
        let path_proof_b = PathProof {
            terminal: PathProofTerminal::Leaf(leaf_b.clone()),
            siblings: vec![
                [1; 32], [2; 32], [3; 32], [4; 32], i5b, [6; 32], [7; 32], l8a,
            ],
        };
        let path_proof_c = PathProof {
            terminal: PathProofTerminal::Leaf(leaf_c.clone()),
            siblings: vec![
                [1; 32], [2; 32], [3; 32], [4; 32], i5a, [6; 32], [7; 32], l8d,
            ],
        };
        let path_proof_d = PathProof {
            terminal: PathProofTerminal::Leaf(leaf_d.clone()),
            siblings: vec![
                [1; 32], [2; 32], [3; 32], [4; 32], i5a, [6; 32], [7; 32], l8c,
            ],
        };

        let multi_proof = MultiProof::from_path_proofs(vec![
            path_proof_a.clone(),
            path_proof_b.clone(),
            path_proof_c.clone(),
            path_proof_d.clone(),
        ]);

        let verified = verify::<Blake3Hasher>(&multi_proof, root).unwrap();

        let ops = vec![(key_path_0, Some([69; 32])), (key_path_3, Some([69; 32]))];

        let (_, l8a) = make_leaf(key_path_0, 69);
        let (_, l8b) = make_leaf(key_path_1, 1);
        let (_, l8c) = make_leaf(key_path_2, 1);
        let (_, l8d) = make_leaf(key_path_3, 69);

        let i7a = internal_hash(l8a, l8b);
        let i7b = internal_hash(l8c, l8d);

        let i6a = internal_hash(i7a, [7; 32]);
        let i6b = internal_hash(i7b, [7; 32]);

        let i5a = internal_hash(i6a, [6; 32]);
        let i5b = internal_hash(i6b, [6; 32]);

        let i4 = internal_hash(i5a, i5b);

        let i3 = internal_hash(i4, [4; 32]);
        let i2 = internal_hash(i3, [3; 32]);
        let i1 = internal_hash(i2, [2; 32]);
        let post_root = internal_hash(i1, [1; 32]);

        assert_eq!(
            verify_update::<Blake3Hasher>(&verified, ops).unwrap(),
            post_root,
        );
    }

    #[test]
    pub fn test_verify_multiproof_multiple_leafs() {
        //                       root
        //                 /            \
        //                i3            i6
        //             /     \       /      \
        //            i2      T     v3      i5
        //          /     \                /   \
        //         i1     v2              i4   v5
        //        / \                    /  \
        //       v0  v1                 v4   T

        let path = |byte| [byte; 32];

        let k0 = path(0b00000000);
        let k1 = path(0b00011000);
        let k2 = path(0b00101101);
        let k3 = path(0b10101010);
        let k4 = path(0b11000011);
        let k5 = path(0b11100010);

        let make_leaf = |key_path| {
            let leaf_data = LeafData {
                key_path,
                value_hash: [key_path[0]; 32],
            };

            let hash = Blake3Hasher::hash_leaf(&leaf_data);
            (leaf_data, hash)
        };
        let internal_hash =
            |left, right| Blake3Hasher::hash_internal(&InternalData { left, right });

        let (l0, v0) = make_leaf(k0);
        let (l1, v1) = make_leaf(k1);
        let (l2, v2) = make_leaf(k2);
        let (l3, v3) = make_leaf(k3);
        let (l4, v4) = make_leaf(k4);
        let (l5, v5) = make_leaf(k5);

        let i1 = internal_hash(v0, v1);
        let i2 = internal_hash(i1, v2);
        let i3 = internal_hash(i2, TERMINATOR);

        let i4 = internal_hash(v4, TERMINATOR);
        let i5 = internal_hash(i4, v5);
        let i6 = internal_hash(v3, i5);

        let root = internal_hash(i3, i6);

        let leaf_proof = |leaf, siblings| PathProof {
            terminal: PathProofTerminal::Leaf(leaf),
            siblings,
        };

        let path_proof_0 = leaf_proof(l0.clone(), vec![i6, TERMINATOR, v2, v1]);
        let path_proof_1 = leaf_proof(l1.clone(), vec![i6, TERMINATOR, v2, v0]);
        let path_proof_2 = leaf_proof(l2.clone(), vec![i6, TERMINATOR, i1]);
        let path_proof_3 = leaf_proof(l3.clone(), vec![i3, i5]);
        let path_proof_4 = leaf_proof(l4.clone(), vec![i3, v3, v5, TERMINATOR]);
        let path_proof_5 = leaf_proof(l5.clone(), vec![i3, v3, i4]);

        let multi_proof = MultiProof::from_path_proofs(vec![
            path_proof_0.clone(),
            path_proof_1.clone(),
            path_proof_2.clone(),
            path_proof_3.clone(),
            path_proof_4.clone(),
            path_proof_5.clone(),
        ]);

        let verified = verify::<Blake3Hasher>(&multi_proof, root).unwrap();
        assert!(verified.confirm_value(&l0).unwrap());
        assert!(verified.confirm_value(&l1).unwrap());
        assert!(verified.confirm_value(&l2).unwrap());
        assert!(verified.confirm_value(&l3).unwrap());
        assert!(verified.confirm_value(&l4).unwrap());
        assert!(verified.confirm_value(&l5).unwrap());
    }

    #[test]
    pub fn test_verify_multiproof_siblings_structure() {
        //                           root
        //                     /            \
        //                    v0           i10
        //                               /     \
        //                              i9     e7
        //                            /     \
        //                           i8     e6
        //                       /        \
        //                      i4        i7
        //                     /  \      /  \
        //                    i3   e3   e5   i6
        //                   /  \           /  \
        //                  i2   e2        e4   i5
        //                 /  \               /  \
        //                i1   e1            v3   v4
        //               /  \
        //              v1  v2

        let path = |byte| [byte; 32];

        let k0 = path(0b00000000);
        let k1 = path(0b10000000);
        let k2 = path(0b10000001);
        let k3 = path(0b10011100);
        let k4 = path(0b10011110);

        let make_leaf = |key_path| {
            let leaf_data = LeafData {
                key_path,
                value_hash: [key_path[0]; 32],
            };

            let hash = Blake3Hasher::hash_leaf(&leaf_data);
            (leaf_data, hash)
        };
        let internal_hash =
            |left, right| Blake3Hasher::hash_internal(&InternalData { left, right });

        let (_l0, v0) = make_leaf(k0);
        let (l1, v1) = make_leaf(k1);
        let (l2, v2) = make_leaf(k2);
        let (l3, v3) = make_leaf(k3);
        let (l4, v4) = make_leaf(k4);

        let e1 = [1; 32];
        let e2 = [2; 32];
        let e3 = [3; 32];
        let e4 = [4; 32];
        let e5 = [5; 32];
        let e6 = [6; 32];
        let e7 = TERMINATOR;

        let i1 = internal_hash(v1, v2);
        let i2 = internal_hash(i1, e1);
        let i3 = internal_hash(i2, e2);
        let i4 = internal_hash(i3, e3);

        let i5 = internal_hash(v3, v4);
        let i6 = internal_hash(e4, i5);
        let i7 = internal_hash(e5, i6);

        let i8 = internal_hash(i4, i7);
        let i9 = internal_hash(i8, e6);
        let i10 = internal_hash(i9, e7);

        let root = internal_hash(v0, i10);

        let leaf_proof = |leaf, siblings| PathProof {
            terminal: PathProofTerminal::Leaf(leaf),
            siblings,
        };

        let path_proof_1 = leaf_proof(l1.clone(), vec![v0, e7, e6, i7, e3, e2, e1, v2]);
        let path_proof_2 = leaf_proof(l2.clone(), vec![v0, e7, e6, i7, e3, e2, e1, v1]);
        let path_proof_3 = leaf_proof(l3.clone(), vec![v0, e7, e6, i4, e5, e4, v4]);
        let path_proof_4 = leaf_proof(l4.clone(), vec![v0, e7, e6, i4, e5, e4, v3]);

        let multi_proof = MultiProof::from_path_proofs(vec![
            path_proof_1.clone(),
            path_proof_2.clone(),
            path_proof_3.clone(),
            path_proof_4.clone(),
        ]);

        assert_eq!(multi_proof.siblings, vec![v0, e7, e6, e3, e2, e1, e5, e4]);

        let verified = verify::<Blake3Hasher>(&multi_proof, root).unwrap();
        assert!(verified.confirm_value(&l1).unwrap());
        assert!(verified.confirm_value(&l2).unwrap());
        assert!(verified.confirm_value(&l3).unwrap());
        assert!(verified.confirm_value(&l4).unwrap());
    }

    #[test]

    fn test_verify_update_underflow_prefix_paths() {
        // 1. Define paths with prefix relationship
        let bits_prefix = bitvec![u8, Msb0; 1, 0, 1, 0]; // length 4
        let bits_longer = bitvec![u8, Msb0; 1, 0, 1, 0, 1, 1]; // length 6 (prefix + 2 bits)

        // Helper to create KeyPath from BitVec (simplified, assumes short paths)
        let keypath_from_bits = |bits: &BitSlice<u8, Msb0>| -> KeyPath {
            let mut kp = KeyPath::default();

            for (i, bit) in bits.iter().by_vals().enumerate() {
                if i >= 256 {
                    break;
                }

                if bit {
                    kp[i / 8] |= 1 << (7 - (i % 8));
                }
            }

            kp
        };

        let kp_prefix = keypath_from_bits(&bits_prefix);
        let kp_longer = keypath_from_bits(&bits_longer);

        // 2. Create corresponding LeafData and Nodes
        let leaf_prefix = LeafData {
            key_path: kp_prefix,
            value_hash: [1; 32],
        };

        let leaf_longer = LeafData {
            key_path: kp_longer,
            value_hash: [2; 32],
        };

        let node_prefix = Blake3Hasher::hash_leaf(&leaf_prefix);
        let node_longer = Blake3Hasher::hash_leaf(&leaf_longer);

        // 3. Construct the VerifiedMultiProof
        let vmp_prefix = VerifiedMultiPath {
            terminal: PathProofTerminal::Leaf(leaf_prefix.clone()),
            depth: bits_prefix.len(),
            unique_siblings: 0..0, // Minimal example
        };

        let vmp_longer = VerifiedMultiPath {
            terminal: PathProofTerminal::Leaf(leaf_longer.clone()),
            depth: bits_longer.len(),
            unique_siblings: 0..0, // Minimal example
        };

        // Create a plausible root for the test setup.
        // For the purpose of triggering the bug, the exact root structure isn't critical,
        // as long as the VerifiedMultiProof structure is valid and contains the prefix paths.
        let plausible_root = Blake3Hasher::hash_internal(&InternalData {
            left: node_prefix,
            right: node_longer,
        });

        let verified_proof = VerifiedMultiProof {
            inner: vec![vmp_prefix, vmp_longer],

            bisections: Vec::new(), // Minimal example

            siblings: Vec::new(), // Minimal example

            root: plausible_root,
        };

        // 4. Create operations falling under each path
        let mut key_op1_bits = bits_prefix.clone();

        key_op1_bits.push(false); // e.g., 10100 (falls under 1010)

        let key_op1 = keypath_from_bits(&key_op1_bits);

        let mut key_op2_bits = bits_longer.clone();

        key_op2_bits.push(true); // e.g., 1010111 (falls under 101011)

        let key_op2 = keypath_from_bits(&key_op2_bits);

        let ops = vec![
            (key_op1, Some(ValueHash::default())), // Op under prefix path
            (key_op2, Some(ValueHash::default())), // Op under longer path
        ];

        // Ensure ops are sorted (should be by construction here)
        assert!(ops[0].0 < ops[1].0);

        // This should trigger the error.
        match verify_update::<Blake3Hasher>(&verified_proof, ops).unwrap_err() {
            MultiVerifyUpdateError::PathPrefixOfAnother => (),
            _ => panic!(),
        }
    }
}
