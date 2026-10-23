//! Multi-proof generation, serialization and verification for QMDb Merkle tree.
//!
//! This implements a compact multi-proof similar in spirit to Bitcoin Core's
//! CPartialMerkleTree, adapted to QMDb's two-branch twig structure:
//! - Left subtree: 2048 entry leaves per twig
//! - Right subtree: 8 leaves (each 32 bytes) encoding 2048 active bits
//! - Twig root = H_11(left_root, active_bits_mtl3)
//! - Upper tree: twig roots combined up to the global root
//!
//! The multi-proof proves a set of serial numbers across arbitrary twigs and
//! verifies both their membership and that their corresponding active bits are set.
//!
//! Differences vs single-entry proof (`singleproof.rs`):
//! - Purpose: Batch verification across many serial numbers and twigs.
//! - Guarantees: Proves membership of all included leaves and that each corresponding active bit is set, per-twig, and anchors to the global root.
//! - How: Builds compact PartialMerkleTrees for both left (entries) and right (active-bits) subtrees per twig, then a PartialMerkleTree over twig roots.
//! - Security: Activeness is enforced; a right-side proof is required and checked for each matched left leaf.
//! - Size/perf: Larger than single-entry proofs but asymptotically efficient when proving many leaves; avoids sending full trees by using matched-node selection and bitsets.
//! - When to use: Any context requiring current-state membership with activeness, or when proving multiple entries efficiently.

use super::tree::Tree;
use anyhow::Result;
use hashbrown::HashMap;
use qmdb_common::def::{LEAF_COUNT_IN_TWIG, TWIG_ROOT_LEVEL, TWIG_SHIFT};
use qmdb_common::merkletree::multiproof::{MultiProof, TwigProof};

use qmdb_common::merkletree::multiproof::compute_matched_nodes;

use qmdb_common::merkletree::multiproof::build_partial_merkle_tree;

impl Tree {
    pub fn get_multi_proof(&self, sns: &[u64]) -> Result<MultiProof, String> {
        if sns.is_empty() {
            return Err("empty input".to_string());
        }

        // Ensure tree is not empty
        let n_twigs: u64 = if self.youngest_twig_is_empty {
            self.youngest_twig_id as u64
        } else {
            (self.youngest_twig_id + 1) as u64
        };
        if n_twigs == 0 {
            return Err("empty tree".to_string());
        }

        // Validate and group by twig
        let mut by_twig: HashMap<u64, Vec<u64>> = HashMap::new();
        for &sn in sns {
            let twig_id = sn >> TWIG_SHIFT;
            let pos = (sn & (LEAF_COUNT_IN_TWIG as u64 - 1)) as u64;
            if twig_id > self.youngest_twig_id {
                return Err("twig_id > youngest_twig_id".to_string());
            }
            if self.youngest_twig_is_empty && twig_id == self.youngest_twig_id {
                return Err("youngest twig is empty".to_string());
            }
            by_twig.entry(twig_id).or_default().push(pos);
        }

        // Shared node cache reused across all get_hash_by_node calls to avoid recomputation
        let mut node_cache: std::collections::HashMap<i64, [u8; 32]> =
            std::collections::HashMap::new();

        // Build per-twig proofs
        let mut twig_proofs = Vec::with_capacity(by_twig.len());
        for (twig_id, poses) in by_twig.iter() {
            // left matched nodes directly from positions iterator
            let left_matched_nodes =
                compute_matched_nodes(LEAF_COUNT_IN_TWIG as u64, poses.iter().copied());

            // right matched nodes (chunk indices) from mapped iterator
            let right_matched_nodes =
                compute_matched_nodes(8u64, poses.iter().copied().map(|p| p / 256));

            let this_twig = *twig_id as u64;

            // Helper to get node hash for left subtree at (level, pos)
            let tree_ref = self;
            let node_left = |level: u8, pos: u64| -> [u8; 32] {
                let lvl = level; // absolute level equals local level 0..=10
                let level_stride = 4096u64 >> lvl;
                let nth_global = this_twig * level_stride + pos as u64;
                tree_ref.get_hash_by_node(lvl, nth_global, &mut node_cache)
            };

            let (left_hashes, left_bits_bytes, left_bits_len_usize) = build_partial_merkle_tree(
                LEAF_COUNT_IN_TWIG as u64,
                &left_matched_nodes,
                node_left,
            );

            // Helper to get node hash for right subtree at absolute level L=8..=11
            let node_right = |level: u8, pos: u64| -> [u8; 32] {
                let lvl = 8u8 + level; // local level 0..3 mapped to absolute 8..11
                let level_stride = 4096u64 >> lvl;
                let nth_global = this_twig * level_stride + (level_stride / 2) + pos as u64;
                tree_ref.get_hash_by_node(lvl, nth_global, &mut node_cache)
            };
            let (right_hashes, right_bits_bytes, right_bits_len_usize) =
                build_partial_merkle_tree(8u64, &right_matched_nodes, node_right);

            twig_proofs.push(TwigProof {
                twig_id: *twig_id,
                left_hashes,
                left_bits_bytes,
                left_bits_len: left_bits_len_usize as u64,
                right_hashes,
                right_bits_bytes,
                right_bits_len: right_bits_len_usize as u64,
            });
        }

        // Build upper PartialMerkleTree over twig roots using keys iterator
        let upper_matched_nodes = compute_matched_nodes(n_twigs as u64, by_twig.keys().copied());

        let tree_ref = self;
        let node_upper = |height: u8, pos: u64| -> [u8; 32] {
            // height 0 => level TWIG_ROOT_LEVEL, pos is twig_id
            let level = TWIG_ROOT_LEVEL as u8 + height;
            // For upper levels, nth at a given level is 'pos'
            tree_ref.get_hash_by_node(level, pos as u64, &mut node_cache)
        };
        let (upper_hashes, upper_bits_bytes, upper_bits_len_usize) =
            build_partial_merkle_tree(n_twigs as u64, &upper_matched_nodes, node_upper);

        // Ensure canonical ordering: twigs sorted by twig_id
        twig_proofs.sort_by_key(|tp| tp.twig_id);

        Ok(MultiProof {
            root: self.get_root_hash(),
            n_twigs: n_twigs as u64,
            upper_hashes,
            upper_bits_bytes,
            upper_bits_len: upper_bits_len_usize as u64,
            twigs: twig_proofs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use borsh::BorshDeserialize;

    use qmdb_common::merkletree::hash::merkle_node_hash;
    use qmdb_common::merkletree::multiproof::pack_bits;

    fn make_hash(x: u8) -> [u8; 32] {
        [x; 32]
    }

    // Helper to compute node hash with a null-right policy for arbitrary n
    fn calc_node_hash_qmdb(
        children_level: u8,
        left: [u8; 32],
        right: Option<[u8; 32]>,
    ) -> [u8; 32] {
        let r = right.unwrap_or([0; 32]);
        merkle_node_hash(children_level, &left, &r)
    }

    fn full_root_qmdb(leaves: &[[u8; 32]]) -> [u8; 32] {
        use qmdb_common::merkletree::multiproof::height_for_n;
        fn rec(height: u8, pos: u64, leaves: &[[u8; 32]], n: u64) -> [u8; 32] {
            if height == 0 {
                return leaves[pos as usize];
            }
            let left = rec(height - 1, pos * 2, leaves, n);
            let nodes_at_hm1 = ((n - 1) >> (height - 1)) + 1;
            let right = if pos * 2 + 1 < nodes_at_hm1 {
                rec(height - 1, pos * 2 + 1, leaves, n)
            } else {
                [0; 32]
            };
            calc_node_hash_qmdb(height - 1, left, Some(right))
        }
        let n = leaves.len() as u64;
        let h: u8 = height_for_n(n);
        rec(h, 0, leaves, n)
    }

    #[test]
    fn test_multiproof_serde_roundtrip() {
        // Build small example using packed bits
        let upper_bits_vec = vec![
            true, false, true, true, false, false, true, false, true, true,
        ];
        let upper_bits_len = upper_bits_vec.len();
        let upper_bits_bytes = pack_bits(&upper_bits_vec);
        let left_bits_vec = vec![true, true, false, true, false, true, false, false, true];
        let left_bits_len = left_bits_vec.len();
        let left_bits_bytes = pack_bits(&left_bits_vec);
        let right_bits_vec = vec![false, true, true, false, true];
        let right_bits_len = right_bits_vec.len();
        let right_bits_bytes = pack_bits(&right_bits_vec);

        let mp = MultiProof {
            root: make_hash(1),
            n_twigs: 3u64,
            upper_hashes: vec![make_hash(2), make_hash(3)],
            upper_bits_bytes,
            upper_bits_len: upper_bits_len as u64,
            twigs: vec![TwigProof {
                twig_id: 0,
                left_hashes: vec![make_hash(10), make_hash(11), make_hash(12)],
                left_bits_bytes,
                left_bits_len: left_bits_len as u64,
                right_hashes: vec![make_hash(20), make_hash(21)],
                right_bits_bytes,
                right_bits_len: right_bits_len as u64,
            }],
        };
        let bz = borsh::to_vec(&mp).expect("encode");
        let mp2 = MultiProof::try_from_slice(&bz).expect("decode ok");
        assert_eq!(mp, mp2);
    }

    #[test]
    fn test_multiproof_verify_rejects_invalid() {
        // Use the roundtrip sample (not a valid proof) and ensure verify() rejects it.
        let upper_bits_vec = vec![
            true, false, true, true, false, false, true, false, true, true,
        ];
        let upper_bits_len = upper_bits_vec.len();
        let upper_bits_bytes = pack_bits(&upper_bits_vec);
        let left_bits_vec = vec![true, true, false, true, false, true, false, false, true];
        let left_bits_len = left_bits_vec.len();
        let left_bits_bytes = pack_bits(&left_bits_vec);
        let right_bits_vec = vec![false, true, true, false, true];
        let right_bits_len = right_bits_vec.len();
        let right_bits_bytes = pack_bits(&right_bits_vec);

        let mp = MultiProof {
            root: make_hash(1),
            n_twigs: 3u64,
            upper_hashes: vec![make_hash(2), make_hash(3)],
            upper_bits_bytes,
            upper_bits_len: upper_bits_len as u64,
            twigs: vec![TwigProof {
                twig_id: 0,
                left_hashes: vec![make_hash(10), make_hash(11), make_hash(12)],
                left_bits_bytes,
                left_bits_len: left_bits_len as u64,
                right_hashes: vec![make_hash(20), make_hash(21)],
                right_bits_bytes,
                right_bits_len: right_bits_len as u64,
            }],
        };
        assert!(mp.verify().is_err());
    }

    /// Helper to sync tree hashes after building (same as in test_proof.rs)
    fn sync_tree(tree: &mut Tree) {
        let n_list = tree.flush_files(0, 0);
        let n_list = tree.upper_tree.evict_twigs(n_list, 0, 0);
        tree.upper_tree
            .sync_upper_nodes(n_list, tree.youngest_twig_id);
    }

    /// End-to-end test: build a real tree, generate multi-proof, verify it
    /// Uses same tree size as test_proof.rs for consistency
    #[test]
    fn test_multiproof_end_to_end() {
        use crate::merkletree::helpers::build_test_tree;
        use qmdb_common::def::TWIG_MASK;
        use std::collections::HashSet;

        // Create a temporary directory for tree files
        let tmp_dir = tempfile::Builder::new()
            .prefix("DataTree-multiproof")
            .tempdir()
            .unwrap();
        let dir_path = tmp_dir.path().to_str().unwrap();

        // Build a tree with enough entries to span multiple twigs (same as test_proof.rs)
        // TWIG_MASK = 2047, so TWIG_MASK * 4 = 8188 entries across ~4 twigs
        let (mut tree, _, _, _) = build_test_tree(dir_path, &vec![], TWIG_MASK as i32 * 4, 1600);

        // Sync tree hashes
        sync_tree(&mut tree);

        // Select some serial numbers to prove (from different twigs)
        // Twig 0: sn 0..2047, Twig 1: sn 2048..4095, etc.
        let sns_to_prove: Vec<u64> = vec![
            100,  // Twig 0
            2100, // Twig 1
            4200, // Twig 2
            6300, // Twig 3
            8000, // Twig 3 (near end)
        ];

        // Step 1: Generate multi-proof
        let proof = tree
            .get_multi_proof(&sns_to_prove)
            .expect("get_multi_proof should succeed");

        // Verify proof has expected structure
        assert_eq!(
            proof.root,
            tree.get_root_hash(),
            "Proof root should match tree root"
        );
        assert!(
            !proof.twigs.is_empty(),
            "Proof should have at least one twig"
        );

        // Step 2: Verify the proof
        let verified_pairs = proof.verify().expect("verify should succeed");

        // Step 3: Check results
        let requested_set: HashSet<u64> = sns_to_prove.iter().copied().collect();
        let verified_set: HashSet<u64> = verified_pairs.iter().map(|(sn, _)| *sn).collect();

        assert_eq!(
            requested_set, verified_set,
            "Verified SNs should match requested SNs"
        );

        // Verify each pair has a non-zero hash
        for (sn, hash) in &verified_pairs {
            assert!(
                hash != &[0u8; 32],
                "Hash for sn {} should not be all zeros",
                sn
            );
        }

        println!(
            "End-to-end test passed: proved {} entries across {} twigs",
            verified_pairs.len(),
            proof.twigs.len()
        );
    }

    /// Test multi-proof with entries from single twig
    #[test]
    fn test_multiproof_single_twig() {
        use crate::merkletree::helpers::build_test_tree;
        use qmdb_common::def::TWIG_MASK;

        let tmp_dir = tempfile::Builder::new()
            .prefix("DataTree-multiproof-single")
            .tempdir()
            .unwrap();
        let dir_path = tmp_dir.path().to_str().unwrap();

        // Build a tree with entries in a single twig
        let (mut tree, _, _, _) = build_test_tree(
            dir_path,
            &vec![],
            TWIG_MASK as i32, // 2047 entries, all in twig 0
            0,
        );

        // Sync tree hashes
        sync_tree(&mut tree);

        // Prove multiple entries from twig 0
        let sns = vec![10u64, 100u64, 500u64, 1000u64, 2000u64];
        let proof = tree
            .get_multi_proof(&sns)
            .expect("get_multi_proof should succeed");

        assert_eq!(proof.twigs.len(), 1, "Should have exactly 1 twig proof");

        let verified = proof.verify().expect("verify should succeed");
        assert_eq!(verified.len(), sns.len());
    }

    /// Test that proof fails with tampered root
    #[test]
    fn test_multiproof_tampered_root_fails() {
        use crate::merkletree::helpers::build_test_tree;
        use qmdb_common::def::TWIG_MASK;

        let tmp_dir = tempfile::Builder::new()
            .prefix("DataTree-multiproof-tamper")
            .tempdir()
            .unwrap();
        let dir_path = tmp_dir.path().to_str().unwrap();

        let (mut tree, _, _, _) = build_test_tree(dir_path, &vec![], TWIG_MASK as i32, 0);

        // Sync tree hashes
        sync_tree(&mut tree);

        let sns = vec![10u64, 20u64, 30u64];
        let mut proof = tree
            .get_multi_proof(&sns)
            .expect("get_multi_proof should succeed");

        // Tamper with the root
        proof.root[0] ^= 0xFF;

        // Verification should fail
        assert!(
            proof.verify().is_err(),
            "Tampered proof should fail verification"
        );
    }
}
