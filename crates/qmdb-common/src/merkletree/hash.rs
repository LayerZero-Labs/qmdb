use crate::utils::hasher::{blake3_keyed_hash_inplace, Hash32};

pub use crate::utils::hasher::AlignedHash32;

/// Compute merkle node hash in-place with AlignedHash32 parameters.
/// Parameters match blake3_keyed_hash_inplace for optimal performance.
#[inline(always)]
pub fn merkle_node_hash_inplace(
    output: &mut AlignedHash32,
    children_level: usize,
    left: &AlignedHash32,
    right: &AlignedHash32,
) {
    blake3_keyed_hash_inplace(output, children_level, left, right);
}

#[inline(always)]
pub fn merkle_node_hash<T: AsRef<[u8]>>(children_level: u8, left: T, right: T) -> Hash32 {
    let mut left_aligned = AlignedHash32::zeroed();
    let mut right_aligned = AlignedHash32::zeroed();
    left_aligned.as_bytes_mut().copy_from_slice(left.as_ref());
    right_aligned.as_bytes_mut().copy_from_slice(right.as_ref());

    let mut output = AlignedHash32::zeroed();
    merkle_node_hash_inplace(
        &mut output,
        children_level as usize,
        &left_aligned,
        &right_aligned,
    );
    *output.as_bytes()
}
