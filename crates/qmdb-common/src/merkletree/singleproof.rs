//! Compact single-entry Merkle proof verification (no_std compatible).
//!
//! This module provides the `SingleProof` structure and verification logic
//! that can be used in both std and no_std environments.

extern crate alloc;

use alloc::vec::Vec;

use crate::merkletree::hash::merkle_node_hash_inplace;

pub use crate::utils::hasher::AlignedHash32;

/// Compact single-entry Merkle proof.
///
/// This proof format yields a classic compact Merkle path consisting only of
/// sibling hashes from the leaf up to the global root. It is intended for
/// historical-existence proofs where activeness is not required.
///
/// Structure:
/// - Left-of-twig path: 11 sibling hashes (levels 0-10)
/// - Join at level 11: one sibling (the right subtree's top hash = active_bits_mtl3)
/// - Upper path: siblings from TWIG_ROOT_LEVEL up to the global root
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SingleProof {
    /// The serial number being proven.
    pub serial_num: u64,
    /// Global leaf index for classic verification: `twig_id * 4096 + pos`.
    pub index: u64,
    /// Leaf hash (entry hash) at the start of the path.
    pub leaf_hash: [u8; 32],
    /// Sibling hashes from leaf up to the global root, in order.
    /// Uses AlignedHash32 to ensure 8-byte alignment for efficient u64 operations.
    pub siblings: Vec<AlignedHash32>,
    /// Expected global root hash.
    pub root: [u8; 32],
}

impl SingleProof {
    /// Returns the number of siblings in the proof.
    #[inline]
    pub fn len(&self) -> usize {
        self.siblings.len()
    }

    /// Returns true if the proof has no siblings.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.siblings.is_empty()
    }
}

/// Verifies a `SingleProof` using QMDb's level-aware node hashing.
///
/// The verifier follows the classic sibling concatenation rule using the
/// provided `index`, hashing upward and comparing to `root`.
///
/// # Algorithm
///
/// 1. Start with `leaf_hash` as the accumulator
/// 2. For each sibling at step `i`:
///    - Determine the children level based on step index
///    - If `index % 2 == 0`, accumulator is left child; else right child
///    - Compute `H(left, right)` as the new accumulator
///    - Divide `index` by 2 to move up one level
/// 3. Compare final accumulator with `root`
///
/// # Returns
///
/// `true` if the proof is valid, `false` otherwise.
#[inline(always)]
pub fn verify_single_proof(sp: &SingleProof) -> bool {
    // Use AlignedHash32 for accumulator (8-byte aligned for u64 operations)
    // Use MaybeUninit for buf_b to avoid zeroing (it will be overwritten by hash)
    let mut buf_a = AlignedHash32::new(sp.leaf_hash);
    let mut buf_b = core::mem::MaybeUninit::<AlignedHash32>::uninit();
    // SAFETY: buf_b will be written before read in the first iteration
    let buf_b_ref = unsafe { buf_b.assume_init_mut() };
    let mut acc = &mut buf_a;
    let mut tmp = buf_b_ref;
    let mut index = sp.index;
    let len = sp.siblings.len();

    // Use raw pointer for direct access (faster than .get() or indexing)
    let ptr = sp.siblings.as_ptr();

    // Manual loop with pointer arithmetic
    let mut step = 0usize;
    while step < len {
        // SAFETY: step < len, AlignedHash32 ensures 8-byte alignment
        let sib: &AlignedHash32 = unsafe { &*ptr.add(step) };

        // Determine left/right order based on index parity
        // Use bitwise AND instead of modulo (faster: 6 vs 8 cycles)
        let is_left = (index & 1) == 0;

        // Compute parent hash into tmp (no per-iteration zeroing)
        if is_left {
            merkle_node_hash_inplace(&mut tmp, step, &acc, sib);
        } else {
            merkle_node_hash_inplace(&mut tmp, step, sib, &acc);
        }
        core::mem::swap(&mut acc, &mut tmp);

        // Use right shift instead of division (faster)
        index >>= 1;
        step += 1;
    }

    // Optimized 32-byte comparison using u64 XOR + OR (no short-circuit)
    hash_eq(&acc.0, &sp.root)
}

/// Fast 32-byte hash comparison using u64 XOR + OR.
/// XOR all words, OR results together - non-zero means mismatch.
/// Avoids branch prediction issues from short-circuit &&.
#[inline(always)]
fn hash_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    // SAFETY: AlignedHash32 ensures 8-byte alignment
    let a_u64: &[u64; 4] = unsafe { core::mem::transmute(a) };
    let b_u64: &[u64; 4] = unsafe { core::mem::transmute(b) };

    // XOR each word, OR together - result is 0 only if all words match
    let diff = (a_u64[0] ^ b_u64[0])
        | (a_u64[1] ^ b_u64[1])
        | (a_u64[2] ^ b_u64[2])
        | (a_u64[3] ^ b_u64[3]);
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merkletree::hash::merkle_node_hash;
    use alloc::vec;

    #[test]
    fn test_single_proof_empty() {
        let proof = SingleProof {
            serial_num: 0,
            index: 0,
            leaf_hash: [0x42; 32],
            siblings: vec![],
            root: [0x42; 32], // With no siblings, root should equal leaf
        };
        assert!(verify_single_proof(&proof));
    }

    #[test]
    fn test_single_proof_one_sibling() {
        let leaf = [0x11; 32];
        let sibling = [0x22; 32];

        // Index 0 (even): leaf is left child
        let root_left = merkle_node_hash(0, &leaf, &sibling);
        let proof_left = SingleProof {
            serial_num: 0,
            index: 0,
            leaf_hash: leaf,
            siblings: vec![AlignedHash32::new(sibling)],
            root: root_left,
        };
        assert!(verify_single_proof(&proof_left));

        // Index 1 (odd): leaf is right child
        let root_right = merkle_node_hash(0, &sibling, &leaf);
        let proof_right = SingleProof {
            serial_num: 1,
            index: 1,
            leaf_hash: leaf,
            siblings: vec![AlignedHash32::new(sibling)],
            root: root_right,
        };
        assert!(verify_single_proof(&proof_right));
    }

    #[test]
    fn test_single_proof_invalid_root() {
        let proof = SingleProof {
            serial_num: 0,
            index: 0,
            leaf_hash: [0x42; 32],
            siblings: vec![AlignedHash32::new([0x11; 32])],
            root: [0xFF; 32], // Wrong root
        };
        assert!(!verify_single_proof(&proof));
    }

    #[test]
    fn test_single_proof_multi_level() {
        // Build a proof with 3 siblings (levels 0, 1, 2)
        // index = 5 (binary: 101)
        // Level 0: index=5 (odd) -> sibling is left
        // Level 1: index=2 (even) -> sibling is right
        // Level 2: index=1 (odd) -> sibling is left

        let leaf = [0xAA; 32];
        let sib0 = [0x00; 32];
        let sib1 = [0x11; 32];
        let sib2 = [0x22; 32];

        // Build expected root
        let n0 = merkle_node_hash(0, &sib0, &leaf); // index=5 odd, sib is left
        let n1 = merkle_node_hash(1, &n0, &sib1); // index=2 even, sib is right
        let root = merkle_node_hash(2, &sib2, &n1); // index=1 odd, sib is left

        let proof = SingleProof {
            serial_num: 5,
            index: 5,
            leaf_hash: leaf,
            siblings: vec![
                AlignedHash32::new(sib0),
                AlignedHash32::new(sib1),
                AlignedHash32::new(sib2),
            ],
            root,
        };
        assert!(verify_single_proof(&proof));
    }
}
