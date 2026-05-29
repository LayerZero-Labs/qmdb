//! Verification utilities for Merkle tree consistency checking.
//!
//! This module provides functions to verify the integrity and consistency of:
//! - Individual Merkle trees within twigs
//! - Active bits Merkle trees
//! - Upper tree node relationships
//! - Hash chain consistency throughout the tree
//!
//! The checks ensure that:
//! 1. Parent nodes correctly hash their children
//! 2. Active bits trees are properly constructed
//! 3. Twig roots match their components
//! 4. All tree invariants are maintained

use super::tree::{NodePos, Tree};
use qmdb_common::def::{
    FIRST_LEVEL_ABOVE_TWIG, MAX_UPPER_LEVEL, NODE_SHARD_COUNT, TWIG_SHARD_COUNT,
};
use qmdb_common::merkletree::hash::merkle_node_hash;
use qmdb_common::merkletree::{
    activebits::ActiveBits,
    null::NULL_TWIG,
    twig::{self, TwigMT},
};
use qmdb_common::utils::hasher::Hash32;
use std::collections::HashMap;

/// Verifies the consistency of a Merkle tree within a twig
///
/// This function checks that each parent node correctly hashes its children
/// throughout the tree, working from the leaves up to the root.
///
/// # Arguments
/// * `mt` - The Merkle tree array to verify
///
/// # Panics
/// Panics if any parent node's hash doesn't match its children
pub fn check_mt(mt: &TwigMT) {
    let mut level = 10;
    let mut stride = 1;
    let mut sum = [0; 32];

    while stride <= 1024 {
        for i in stride..2 * stride {
            sum = merkle_node_hash(level, &mt[2 * i], &mt[2 * i + 1]);
            if !mt[i].eq(&sum) {
                panic!("Mismatch {}-{} {} {}", level, i, 2 * i, 2 * i + 1);
            }
        }

        if stride == 1024 {
            break;
        }
        stride *= 2;
        level -= 1;
    }
}

/// Verifies that two byte slices contain the same hash value
///
/// # Arguments
/// * `tag` - A string identifier for the comparison (used in error messages)
/// * `a` - First hash value
/// * `b` - Second hash value
///
/// # Panics
/// Panics if the hash values don't match
pub fn hash_equal(tag: &str, a: &[u8], b: &[u8]) {
    if !a.eq(b) {
        println!("a: {a:?}, b: {b:?}");
        panic!("{}", tag.to_owned() + "Not Equal");
    }
}

/// Verifies the consistency of all upper tree nodes
///
/// This function checks that all nodes in the upper levels of the tree
/// (above twigs) have correct hash relationships with their children.
///
/// # Arguments
/// * `tree` - The tree to verify
///
/// # Panics
/// Panics if any hash relationships are invalid
pub fn check_upper_nodes(tree: &Tree) {
    for k in 0..MAX_UPPER_LEVEL {
        for s in 0..NODE_SHARD_COUNT {
            check_upper_nodes_internal(tree, &tree.upper_tree.nodes[k][s]);
        }
    }
}

/// Internal helper for checking upper tree node consistency
///
/// For each node in a shard, verifies that its hash matches its children's hashes.
/// Handles both twig-level nodes and higher level nodes appropriately.
///
/// # Arguments
/// * `tree` - The tree containing the nodes
/// * `nodes` - Map of node positions to their hash values
///
/// # Panics
/// Panics if any parent node's hash doesn't match its children
fn check_upper_nodes_internal(tree: &Tree, nodes: &HashMap<NodePos, [u8; 32]>) {
    for (pos, parent_hash) in nodes {
        let level = pos.level();
        let n = pos.nth();
        let mut left_child = [0; 32];
        let mut right_child = [0; 32];
        if level == FIRST_LEVEL_ABOVE_TWIG as u64 {
            let left_child_option = tree.upper_tree.get_twig_root(2 * n);
            match left_child_option {
                Some(v) => left_child.copy_from_slice(v),
                None => continue,
            }
            let right_child_option = tree.upper_tree.get_twig_root(2 * n + 1);
            match right_child_option {
                Some(v) => right_child.copy_from_slice(v),
                None => {
                    right_child.copy_from_slice(NULL_TWIG.twig_root.as_slice());
                }
            }
        } else {
            let left_child_option = tree.upper_tree.get_node(NodePos::pos(level - 1, 2 * n));
            match left_child_option {
                Some(v) => left_child.copy_from_slice(v),
                None => continue,
            }
            let right_child_option = tree.upper_tree.get_node(NodePos::pos(level - 1, 2 * n + 1));
            match right_child_option {
                Some(v) => right_child.copy_from_slice(v),
                None => continue,
            }
        }
        let h = merkle_node_hash(level as u8 - 1, &left_child, &right_child);
        if !h.eq(parent_hash) {
            panic!("Mismatch at {}-{} l:{} r:{}", level, n, 2 * n, 2 * n + 1);
        }
    }
}

/// Verifies the consistency of a twig's active bits Merkle tree
///
/// This function checks that:
/// 1. Level 1 nodes correctly hash their active bits
/// 2. Level 2 nodes correctly hash their level 1 children
/// 3. Level 3 node correctly hashes its level 2 children
/// 4. Twig root correctly hashes the left root and active bits root
///
/// # Arguments
/// * `twig` - The twig to verify
/// * `active_bits` - The active bits structure for this twig
///
/// # Panics
/// Panics if any hash relationships are invalid
pub fn check_twig(twig: &twig::Twig, active_bits: &ActiveBits) {
    hash_equal(
        "L1-0",
        &twig.active_bits_mtl1[0],
        hash1(8, active_bits.get_bits(0, 64)).as_slice(),
    );
    hash_equal(
        "L1-1",
        &twig.active_bits_mtl1[1],
        hash1(8, active_bits.get_bits(1, 64)).as_slice(),
    );
    hash_equal(
        "L1-2",
        &twig.active_bits_mtl1[2],
        hash1(8, active_bits.get_bits(2, 64)).as_slice(),
    );
    hash_equal(
        "L1-3",
        &twig.active_bits_mtl1[3],
        hash1(8, active_bits.get_bits(3, 64)).as_slice(),
    );
    hash_equal(
        "L2-0",
        &twig.active_bits_mtl2[0],
        merkle_node_hash(9, &twig.active_bits_mtl1[0], &twig.active_bits_mtl1[1]).as_slice(),
    );
    hash_equal(
        "L2-1",
        &twig.active_bits_mtl2[1],
        merkle_node_hash(9, &twig.active_bits_mtl1[2], &twig.active_bits_mtl1[3]).as_slice(),
    );
    hash_equal(
        "L3",
        &twig.active_bits_mtl3,
        merkle_node_hash(10, &twig.active_bits_mtl2[0], &twig.active_bits_mtl2[1]).as_slice(),
    );
    hash_equal(
        "Top",
        &twig.twig_root,
        merkle_node_hash(11, &twig.left_root, &twig.active_bits_mtl3).as_slice(),
    );
}

/// Verifies the consistency of all active twigs in the tree
///
/// This function iterates through all twig shards and checks each active
/// twig's hash relationships with its active bits.
///
/// # Arguments
/// * `tree` - The tree containing the twigs to verify
///
/// # Panics
/// Panics if any twig's hash relationships are invalid
pub fn check_all_twigs(tree: &Tree) {
    for s in 0..TWIG_SHARD_COUNT {
        for (twig_id, twig) in tree.upper_tree.active_twig_shards[s].iter() {
            let active_bits = &tree.active_bit_shards[s][twig_id];
            check_twig(twig, active_bits);
        }
    }
}

/// Performs a complete consistency check of the entire tree
///
/// This function verifies:
/// 1. All active twigs and their active bits trees
/// 2. All upper tree node relationships
/// 3. The Merkle tree for the youngest twig
///
/// # Arguments
/// * `tree` - The tree to verify
///
/// # Panics
/// Panics if any inconsistencies are found
pub fn check_hash_consistency(tree: &Tree) {
    check_all_twigs(tree);
    check_upper_nodes(tree);
    check_mt(&tree.mtree_for_youngest_twig);
}

fn hash1(level: u8, a: &[u8]) -> Hash32 {
    merkle_node_hash(level, &a[0..32], &a[32..64])
}
