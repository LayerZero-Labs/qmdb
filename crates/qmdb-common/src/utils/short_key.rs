//! Short key extraction functions for hash-based data structures.
//!
//! These functions extract a `u64` "short key" from various key types,
//! used for efficient hash table indexing in RomHashMap and SimpleMap.
//!
//! # Usage Summary
//!
//! | HashMap              | Map Type    | Key Type        | Key Size | Hasher               |
//! |----------------------|-------------|-----------------|----------|----------------------|
//! | `AddrToHashMap`      | RomHashMap  | address         | 20 bytes | `addr_short_key`     |
//! | `SlotToHashMap`      | RomHashMap  | slot key        | 52 bytes | `slot_short_key`     |
//! | `RomEntryMap`        | RomHashMap  | sn (u64)        | 8 bytes  | `sn_short_key`       |
//! | `RomKhToSnMap`       | RomHashMap  | keccak hash     | 32 bytes | `hash32_short_key`   |
//! | `CodeMap`            | SimpleMap   | Hash32          | 32 bytes | `Hash32Hasher`       |
//! | `activebits_map`     | SimpleMap   | level8nth       | 8 bytes  | `Level8NthHasher`    |
//! | `ChunkVerifier`      | SimpleMap   | jumptable_ptr   | usize    | `PtrHasher`          |
//! | `created_code_map`   | SimpleMap   | Address         | 20 bytes | `AddressHasher`      |
//! | `orig_acc_map`       | SimpleMap   | Address         | 20 bytes | `AddressHasher`      |
//! | `orig_slot_dust_map` | SimpleMap   | SlotKey         | 52 bytes | `SlotKeyHasher`      |
//!
//! All byte-slice functions use `read_unaligned`, safe for any alignment.

use core::marker::PhantomData;

// ============================================================================
// ShortKeyHasher trait for zero-cost SimpleMap hashing
// ============================================================================

/// Trait for computing short keys from key types.
/// Used by SimpleMap for zero-cost hash computation (no function pointer overhead).
pub trait ShortKeyHasher<K> {
    fn hash(key: &K) -> u64;
}

/// Hasher for level8nth u64 keys.
/// Compresses sparse values (8-15, 24-31, ...) into contiguous indices.
pub struct Level8NthHasher;

impl ShortKeyHasher<u64> for Level8NthHasher {
    #[inline(always)]
    fn hash(key: &u64) -> u64 {
        (key >> 4) << 3 | (key & 7)
    }
}

/// Hasher for 32-byte hash keys (keccak, etc).
/// Uses bytes 0..8 XOR 24..32. No wyhash64 needed (keccak is well-distributed).
pub struct Hash32Hasher<T>(PhantomData<T>);

impl<T: AsRef<[u8]>> ShortKeyHasher<T> for Hash32Hasher<T> {
    #[inline(always)]
    fn hash(key: &T) -> u64 {
        hash32_short_key(key.as_ref())
    }
}

/// Hasher for 20-byte address keys.
/// Uses all 20 bytes: bytes 0..8 XOR 8..16 XOR (12..20 shifted) + wyhash64.
pub struct AddressHasher<T>(PhantomData<T>);

impl<T: AsRef<[u8]>> ShortKeyHasher<T> for AddressHasher<T> {
    #[inline(always)]
    fn hash(key: &T) -> u64 {
        addr_short_key(key.as_ref())
    }
}

/// Hasher for 52-byte slot keys.
/// Uses all 52 bytes via 7 overlapping u64 reads + wyhash64.
pub struct SlotKeyHasher<T>(PhantomData<T>);

impl<T: AsRef<[u8]>> ShortKeyHasher<T> for SlotKeyHasher<T> {
    #[inline(always)]
    fn hash(key: &T) -> u64 {
        slot_short_key(key.as_ref())
    }
}

/// Hasher for pointer-as-usize keys.
/// Uses wyhash64 to avoid hash collision from predictable allocation patterns.
pub struct PtrHasher;

impl ShortKeyHasher<usize> for PtrHasher {
    #[inline(always)]
    fn hash(key: &usize) -> u64 {
        wyhash64(*key as u64)
    }
}

/// Identity hasher for testing - returns the key as-is.
pub struct IdentityHasher;

impl ShortKeyHasher<u64> for IdentityHasher {
    #[inline(always)]
    fn hash(key: &u64) -> u64 {
        *key
    }
}

// ============================================================================
// wyhash64
// ============================================================================

/// Fast hash mixing using wyhash64 algorithm.
#[inline(always)]
pub fn wyhash64(mut x: u64) -> u64 {
    x ^= x >> 32;
    x = x.wrapping_mul(0xd6e8feb86659fd93);
    x ^= x >> 32;
    x = x.wrapping_mul(0xd6e8feb86659fd93);
    x
}

// ============================================================================
// Byte-slice short_key_fn (ShortKeyFn = fn(&[u8]) -> u64)
// ============================================================================

/// Extracts short key from 20-byte address keys.
/// Uses all 20 bytes: bytes 0..8 XOR 8..16 XOR (12..20 shifted) + wyhash64.
#[inline(always)]
pub fn addr_short_key(key: &[u8]) -> u64 {
    debug_assert!(key.len() == 20);
    let ptr = key.as_ptr() as *const u64;
    // bytes 0..8
    let a = unsafe { ptr.read_unaligned() };
    // bytes 8..16
    let b = unsafe { ptr.add(1).read_unaligned() };
    // bytes 12..20 (overlaps with b to cover last 4 bytes)
    let c = unsafe { (key.as_ptr().add(12) as *const u64).read_unaligned() };
    wyhash64(a ^ b ^ c)
}

/// Extracts short key from 52-byte slot keys.
/// Layout: [address 20B][storage_index 32B]
/// Uses all 52 bytes via 7 overlapping u64 reads + wyhash64.
#[inline(always)]
pub fn slot_short_key(key: &[u8]) -> u64 {
    debug_assert!(key.len() == 52);
    let ptr = key.as_ptr() as *const u64;
    // bytes 0..8
    let a = unsafe { ptr.read_unaligned() };
    // bytes 8..16
    let b = unsafe { ptr.add(1).read_unaligned() };
    // bytes 16..24
    let c = unsafe { ptr.add(2).read_unaligned() };
    // bytes 24..32
    let d = unsafe { ptr.add(3).read_unaligned() };
    // bytes 32..40
    let e = unsafe { ptr.add(4).read_unaligned() };
    // bytes 40..48
    let f = unsafe { ptr.add(5).read_unaligned() };
    // bytes 44..52 (overlaps with f to cover last 4 bytes)
    let g = unsafe { (key.as_ptr().add(44) as *const u64).read_unaligned() };
    wyhash64(a ^ b ^ c ^ d ^ e ^ f ^ g)
}

/// Extracts short key from 8-byte sn keys.
#[inline(always)]
pub fn sn_short_key(key: &[u8]) -> u64 {
    debug_assert!(key.len() == 8);
    unsafe { (key.as_ptr() as *const u64).read_unaligned() }
}

/// Extracts short key from 32-byte keccak hash keys.
/// Uses bytes 0..8 XOR 24..32. No wyhash64 needed (keccak is well-distributed).
#[inline(always)]
pub fn hash32_short_key(key: &[u8]) -> u64 {
    debug_assert!(key.len() == 32);
    let ptr = key.as_ptr() as *const u64;
    let head = unsafe { ptr.read_unaligned() };
    let tail = unsafe { ptr.add(3).read_unaligned() };
    head ^ tail
}

// ============================================================================
// Typed key short_key_fn
// ============================================================================

/// Extracts short key from pointer-as-usize keys.
/// Uses wyhash64 to avoid hash collision from predictable allocation patterns.
#[inline(always)]
pub fn ptr_short_key(key: &usize) -> u64 {
    wyhash64(*key as u64)
}

/// Extracts short key from level8nth keys.
/// Compresses sparse values (8-15, 24-31, ...) into contiguous indices.
#[inline(always)]
pub fn level8nth_short_key(key: &u64) -> u64 {
    (key >> 4) << 3 | (key & 7)
}
