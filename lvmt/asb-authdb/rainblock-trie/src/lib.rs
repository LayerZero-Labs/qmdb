mod child_ref;
mod nibble;
mod rain_mpt;
#[cfg(test)]
mod tests;
mod trie_node;
mod trie_node_ext;

#[cfg(not(feature = "thread-safe"))]
mod thread_non_safe;
#[cfg(not(feature = "thread-safe"))]
pub use thread_non_safe::{Node, NodePtr, NodePtrWeak};
#[cfg(not(feature = "thread-safe"))]
unsafe impl<const N: usize> Send for MerklePatriciaTree<N> {} // As internal Rc does not expose to outsize, it is safe to declare it as Send.
#[cfg(feature = "thread-safe")]
mod thread_safe;
#[cfg(feature = "thread-safe")]
pub use thread_safe::{Node, NodePtr, NodePtrWeak};

pub use rain_mpt::MerklePatriciaTree;

fn common_prefix_iter<'a, T: Eq>(a: &'a [T], b: &'a [T]) -> impl Iterator<Item = &'a T> {
    a.iter()
        .zip(b.iter())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.clone())
}

fn add_prefix<T: Copy>(base: &mut Vec<T>, prefix: &[T]) {
    *base = [prefix, &base[..]].concat()
}

#[cfg(feature = "light-hash")]
pub use blake2_hasher::Blake2bHasher as RlpHasher;
#[cfg(not(feature = "light-hash"))]
pub use keccak_hasher::KeccakHasher as RlpHasher;
