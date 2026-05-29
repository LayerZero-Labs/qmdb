//! Precomputed null values for merkle tree optimization.
//!
//! This module provides precomputed static values representing null states
//! for various merkle tree components in QMDB's right-most uninitialized tree.
//! These values are computed at compile time to avoid runtime overhead when
//! initializing empty merkle tree components.
//! In this module, the functions are only used for generating the static variables.
//!
//! # Components
//!
//! - **Null Merkle Tree**: Precomputed tree of null entry hashes for empty twigs
//! - **Null Twig**: Complete twig structure in empty state
//! - **Null Higher Tree Nodes**: Hash values for empty nodes above twig level
//! - **Null Active Bits**: Default active bits structure in empty twigs

use crate::{
    def::{ENTRY_BASE_LENGTH, FIRST_LEVEL_ABOVE_TWIG, TWIG_ROOT_LEVEL},
    entryfile::entry,
    merkletree::{
        activebits::ActiveBits,
        diffmap::DiffMap,
        hash::merkle_node_hash,
        twig::{get_null_mt_hash, sync_mtree, Twig, TwigMT},
    },
    utils::hasher::{Hash32, ZERO_HASH32},
};
use alloc::boxed::Box;
use alloc::vec;
use once_cell::race::OnceBox;

/// Precomputed diff representation of a null merkle tree for twigs.
///
/// This static array contains the compressed diff format of a complete null merkle tree
/// used within twigs' left trees.
///
/// The array represents a 4096-node tree where:
/// - Level 0 (leaves): 2048 nodes with null entry hashes
/// - Levels 1-11: Internal nodes computed from null leaves
/// - Each tuple contains (index, hash) pairs marking where values change
///
/// This is used to initialize `get_null_mt_for_twig()` via the `DiffMap` structure.
static NULL_MT_FOR_TWIG_DIFF: [(usize, [u8; 32]); 13] = [
    (
        0,
        [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0,
        ],
    ),
    (
        1,
        [
            52, 157, 61, 234, 93, 105, 18, 246, 215, 235, 91, 179, 199, 224, 103, 43, 14, 50, 187,
            62, 99, 131, 172, 212, 65, 174, 210, 94, 125, 208, 138, 114,
        ],
    ),
    (
        2,
        [
            242, 168, 68, 199, 68, 111, 169, 124, 107, 37, 246, 38, 68, 247, 60, 79, 127, 99, 246,
            70, 125, 191, 32, 20, 107, 46, 5, 140, 206, 93, 14, 234,
        ],
    ),
    (
        4,
        [
            226, 107, 29, 235, 68, 230, 205, 59, 237, 205, 60, 180, 43, 176, 102, 142, 94, 27, 104,
            192, 61, 208, 69, 156, 4, 178, 66, 119, 53, 99, 30, 247,
        ],
    ),
    (
        8,
        [
            210, 44, 1, 95, 216, 196, 52, 184, 27, 50, 64, 164, 100, 212, 205, 100, 103, 84, 209,
            32, 184, 91, 30, 187, 116, 60, 33, 184, 120, 218, 139, 161,
        ],
    ),
    (
        16,
        [
            251, 80, 193, 238, 2, 221, 20, 2, 44, 117, 248, 150, 138, 94, 79, 81, 84, 180, 4, 213,
            20, 199, 51, 61, 74, 208, 181, 239, 4, 137, 55, 19,
        ],
    ),
    (
        32,
        [
            251, 151, 202, 17, 80, 191, 177, 105, 146, 7, 152, 127, 113, 145, 212, 178, 185, 105,
            228, 141, 128, 41, 9, 208, 19, 213, 107, 110, 174, 55, 149, 105,
        ],
    ),
    (
        64,
        [
            98, 71, 121, 251, 164, 228, 1, 139, 23, 30, 146, 82, 94, 37, 10, 33, 181, 192, 230,
            115, 221, 105, 198, 8, 70, 117, 218, 156, 143, 168, 161, 90,
        ],
    ),
    (
        128,
        [
            54, 125, 124, 236, 66, 177, 17, 162, 36, 214, 69, 169, 237, 86, 152, 166, 133, 31, 144,
            99, 26, 19, 161, 118, 15, 21, 194, 68, 128, 241, 174, 249,
        ],
    ),
    (
        256,
        [
            245, 2, 94, 52, 108, 108, 94, 191, 5, 60, 247, 49, 59, 209, 15, 22, 219, 9, 125, 239,
            153, 53, 10, 142, 35, 190, 42, 130, 95, 23, 181, 136,
        ],
    ),
    (
        512,
        [
            135, 23, 162, 23, 229, 59, 176, 217, 49, 56, 112, 114, 179, 208, 18, 100, 82, 56, 83,
            94, 55, 226, 201, 82, 174, 204, 46, 61, 172, 43, 85, 63,
        ],
    ),
    (
        1024,
        [
            208, 78, 182, 5, 209, 182, 168, 66, 36, 49, 223, 174, 145, 228, 157, 202, 7, 108, 110,
            210, 140, 131, 206, 116, 36, 32, 84, 230, 131, 82, 0, 39,
        ],
    ),
    (
        2048,
        [
            80, 147, 214, 100, 38, 77, 154, 34, 167, 144, 122, 230, 70, 37, 2, 1, 37, 102, 113,
            165, 226, 89, 52, 175, 235, 105, 15, 50, 120, 8, 55, 145,
        ],
    ),
];

/// Lazy-initialized null merkle tree for twigs.
///
/// This provides a space-efficient representation of a complete null merkle tree
/// containing 4096 nodes. The tree represents the state of an empty twig where
/// all entries are null entries.
///
/// The structure uses `DiffMap` to compress the tree by storing only the change
/// points, since most of the tree contains identical null entry hashes. This
/// provides significant memory savings compared to storing the full 4096-node array.
///
/// # Usage
///
/// This is used as a template when initializing new empty twigs, avoiding the
/// need to compute the null tree structure at runtime.
static NULL_MT_FOR_TWIG: OnceBox<DiffMap> = OnceBox::new();
pub fn get_null_mt_for_twig() -> &'static DiffMap<'static> {
    NULL_MT_FOR_TWIG.get_or_init(|| {
        let null_mt_for_twig = DiffMap::from_diff(&NULL_MT_FOR_TWIG_DIFF, 4096);
        Box::new(null_mt_for_twig)
    })
}

pub fn get_in_null_mt(idx: usize) -> &'static [u8; 32] {
    assert!(idx < 4096);
    let leading_zeros = (idx as u64).leading_zeros() as usize;
    &NULL_MT_FOR_TWIG_DIFF[64 - leading_zeros].1
}

/// Gets the pre-calculated hash of a null/empty node at a specific position
#[inline(always)]
pub fn get_null_hash_by_pos(level: u8, nth: u64) -> &'static [u8; 32] {
    // Levels above twig: directly index precomputed array
    if level > 12 {
        debug_assert!((level as usize) < NULL_NODE_IN_HIGHER_TREE.len());
        return unsafe { NULL_NODE_IN_HIGHER_TREE.get_unchecked(level as usize) };
    }

    // Levels within twig
    let stride: usize = 1usize << (12 - level); // power of two
    let mask = stride - 1;
    let nth_usize: usize = (nth as usize) & mask;

    // Fast paths
    if level == 12 {
        return &NULL_TWIG.twig_root;
    }
    if level == 11 && nth_usize == 0 {
        return &NULL_TWIG.left_root;
    }

    let half_stride = stride >> 1;
    if nth_usize >= half_stride {
        let idx = nth_usize - half_stride;
        return unsafe {
            match level {
                8 => ZERO_HASH32,
                9 => NULL_TWIG.active_bits_mtl1.get_unchecked(idx),
                10 => NULL_TWIG.active_bits_mtl2.get_unchecked(idx),
                11 => &NULL_TWIG.active_bits_mtl3,
                _ => core::hint::unreachable_unchecked(),
            }
        };
    }

    get_null_mt_hash(level, nth_usize as u64)
}

/// Precomputed null twig instance.
///
/// This static twig represents the empty state of a twig structure,
/// including all active bits merkle tree levels and root hashes.
/// The twig's left subtree is not included here, instead it's in get_null_mt_for_twig().
///
/// # Structure
///
/// - `active_bits_mtl1`: Level 1 active bits merkle tree (4 nodes)
/// - `active_bits_mtl2`: Level 2 active bits merkle tree (2 nodes)  
/// - `active_bits_mtl3`: Level 3 active bits merkle tree (1 node)
/// - `left_root`: Root hash of the left subtree (from null merkle tree)
/// - `twig_root`: Root hash of the entire twig
///
/// # Usage
///
/// This serves as a template for initializing new empty twigs, providing
/// instant access to the null state without runtime computation.
pub static NULL_TWIG: Twig = Twig {
    active_bits_mtl1: [
        [
            73, 87, 96, 184, 115, 64, 38, 88, 21, 156, 253, 238, 141, 182, 194, 64, 28, 164, 163,
            123, 1, 188, 58, 229, 192, 245, 21, 122, 65, 178, 246, 58,
        ],
        [
            73, 87, 96, 184, 115, 64, 38, 88, 21, 156, 253, 238, 141, 182, 194, 64, 28, 164, 163,
            123, 1, 188, 58, 229, 192, 245, 21, 122, 65, 178, 246, 58,
        ],
        [
            73, 87, 96, 184, 115, 64, 38, 88, 21, 156, 253, 238, 141, 182, 194, 64, 28, 164, 163,
            123, 1, 188, 58, 229, 192, 245, 21, 122, 65, 178, 246, 58,
        ],
        [
            73, 87, 96, 184, 115, 64, 38, 88, 21, 156, 253, 238, 141, 182, 194, 64, 28, 164, 163,
            123, 1, 188, 58, 229, 192, 245, 21, 122, 65, 178, 246, 58,
        ],
    ],
    active_bits_mtl2: [
        [
            219, 51, 164, 183, 254, 73, 161, 190, 237, 43, 189, 35, 91, 132, 16, 155, 80, 84, 237,
            3, 21, 130, 55, 19, 203, 107, 83, 152, 18, 110, 88, 166,
        ],
        [
            219, 51, 164, 183, 254, 73, 161, 190, 237, 43, 189, 35, 91, 132, 16, 155, 80, 84, 237,
            3, 21, 130, 55, 19, 203, 107, 83, 152, 18, 110, 88, 166,
        ],
    ],
    active_bits_mtl3: [
        240, 147, 113, 150, 250, 238, 99, 109, 181, 151, 127, 147, 45, 12, 140, 130, 69, 204, 127,
        137, 241, 181, 176, 198, 102, 138, 118, 180, 134, 202, 3, 48,
    ],
    left_root: [
        52, 157, 61, 234, 93, 105, 18, 246, 215, 235, 91, 179, 199, 224, 103, 43, 14, 50, 187, 62,
        99, 131, 172, 212, 65, 174, 210, 94, 125, 208, 138, 114,
    ],
    twig_root: [
        125, 19, 129, 63, 154, 60, 143, 10, 186, 27, 20, 210, 191, 17, 173, 253, 64, 241, 104, 247,
        40, 25, 190, 137, 114, 127, 245, 71, 86, 246, 247, 206,
    ],
};

/// Precomputed null node hashes for tree levels above twigs.
///
/// This array contains 64 precomputed hash values representing null/empty nodes
/// at each level of the merkle tree hierarchy above the twig level. Each hash
/// represents what an empty subtree would hash to at that particular level.
///
/// # Structure
///
/// - **Levels 0-12**: All zeros (unused levels)
/// - **Level 13**: First level above twig (computed from null twig root)
/// - **Levels 14-63**: Each level computed from the previous level's null hash
///
/// # Usage
///
/// These values are used when constructing merkle proofs or computing tree roots
/// for trees that contain empty subtrees. Instead of computing these null values
/// at runtime, they can be looked up from this precomputed array.
///
/// # Computation
///
/// Each level i > 13 is computed as: `hash2(level, null_hash[i-1], null_hash[i-1])`
/// where `null_hash[i-1]` represents the null hash from the previous level.
pub static NULL_NODE_IN_HIGHER_TREE: [[u8; 32]; 64] = [
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ],
    [
        83, 23, 148, 31, 55, 61, 39, 161, 91, 124, 17, 116, 217, 9, 10, 199, 74, 80, 229, 136, 127,
        207, 74, 45, 59, 91, 130, 176, 25, 172, 136, 33,
    ],
    [
        61, 151, 19, 222, 86, 144, 76, 136, 85, 94, 221, 58, 72, 250, 160, 134, 211, 119, 138, 76,
        213, 239, 209, 246, 105, 246, 19, 185, 144, 42, 134, 50,
    ],
    [
        148, 147, 11, 109, 7, 221, 89, 204, 103, 53, 241, 111, 98, 103, 112, 173, 133, 196, 243,
        18, 100, 159, 143, 98, 128, 28, 138, 226, 178, 247, 104, 207,
    ],
    [
        75, 10, 170, 172, 243, 142, 231, 124, 121, 242, 124, 61, 161, 89, 213, 69, 18, 124, 30, 11,
        69, 180, 49, 227, 69, 210, 62, 111, 115, 47, 54, 200,
    ],
    [
        43, 230, 2, 169, 103, 73, 167, 120, 125, 117, 102, 13, 124, 246, 183, 235, 65, 46, 45, 143,
        105, 199, 23, 201, 204, 241, 212, 192, 8, 9, 248, 111,
    ],
    [
        28, 152, 217, 123, 6, 15, 79, 168, 39, 172, 205, 215, 93, 160, 53, 41, 61, 213, 12, 121,
        164, 18, 52, 153, 252, 103, 35, 157, 52, 27, 177, 189,
    ],
    [
        189, 73, 233, 138, 71, 48, 191, 40, 151, 123, 86, 253, 205, 77, 94, 232, 236, 89, 1, 225,
        36, 90, 237, 246, 249, 128, 134, 29, 229, 170, 135, 178,
    ],
    [
        21, 58, 34, 146, 222, 65, 246, 44, 179, 156, 103, 165, 75, 210, 184, 101, 105, 77, 95, 128,
        28, 193, 94, 64, 213, 12, 25, 180, 92, 163, 109, 245,
    ],
    [
        15, 246, 95, 116, 39, 142, 154, 102, 239, 131, 164, 199, 216, 237, 128, 231, 254, 69, 82,
        150, 38, 255, 27, 57, 201, 11, 121, 30, 222, 6, 255, 63,
    ],
    [
        237, 213, 192, 7, 253, 100, 26, 176, 189, 83, 134, 251, 28, 38, 110, 137, 125, 129, 32,
        222, 109, 72, 24, 91, 152, 44, 0, 110, 99, 248, 228, 168,
    ],
    [
        125, 174, 235, 194, 2, 166, 251, 165, 31, 234, 67, 223, 23, 238, 159, 67, 198, 223, 136,
        137, 4, 201, 163, 31, 211, 248, 173, 138, 241, 17, 172, 193,
    ],
    [
        157, 202, 244, 197, 227, 62, 220, 61, 53, 58, 114, 72, 231, 8, 34, 23, 45, 8, 115, 183,
        122, 69, 112, 250, 198, 71, 62, 125, 19, 175, 18, 97,
    ],
    [
        37, 166, 59, 93, 166, 125, 53, 232, 94, 96, 72, 91, 237, 114, 249, 44, 69, 237, 237, 99,
        110, 103, 191, 182, 39, 145, 215, 170, 177, 171, 129, 24,
    ],
    [
        51, 105, 119, 213, 150, 52, 197, 104, 62, 190, 219, 231, 254, 200, 119, 153, 243, 159, 27,
        233, 210, 6, 189, 24, 10, 169, 94, 21, 89, 139, 234, 121,
    ],
    [
        242, 157, 51, 126, 19, 88, 109, 167, 230, 151, 251, 214, 175, 241, 187, 29, 164, 49, 85,
        236, 225, 112, 14, 12, 241, 185, 29, 101, 100, 187, 212, 34,
    ],
    [
        239, 140, 232, 177, 167, 14, 0, 5, 122, 142, 37, 210, 27, 148, 92, 152, 237, 254, 43, 12,
        20, 67, 181, 8, 115, 111, 237, 148, 174, 2, 53, 31,
    ],
    [
        252, 62, 221, 64, 144, 201, 81, 118, 184, 197, 96, 97, 60, 10, 198, 194, 141, 203, 240,
        242, 94, 192, 5, 238, 225, 151, 231, 239, 214, 95, 246, 100,
    ],
    [
        138, 191, 156, 182, 163, 154, 18, 117, 76, 35, 103, 190, 113, 176, 2, 40, 91, 255, 206,
        201, 43, 76, 159, 244, 206, 182, 136, 210, 66, 24, 175, 245,
    ],
    [
        118, 140, 0, 6, 158, 94, 40, 132, 216, 116, 223, 135, 205, 50, 57, 85, 228, 55, 21, 86,
        105, 166, 95, 230, 25, 9, 8, 131, 126, 96, 193, 10,
    ],
    [
        51, 232, 214, 170, 28, 204, 1, 250, 174, 136, 160, 167, 3, 77, 25, 250, 175, 222, 27, 90,
        168, 214, 248, 0, 188, 150, 136, 0, 175, 83, 54, 150,
    ],
    [
        87, 77, 93, 29, 106, 143, 173, 111, 140, 182, 93, 118, 240, 112, 142, 143, 102, 113, 161,
        82, 157, 107, 152, 5, 106, 151, 10, 250, 25, 137, 60, 68,
    ],
    [
        225, 212, 197, 80, 155, 34, 32, 207, 32, 222, 122, 74, 88, 164, 19, 238, 170, 190, 138, 57,
        218, 104, 124, 173, 63, 104, 27, 81, 91, 184, 160, 146,
    ],
    [
        182, 46, 93, 96, 175, 206, 55, 212, 137, 192, 80, 79, 138, 115, 140, 182, 66, 41, 72, 97,
        121, 133, 9, 38, 141, 171, 179, 126, 142, 56, 135, 189,
    ],
    [
        160, 212, 56, 49, 76, 119, 230, 170, 79, 179, 15, 191, 223, 231, 194, 97, 247, 214, 31,
        121, 50, 71, 239, 9, 140, 210, 111, 70, 155, 190, 45, 91,
    ],
    [
        58, 208, 11, 224, 89, 17, 159, 234, 42, 222, 88, 39, 145, 218, 127, 36, 120, 179, 205, 155,
        182, 157, 251, 224, 26, 188, 157, 210, 254, 113, 16, 206,
    ],
    [
        108, 236, 76, 25, 115, 198, 150, 214, 233, 157, 29, 222, 123, 249, 0, 248, 147, 22, 213,
        44, 227, 27, 162, 248, 254, 10, 22, 107, 23, 89, 27, 174,
    ],
    [
        73, 63, 210, 209, 57, 1, 172, 139, 215, 8, 221, 151, 93, 187, 255, 127, 167, 140, 61, 43,
        216, 234, 204, 247, 108, 19, 127, 87, 141, 179, 137, 102,
    ],
    [
        68, 84, 202, 192, 128, 28, 117, 209, 184, 46, 35, 143, 173, 195, 243, 92, 6, 47, 35, 151,
        47, 179, 106, 160, 50, 78, 113, 217, 37, 203, 149, 128,
    ],
    [
        63, 252, 243, 46, 252, 26, 34, 86, 134, 15, 243, 9, 87, 113, 24, 141, 134, 145, 244, 61,
        84, 56, 208, 105, 66, 137, 222, 212, 239, 77, 181, 160,
    ],
    [
        136, 170, 232, 25, 55, 54, 70, 130, 8, 187, 85, 152, 208, 22, 94, 31, 231, 228, 188, 235,
        54, 182, 173, 53, 39, 43, 31, 111, 145, 131, 56, 239,
    ],
    [
        201, 211, 16, 1, 235, 216, 46, 127, 11, 82, 40, 246, 161, 246, 188, 143, 148, 153, 69, 243,
        145, 23, 253, 45, 119, 98, 118, 227, 5, 50, 73, 225,
    ],
    [
        113, 97, 201, 41, 72, 157, 132, 248, 167, 12, 38, 229, 76, 206, 112, 19, 199, 245, 169,
        189, 67, 193, 242, 172, 123, 51, 198, 202, 74, 243, 16, 126,
    ],
    [
        166, 151, 160, 182, 243, 205, 137, 69, 70, 153, 147, 19, 196, 224, 84, 226, 92, 186, 31,
        31, 160, 247, 148, 245, 245, 153, 157, 126, 158, 75, 145, 121,
    ],
    [
        31, 41, 115, 121, 73, 186, 147, 241, 207, 28, 63, 128, 185, 229, 34, 69, 91, 144, 99, 60,
        241, 161, 68, 12, 252, 149, 59, 211, 31, 206, 45, 2,
    ],
    [
        69, 128, 164, 109, 176, 251, 249, 198, 186, 173, 75, 224, 201, 92, 108, 17, 127, 73, 144,
        128, 166, 130, 176, 131, 130, 34, 126, 247, 61, 33, 57, 28,
    ],
    [
        52, 222, 47, 43, 41, 191, 26, 215, 214, 177, 46, 74, 225, 225, 7, 236, 210, 71, 22, 188,
        68, 195, 219, 254, 151, 203, 20, 152, 186, 78, 176, 162,
    ],
    [
        145, 57, 147, 101, 37, 74, 84, 201, 57, 100, 188, 192, 228, 13, 51, 85, 87, 130, 228, 227,
        234, 31, 183, 122, 88, 123, 168, 160, 172, 140, 39, 106,
    ],
    [
        40, 89, 112, 17, 162, 175, 242, 25, 64, 247, 172, 48, 102, 163, 247, 188, 4, 137, 55, 9,
        184, 201, 118, 137, 50, 228, 187, 183, 174, 111, 169, 105,
    ],
    [
        181, 251, 82, 115, 246, 7, 203, 29, 123, 127, 86, 147, 161, 46, 232, 255, 132, 214, 165,
        141, 87, 130, 152, 113, 10, 197, 231, 44, 45, 201, 210, 133,
    ],
    [
        122, 86, 13, 236, 219, 146, 185, 157, 150, 250, 235, 137, 193, 142, 57, 34, 177, 176, 213,
        116, 90, 161, 19, 234, 38, 109, 209, 5, 197, 74, 130, 232,
    ],
    [
        60, 162, 172, 139, 228, 14, 187, 171, 168, 99, 56, 31, 80, 28, 248, 151, 118, 88, 24, 161,
        121, 248, 37, 223, 250, 198, 32, 0, 244, 181, 174, 2,
    ],
    [
        7, 169, 77, 109, 211, 145, 150, 209, 124, 68, 25, 135, 102, 224, 197, 120, 202, 242, 69,
        144, 140, 62, 6, 133, 85, 194, 107, 227, 245, 247, 164, 161,
    ],
    [
        88, 29, 106, 233, 223, 7, 127, 24, 142, 0, 136, 133, 87, 105, 48, 31, 218, 179, 183, 64,
        125, 166, 159, 77, 119, 196, 115, 6, 47, 63, 234, 221,
    ],
    [
        99, 18, 119, 251, 152, 100, 63, 133, 188, 33, 26, 84, 16, 204, 10, 77, 46, 32, 39, 129,
        252, 56, 165, 118, 69, 175, 229, 68, 136, 200, 248, 82,
    ],
    [
        137, 134, 54, 15, 123, 227, 132, 4, 109, 133, 218, 117, 190, 65, 210, 54, 113, 77, 46, 57,
        192, 157, 60, 50, 40, 179, 116, 104, 188, 70, 131, 199,
    ],
    [
        122, 45, 34, 38, 201, 14, 186, 118, 24, 17, 78, 61, 74, 54, 103, 230, 195, 17, 218, 2, 59,
        113, 88, 157, 148, 25, 160, 46, 200, 7, 205, 72,
    ],
    [
        21, 44, 99, 169, 46, 183, 59, 249, 168, 244, 115, 92, 77, 137, 20, 235, 10, 243, 156, 57,
        42, 59, 177, 0, 57, 235, 68, 167, 12, 14, 108, 181,
    ],
    [
        201, 224, 201, 153, 80, 149, 234, 79, 4, 129, 83, 137, 216, 162, 172, 75, 211, 157, 228,
        46, 167, 30, 104, 235, 100, 90, 213, 243, 187, 120, 59, 73,
    ],
    [
        82, 10, 2, 55, 195, 192, 219, 59, 241, 194, 115, 76, 238, 76, 240, 228, 199, 187, 136, 66,
        170, 118, 51, 112, 93, 232, 238, 44, 82, 162, 156, 28,
    ],
    [
        68, 55, 32, 159, 98, 94, 85, 70, 177, 6, 50, 246, 168, 93, 157, 28, 182, 167, 142, 26, 78,
        84, 206, 54, 151, 14, 154, 184, 146, 50, 116, 114,
    ],
    [
        18, 95, 227, 96, 243, 10, 211, 34, 187, 124, 174, 185, 155, 82, 101, 216, 31, 219, 105,
        141, 32, 115, 31, 112, 143, 89, 174, 12, 178, 111, 118, 14,
    ],
];

/// Lazy-initialized default active bits structure.
///
/// This provides a default `ActiveBits` instance representing an empty/null state.
/// The active bits structure is used to track which positions in the merkle tree
/// contain actual data versus empty/null entries.
///
/// # Usage
///
/// This is used when initializing new empty regions of the merkle tree,
/// providing a baseline "all inactive" state without runtime computation.
static NULL_ACTIVE_BITS: OnceBox<ActiveBits> = OnceBox::new();
fn get_active_bits() -> &'static ActiveBits {
    NULL_ACTIVE_BITS.get_or_init(|| Box::new(ActiveBits::default()))
}
/// Initializes the global null data structures used by twigs
///
/// Returns a tuple containing:
/// 1. A null Merkle tree for a twig
/// 2. A null twig instance
/// 3. An array of null node hashes for higher tree levels
fn get_init_data() -> (Box<TwigMT>, Box<Twig>, [Hash32; 64]) {
    // null left tree in twig:
    let null_mt_for_twig = create_null_mt_for_twig();
    // null right tree in twig:
    let null_twig = create_null_twig(null_mt_for_twig[1]);
    // null higher tree above twig:
    let null_node_in_higher_tree = create_null_node_in_higher_tree(&null_twig);

    (null_mt_for_twig, null_twig, null_node_in_higher_tree)
}

/// Creates a null Merkle tree for a twig
///
/// This tree represents an empty twig where all entries are null.
/// The tree is structured with:
/// - Levels 0: 2048 leaf nodes with null entry hashes
/// - Levels 1-11: Internal nodes computed from the null leaves
fn create_null_mt_for_twig() -> Box<TwigMT> {
    let mut bz = [0u8; ENTRY_BASE_LENGTH + 8];
    let null_hash = entry::null_entry(&mut bz[..]).hash();

    // Index of first node at each level:
    //   Level:   0     1     2     3     4     5     6     7     8     9    10    11
    //   Base:  2048  1024   512   256   128    64    32    16     8     4     2     1
    //   First: 2048  1024   512   256   128    64    32    16     8     4     2     1
    // Total levels: 12, number of nodes: 4096
    // For any level L, node at position j is stored at: base + j where base = 2048 >> L
    // Parent/child relationships:
    //   - Parent of node i is stored at: i/2
    //   - Children of node i are stored at: base + 2j, base + 2j + 1 (next level's base)
    let mut null_mt_for_twig = vec![*ZERO_HASH32; 4096].into_boxed_slice();
    for i in 2048..4096 {
        null_mt_for_twig[i] = null_hash;
    }
    sync_mtree(&mut null_mt_for_twig, 0, 2047);

    null_mt_for_twig
}

/// Creates a null twig instance with all fields initialized to represent empty state
///
/// # Arguments
/// * `null_mt_for_twig` - The root hash of a null Merkle tree to use as the left_root
fn create_null_twig(null_mt_for_twig: Hash32) -> Box<Twig> {
    let mut null_twig = Box::new(Twig::new());

    null_twig.sync_l1(0, get_active_bits());
    null_twig.sync_l1(1, get_active_bits());
    null_twig.sync_l1(2, get_active_bits());
    null_twig.sync_l1(3, get_active_bits());
    null_twig.sync_l2(0);
    null_twig.sync_l2(1);
    null_twig.sync_l3();

    null_twig.left_root = null_mt_for_twig;
    null_twig.sync_top();

    null_twig
}

/// Creates an array of null node hashes for levels above the twig
///
/// # Arguments
/// * `null_twig` - A null twig instance to use as the base for computing higher level hashes
///
/// # Returns
/// An array of 64 hashes representing null nodes at each level above the twig
fn create_null_node_in_higher_tree(null_twig: &Twig) -> [Hash32; 64] {
    let mut null_node_in_higher_tree = [*ZERO_HASH32; 64];

    null_node_in_higher_tree[FIRST_LEVEL_ABOVE_TWIG as usize] = merkle_node_hash(
        TWIG_ROOT_LEVEL as u8,
        &null_twig.twig_root,
        &null_twig.twig_root,
    );

    for i in (FIRST_LEVEL_ABOVE_TWIG + 1)..64 {
        let left = null_node_in_higher_tree[(i - 1) as usize];
        null_node_in_higher_tree[i as usize] = merkle_node_hash((i - 1) as u8, &left, &left);
    }

    null_node_in_higher_tree
}

#[cfg(test)]
mod tests {

    use crate::merkletree::{
        diffmap::DiffMap,
        null::{
            get_in_null_mt, get_init_data, get_null_mt_for_twig, NULL_MT_FOR_TWIG_DIFF,
            NULL_NODE_IN_HIGHER_TREE,
        },
        twig::get_null_twig_box,
    };

    #[test]
    fn test_null() {
        let (a, b, c) = get_init_data();
        let diff = DiffMap::generate_diff(&a);
        assert_eq!(diff, NULL_MT_FOR_TWIG_DIFF);

        for i in 0..4096 {
            assert_eq!(*get_null_mt_for_twig().get(i), a[i]);
            assert_eq!(*get_in_null_mt(i), a[i]);
        }

        assert!(a == get_null_mt_for_twig().to_vec());
        assert!(b == get_null_twig_box().clone());
        assert!(c == NULL_NODE_IN_HIGHER_TREE);
    }
}
