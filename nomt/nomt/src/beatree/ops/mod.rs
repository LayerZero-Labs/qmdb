//! BTree Operations.

use anyhow::Result;
use bitvec::prelude::*;

use std::{cmp::Ordering, sync::Arc};

use super::{
    allocator::{PageNumber, StoreReader},
    branch::{node::get_key, BranchNode},
    index::Index,
    leaf::node::LeafNode,
    leaf_cache::LeafCache,
    Key,
};

pub(crate) mod bit_ops;
pub mod overflow;
mod reconstruction;
mod update;

pub use reconstruction::reconstruct;
pub use update::update;

/// Do a partial lookup of the key in the beatree.
///
/// This determines the leaf store page number which might store the associated value, or `None`
/// if the value is definitely non-existent.
pub fn partial_lookup(key: Key, bbn_index: &Index) -> Option<PageNumber> {
    let branch = match bbn_index.lookup(key) {
        None => return None,
        Some((_, branch)) => branch,
    };

    search_branch(&branch, key.clone()).map(|(_, leaf_pn)| leaf_pn)
}

/// Find the associated value associated with the key in the given leaf node, if any.
///
/// If the value is an overflow, this function will load it with blocking I/O.
pub fn finish_lookup_blocking(
    key: Key,
    leaf: &LeafNode,
    leaf_store: &StoreReader,
) -> Option<Vec<u8>> {
    leaf.get(&key).map(|(v, is_overflow)| {
        if is_overflow {
            overflow::read_blocking(v, leaf_store)
        } else {
            v.to_vec()
        }
    })
}

/// Find the associated value associated with the key in the given leaf node, if any.
///
/// If the value is an overflow, this function will create an asynchronous reader.
pub fn finish_lookup_async(
    key: Key,
    leaf: &LeafNode,
    leaf_store: &StoreReader,
) -> Result<Option<Vec<u8>>, overflow::AsyncReader> {
    leaf.get(&key)
        .map(|(v, is_overflow)| {
            if is_overflow {
                Err(overflow::AsyncReader::new(v, leaf_store.clone()))
            } else {
                Ok(v.to_vec())
            }
        })
        .transpose()
}

/// Lookup a key in the btree using blocking I/O.
pub fn lookup_blocking(
    key: Key,
    bbn_index: &Index,
    leaf_cache: &LeafCache,
    leaf_store: &StoreReader,
) -> Result<Option<Vec<u8>>> {
    let leaf_pn = match partial_lookup(key, bbn_index) {
        None => return Ok(None),
        Some(pn) => pn,
    };

    let leaf = match leaf_cache.get(leaf_pn) {
        Some(leaf) => leaf,
        None => {
            let leaf = Arc::new(LeafNode {
                inner: leaf_store.query(leaf_pn),
            });
            leaf_cache.insert(leaf_pn, leaf.clone());
            leaf
        }
    };

    Ok(finish_lookup_blocking(key, &leaf, leaf_store))
}

/// Binary search a branch node for the child node containing the key. This returns the last child
/// node pointer whose separator is less than or equal to the given key.
pub fn search_branch(branch: &BranchNode, key: Key) -> Option<(usize, PageNumber)> {
    let (found, pos) = find_key_pos(branch, &key, None);

    if found {
        return Some((pos, branch.node_pointer(pos).into()));
    } else if pos == 0 {
        return None;
    } else {
        // first key greater than the one we are looking for has been returned,
        // thus the correct child is the previous one
        return Some((pos - 1, branch.node_pointer(pos - 1).into()));
    }
}

// Binary search for a key within a branch node.
// Accept a field to override the starting point of the binary search.
// It returns true and the index of the specified key,
// or false and the index containing the first key greater than the specified one.
pub fn find_key_pos(branch: &BranchNode, key: &Key, low: Option<usize>) -> (bool, usize) {
    let prefix = branch.prefix();
    let n = branch.n() as usize;
    let prefix_compressed = branch.prefix_compressed() as usize;

    match key.view_bits::<Msb0>()[..prefix.len()].cmp(prefix) {
        Ordering::Less => return (false, 0),
        Ordering::Greater if n == prefix_compressed => return (false, n),
        Ordering::Equal | Ordering::Greater => {}
    }

    let mut low = low.unwrap_or(0);
    let mut high = branch.n() as usize;

    while low < high {
        let mid = low + (high - low) / 2;

        match key.cmp(&get_key(branch, mid)) {
            Ordering::Equal => {
                return (true, mid);
            }
            Ordering::Less => high = mid,
            Ordering::Greater => low = mid + 1,
        }
    }

    (false, high)
}

#[cfg(feature = "benchmarks")]
pub mod benches {
    use crate::{
        beatree::{
            benches::get_keys,
            branch::{node::BranchNodeBuilder, BranchNode},
            ops::bit_ops::separator_len,
            Key,
        },
        io::{PagePool, PAGE_SIZE},
    };
    use criterion::{BenchmarkId, Criterion};
    use rand::Rng;

    pub fn search_branch_benchmark(c: &mut Criterion) {
        let mut group = c.benchmark_group("search_branch");
        let mut rand = rand::thread_rng();
        let page_pool = PagePool::new();

        for prefix_len_bytes in [1, 4, 8, 12, 16] {
            // fill the branch node with as many separators as possible
            //
            // body_size = (prefix_len_bits + (separator_len_bits * n) + 7)/8 + 4 * n
            // n = (8 * body_size - prefix_len_bits) / (separator_len_bits + 8*4)
            let body_size_target = PAGE_SIZE - 8;
            let prefix_len_bits = prefix_len_bytes * 8;
            let separator_len_bits = (32 - prefix_len_bytes) * 8;
            let n = (8 * body_size_target - prefix_len_bits) / (separator_len_bits + 8 * 4);

            let mut separators: Vec<(usize, Key)> = get_keys(prefix_len_bytes, n)
                .into_iter()
                .map(|s| (separator_len(&s), s))
                .collect();
            separators.sort_by(|a, b| a.1.cmp(&b.1));

            let branch_node = BranchNode::new_in(&page_pool);
            let mut branch_node_builder =
                BranchNodeBuilder::new(branch_node, n, prefix_len_bits, 256);

            for (index, (separator_len, separator)) in separators.iter().enumerate() {
                branch_node_builder.push(separator.clone(), *separator_len, index as u32);
            }

            let branch = branch_node_builder.finish();

            group.bench_function(
                BenchmarkId::new("prefix_len_bytes", prefix_len_bytes),
                |b| {
                    b.iter_batched(
                        || {
                            let index = rand.gen_range(0..separators.len());
                            separators[index].1.clone()
                        },
                        |separator| super::search_branch(&branch, separator),
                        criterion::BatchSize::SmallInput,
                    )
                },
            );
        }

        group.finish();
    }
}
