#![cfg(feature = "benchmarks")]

use crate::beatree::{
    branch::node::benches::*, leaf::node::benches::*, ops::benches::*, ops::bit_ops::benches::*,
    Key,
};
use rand::RngCore;

pub fn beatree_benchmark(c: &mut criterion::Criterion) {
    separate_benchmark(c);
    separator_len_benchmark(c);
    prefix_len_benchmark(c);
    search_branch_benchmark(c);
    leaf_search_benchmark(c);
    reconstruct_key_benchmark(c);
    branch_builder_benchmark(c);
    leaf_builder_benchmark(c);
}

// returns two keys a and b where b > a and b shares the first n bits with a
pub fn get_key_pair(shared_bytes: usize) -> (Key, Key) {
    let mut rand = rand::thread_rng();
    let mut a = [0; 32];
    rand.fill_bytes(&mut a[0..shared_bytes]);

    // b > a
    let mut b = a.clone();
    b[shared_bytes] = 1;

    (a, b)
}

// Get a vector containing `n` random keys that share the first `shared_bytes`
pub fn get_keys(shared_bytes: usize, n: usize) -> Vec<Key> {
    let mut rand = rand::thread_rng();
    let mut prefix = [0; 32];
    rand.fill_bytes(&mut prefix[0..shared_bytes]);

    let mut keys = vec![];
    for _ in 0..n {
        let mut key = prefix.clone();
        rand.fill_bytes(&mut key[shared_bytes..]);
        keys.push(key);
    }

    keys
}
