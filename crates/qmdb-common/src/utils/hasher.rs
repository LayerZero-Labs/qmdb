//! Hashing utilities for the QMDB database.
//!
//! This module provides SHA-256 based hashing functions for:
//! - Single values
//! - Values with level information
//! - Pairs of values (for Merkle tree nodes)
//! - In-place node hashing

/// Type alias for a 32-byte hash value.
/// Used throughout the database for cryptographic hashes.
pub type Hash32 = [u8; 32];

#[repr(align(8))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AlignedHash32(pub Hash32);

impl AlignedHash32 {
    #[inline(always)]
    pub const fn new(bytes: Hash32) -> Self {
        Self(bytes)
    }
    #[inline(always)]
    pub const fn zeroed() -> Self {
        Self([0u8; 32])
    }
    #[inline(always)]
    pub const fn as_ref(&self) -> &Hash32 {
        &self.0
    }
    #[inline(always)]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    #[inline(always)]
    pub fn as_bytes_mut(&mut self) -> &mut [u8; 32] {
        &mut self.0
    }
    #[allow(dead_code)]
    pub const fn value(&self) -> Hash32 {
        self.0
    }
}

/// Zero hash stored with 8-byte alignment for fast u64 copies.
/// Use `*ZERO_HASH32` or `ZERO_HASH32.clone()` when a value is needed.
static ZERO_HASH32_STORAGE: AlignedHash32 = AlignedHash32([0u8; 32]);

/// A reference to a zero hash with guaranteed 8-byte alignment.
pub const ZERO_HASH32: &Hash32 = &ZERO_HASH32_STORAGE.0;

/// Blake3 hash with result written to output buffer.
#[inline(always)]
pub fn blake3_hash_inplace(output: &mut Hash32, input: &[u8]) {
    let hash = blake3::hash(input);
    *output = *hash.as_bytes();
}

/// Keyed blake3 hash with result written to output buffer.
///
/// # Arguments
/// * `output` - 32-byte buffer to write the hash result
/// * `key` - u8 key value (will be XORed with first byte of blake3 IV)
/// * `left` - Left 32-byte input
/// * `right` - Right 32-byte input
#[inline(always)]
pub fn blake3_keyed_hash_inplace(
    output: &mut AlignedHash32,
    key: usize,
    left: &AlignedHash32,
    right: &AlignedHash32,
) {
    let mut iv = BLAKE3_IVU8;
    // XOR key into first byte of IV (only low 8 bits matter for Blake3)
    iv[0] ^= key as u8;

    let mut input = [0u8; 64];
    input[..32].copy_from_slice(&left.0);
    input[32..].copy_from_slice(&right.0);

    let hash = blake3::keyed_hash(&iv, &input);
    output.0.copy_from_slice(hash.as_bytes());
}

/// Blake2b hash with result written to output buffer.
#[inline(always)]
pub fn blake2b_hash_inplace(output: &mut Hash32, input: &[u8]) {
    use blake2::Digest;
    let r = blake2::Blake2b512::digest(input);
    output.copy_from_slice(&r[..32]);
}

/// Blake2b hash of multiple slices with result written to output buffer.
#[inline(always)]
pub fn blake2b_hash_slices_inplace(output: &mut Hash32, inputs: &[&[u8]]) {
    use blake2::Digest;
    let mut hasher = blake2::Blake2b512::new();
    for input in inputs {
        hasher.update(input);
    }
    let r = hasher.finalize();
    output.copy_from_slice(&r[..32]);
}

// Copied IV from blake3 and converted from [u32; 8] to [u8; 32]
static BLAKE3_IVU8: [u8; 32] = [
    0x67, 0xE6, 0x09, 0x6A, // 0x6A09E667
    0x85, 0xAE, 0x67, 0xBB, // 0xBB67AE85
    0x72, 0xF3, 0x6E, 0x3C, // 0x3C6EF372
    0x3A, 0xF5, 0x4F, 0xA5, // 0xA54FF53A
    0x7F, 0x52, 0x0E, 0x51, // 0x510E527F
    0x8C, 0x68, 0x05, 0x9B, // 0x9B05688C
    0xAB, 0xD9, 0x83, 0x1F, // 0x1F83D9AB
    0x19, 0xCD, 0xE0, 0x5B, // 0x5BE0CD19
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ivu8() {
        const IV: [u32; 8] = [
            0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB,
            0x5BE0CD19,
        ];
        for i in 0..8 {
            assert_eq!(BLAKE3_IVU8[i * 4..(i + 1) * 4], IV[i].to_le_bytes());
        }
    }
}
