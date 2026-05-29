//! Implementation of Merkle tree twigs - the leaf-level subtrees that form the base of the tree.
//!
//! A twig is a subtree that manages a fixed number of leaf nodes (entries) and their active status.
//! Each twig consists of:
//! 1. A left subtree containing the actual entry hashes
//! 2. A right subtree containing active bits that track which entries are currently valid
//!
//! The structure provides efficient operations for:
//! - Managing entry activation status
//! - Incremental updates of the Merkle tree
//! - Proof generation for entries within the twig
//!

use crate::def::LEAF_COUNT_IN_TWIG;
use crate::merkletree::activebits::ActiveBits;
use crate::merkletree::hash::merkle_node_hash;
use crate::merkletree::null::{get_in_null_mt, NULL_TWIG};
use crate::utils::hasher::{Hash32, ZERO_HASH32};
use alloc::boxed::Box;
use once_cell::race::OnceBox;

pub type TwigMT = [Hash32]; // size is 4096

static NULL_TWIG_BOX: OnceBox<Box<Twig>> = OnceBox::new();
pub fn get_null_twig_box() -> &'static Box<Twig> {
    NULL_TWIG_BOX.get_or_init(|| Box::new(Box::new(NULL_TWIG.clone())))
}

/// Represents a twig in the Merkle tree, which is a subtree managing 2048 entries
///
/// Each twig maintains:
/// 1. A Merkle tree(left tree) for entry hashes
/// 2. A Merkle tree(right tree) for active bits (tracking which entries are valid)
/// 3. The combined root hash of the entire twig
#[repr(align(8))]
#[derive(Clone, Debug, PartialEq)]
pub struct Twig {
    /// Level 1 (8) hashes of the active bits Merkle tree
    pub active_bits_mtl1: [Hash32; 4],
    /// Level 2 (9) hashes of the active bits Merkle tree
    pub active_bits_mtl2: [Hash32; 2],
    /// Level 3 (10) hash of the active bits Merkle tree
    pub active_bits_mtl3: Hash32,
    /// Root hash of the left subtree containing entry hashes
    pub left_root: Hash32,
    /// Root hash of the entire twig (combination of left_root and active_bits_mtl3)
    pub twig_root: Hash32,
}

/// Synchronizes a range of nodes in a twig's Merkle tree
///
/// This function updates the internal nodes of the tree after leaf changes.
///
/// # Arguments
/// * `mtree` - The Merkle tree array to update
/// * `start` - Starting leaf index to sync from
/// * `end` - Ending leaf index to sync to
pub fn sync_mtree(mtree: &mut TwigMT, start: i32, end: i32) {
    let mut cur_start = start;
    let mut cur_end = end;
    let mut level = 0;
    let mut base = LEAF_COUNT_IN_TWIG as i32;
    let mut node = [0u8; 32];
    while base >= 2 {
        let mut end_round = cur_end;
        if cur_end & 1 == 1 {
            end_round += 1;
        };
        let mut j = (cur_start >> 1) << 1; //clear the lowest bit of cur_start
        while j <= end_round && j + 1 < base {
            let i = (base + j) as usize;
            node = merkle_node_hash(level, &mtree[i], &mtree[i + 1]);
            mtree[i / 2].copy_from_slice(&node);
            j += 2;
        }
        cur_start >>= 1;
        cur_end >>= 1;
        level += 1;
        base >>= 1;
    }
}

/// Get the null hash for a node in the left tree at a specific level and position
///
/// # Arguments
/// * `level` - The level in the tree (0-11)
/// * `nth` - The position within the level
///
/// # Returns
/// The null hash for the specified position
///
/// For any level L, its start index is: 1 << (11 - L)
pub fn get_null_mt_hash(level: u8, nth: u64) -> &'static [u8; 32] {
    assert!(level <= 11, "Invalid level for twig Merkle tree: {level}");
    let level_start = 1 << (11 - level); // Simple bit shift to get start index
    get_in_null_mt(level_start + nth as usize)
}

impl Default for Twig {
    fn default() -> Self {
        Self::new()
    }
}

impl Twig {
    /// Creates a new empty twig with all fields initialized to zero
    pub fn new() -> Self {
        Self {
            active_bits_mtl1: [*ZERO_HASH32; 4],
            active_bits_mtl2: [*ZERO_HASH32; 2],
            active_bits_mtl3: *ZERO_HASH32,
            left_root: *ZERO_HASH32,
            twig_root: *ZERO_HASH32,
        }
    }

    /// Synchronizes a level 1 (level 8 if seeing the tree from the left) node in
    /// the active bits Merkle tree
    ///
    /// # Arguments
    /// * `pos` - Position of the node to sync (0-3)
    /// * `active_bits` - The active bits structure containing entry status
    pub fn sync_l1(&mut self, pos: i32, active_bits: &ActiveBits) {
        match pos {
            0 => {
                self.active_bits_mtl1[0] =
                    merkle_node_hash(8, active_bits.get_bits(0, 32), active_bits.get_bits(1, 32))
            }
            1 => {
                self.active_bits_mtl1[1] =
                    merkle_node_hash(8, active_bits.get_bits(2, 32), active_bits.get_bits(3, 32))
            }
            2 => {
                self.active_bits_mtl1[2] =
                    merkle_node_hash(8, active_bits.get_bits(4, 32), active_bits.get_bits(5, 32))
            }
            3 => {
                self.active_bits_mtl1[3] =
                    merkle_node_hash(8, active_bits.get_bits(6, 32), active_bits.get_bits(7, 32))
            }
            _ => panic!("Can not reach here!"),
        }
    }

    /// Synchronizes a level 2 (9) node in the active bits Merkle tree
    ///
    /// # Arguments
    /// * `pos` - Position of the node to sync (0-1)
    pub fn sync_l2(&mut self, pos: i32) {
        match pos {
            0 => {
                self.active_bits_mtl2[0] =
                    merkle_node_hash(9, &self.active_bits_mtl1[0], &self.active_bits_mtl1[1])
            }
            1 => {
                self.active_bits_mtl2[1] =
                    merkle_node_hash(9, &self.active_bits_mtl1[2], &self.active_bits_mtl1[3])
            }
            _ => panic!("Can not reach here!"),
        }
    }

    /// Synchronizes the level 3 (10) node in the active bits Merkle tree
    pub fn sync_l3(&mut self) {
        self.active_bits_mtl3 =
            merkle_node_hash(10, &self.active_bits_mtl2[0], &self.active_bits_mtl2[1]);
    }

    /// Synchronizes the twig root by combining the left_root and active_bits_mtl3
    pub fn sync_top(&mut self) {
        self.twig_root = merkle_node_hash(11, &self.left_root, &self.active_bits_mtl3);
    }
}

#[cfg(test)]
mod twig_tests {
    use crate::merkletree::{
        activebits::ActiveBits,
        null::{get_in_null_mt, NULL_NODE_IN_HIGHER_TREE, NULL_TWIG},
        twig::Twig,
    };

    #[test]
    #[ignore = "hash"]
    fn test_sync() {
        let mut twig = Twig::new();
        let mut active_bits = ActiveBits::default();
        // Set u64 values to produce same byte pattern as old test (0, 1, 2, ..., 254)
        for i in 0..32 {
            let mut val: u64 = 0;
            for j in 0..8 {
                let byte_idx = i * 8 + j;
                if byte_idx < 255 {
                    val |= (byte_idx as u64) << (j * 8);
                }
            }
            active_bits.0[i] = val;
        }

        twig.sync_l1(0, &active_bits);
        twig.sync_l1(1, &active_bits);
        twig.sync_l1(2, &active_bits);
        twig.sync_l1(3, &active_bits);
        assert_eq!(
            "ebdc6bccc0d70075f48ab3c602652a1787d41c05f5a0a851ffe479df0975e683",
            hex::encode(twig.active_bits_mtl1[0])
        );
        assert_eq!(
            "3eac125482e6c5682c92af7dd633d9e99d027cf3f53237b46e2507ca2c9cd599",
            hex::encode(twig.active_bits_mtl1[1])
        );
        assert_eq!(
            "e208457ddd8f66e95ea947bc1beb5c463de054daa3f0ae1c3682a973c1861a32",
            hex::encode(twig.active_bits_mtl1[2])
        );
        assert_eq!(
            "e8b9fd47cce5df56b8d4b0b098af1b49ff3ea97d0c093c8ef6eccb34ae73ac8f",
            hex::encode(twig.active_bits_mtl1[3])
        );

        twig.sync_l2(0);
        twig.sync_l2(1);
        assert_eq!(
            "cf1a0078d5a94742b42bf05d301919b5ae89c155fc1e68a08d260e7ec27c967e",
            hex::encode(twig.active_bits_mtl2[0])
        );
        assert_eq!(
            "cf1a0078d5a94742b42bf05d301919b5ae89c155fc1e68a08d260e7ec27c967e",
            hex::encode(twig.active_bits_mtl2[0])
        );

        twig.sync_l3();
        assert_eq!(
            "d911c0d3beffe478f28b2ebc7cb824ad02ff2793534f37a0c6ddaf9d84527a66",
            hex::encode(twig.active_bits_mtl3)
        );

        twig.left_root = [88; 32];
        twig.sync_top();
        assert_eq!(
            "9312922a448932555a5f1d07b98f422fc0a4259e450f7536161b8ef8ddc96e08",
            hex::encode(twig.twig_root)
        );
    }

    #[test]
    #[ignore = "hash"]
    fn test_init_data() {
        // Each hash corresponds to the first node at each level
        let null_twigmt_hashes = [
            "cce5498796e1da850e39978e5e7bc572779e8ddc5eca8532aa8d28eb8b9fa839", // 1
            "6625f6aa53d328b2572979b52d98b376f26d86ead0fc89b386d4ed026e944e42", // 2
            "c6085473880d2de6339201f1855d088c7a7fc74ab884c5bcc8b851d202328646", // 4
            "730bf342c9b3d3e9a5ecd86d26d9bb3333a6038a110455bd98cae0b91284a50b", // 8
            "122c8ce9fa6aaa67e3afa2e1b47a704ad12c1e6608b2a21e84fd19bd07c30713", // 16
            "692f5b1dc974510438da37d0c46c8e39946a79af1246fe6fbc3f44fc80bc40c3", // 32
            "053d9c73883c8ee7eac9cf011458c61433bbd4bba561e3ddc3f49cf76e52e288", // 64
            "7c81680ffb753a36d9e0b345f308fd818a402b0ecb5e1366cc94991a56075044", // 128
            "e4c3f379b7a5789594c9109e9896aecf749b85c4a6a0b3d26a2c697e26f36fd3", // 256
            "065ac2fd5a856e8e35e104a78235fc5f8c7e75fabbf8064cda207c4babbeb56c", // 512
            "28c0cc1650e8b10b29de7eb17201be478391272380e55745fd52d5feb8554eaa", // 1024
            "ca2337691033ab0a24c10fbc70b49bea8c5978db1a0ec6510e7e97f528301c39", // 2048
        ];
        for (i, &hash) in null_twigmt_hashes.iter().enumerate() {
            assert_eq!(hash, hex::encode(get_in_null_mt(2usize.pow(i as u32))));
        }

        assert_eq!(
            "37f6d34b5f4fe4aba10fd7411d6f58efc4bf844935c37dbe83c5686ceb62ce9d",
            hex::encode(NULL_TWIG.twig_root)
        );

        assert_eq!(
            "c787c83f6f8402c636a2f48f1bf2c02ceb31ea5ccdd4bd9e6fe6efcc3031b640",
            hex::encode(NULL_NODE_IN_HIGHER_TREE[63])
        );
    }
}
