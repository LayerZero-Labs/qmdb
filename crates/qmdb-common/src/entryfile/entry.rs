//! Entry file format and manipulation utilities.
//!
//! This module provides data structures and functions for handling entries in the QMDB database.
//! Entries are the fundamental unit of data storage, containing key-value pairs along with
//! versioning and serialization metadata.
//!
//! # Entry Format
//!
//! The serialized entry format consists of:
//! - 1 byte: Key length
//! - 3 bytes: Value length
//! - 1 byte: Deactivated serial number list length
//! - Variable: Key data
//! - Variable: Value data
//! - 32 bytes: Next key hash
//! - 8 bytes: Version (signed 64-bit)
//! - 8 bytes: Serial number (unsigned 64-bit)
//! - Variable: Deactivated serial number list (8 bytes per entry)
//! - Padding: Zero bytes to align to 8-byte boundary
//! - Optional: AES-GCM tag (if `tee_cipher` feature is enabled)
//!
//! # Special Entry Types
//!
//! - **Sentry Entry**: Used to limit range queries in indexing. Has empty value and special key format, as well as a special hash.
//! - **Null Entry**: Represents a empty entry with `NULL_ENTRY_VERSION`.
use crate::def::{
    ENTRY_BASE_LENGTH, ENTRY_FIXED_LENGTH, NULL_ENTRY_VERSION, SHARD_COUNT, TAG_SIZE,
};
use crate::utils::byte0_to_shard_id;
use crate::utils::byteorder::{BigEndian, LittleEndian};
use crate::utils::hasher::{blake2b_hash_inplace, blake3_hash_inplace, Hash32};
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::ptr;
use once_cell::race::OnceBox;

/// Index position of the deactivated serial number list length in the entry header.
const DSN_IDX: usize = 4;

/// An entry in the database containing key-value data and metadata.
///
/// This structure represents a single entry with its key, value, and associated metadata
/// including version information, serial number, and hash of the next key for ordering.
#[derive(Debug)]
pub struct Entry<'a> {
    /// The key bytes for this entry.
    pub key: &'a [u8],
    /// The value bytes for this entry.
    pub value: &'a [u8],
    /// Hash of the next key in the ordering sequence.
    pub next_key_hash: &'a [u8],
    /// Version number of this entry. `NULL_ENTRY_VERSION` indicates a deleted entry.
    pub version: i64,
    /// Unique serial number for this entry.
    pub serial_number: u64,
}

/// A serialized entry represented as bytes.
///
/// This structure wraps the byte representation of an entry and provides methods
/// to extract individual fields without deserializing the entire entry.
#[derive(PartialEq)]
pub struct EntryBz<'a> {
    /// The raw bytes of the serialized entry.
    pub bz: &'a [u8],
}

impl core::fmt::Debug for EntryBz<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "key: {}, value: {}, next_key_hash: {}, version: {}, serial_number: {}",
            hex::encode(self.key()),
            hex::encode(self.value()),
            hex::encode(self.next_key_hash()),
            self.version(),
            self.serial_number()
        )
    }
}

impl<'a> Entry<'a> {
    /// Creates an `Entry` from a serialized `EntryBz`.
    ///
    /// # Arguments
    ///
    /// * `e` - The serialized entry to deserialize.
    ///
    /// # Returns
    ///
    /// An `Entry` struct with references to the data in the serialized entry.
    pub fn from_bz(e: &'a EntryBz) -> Entry<'a> {
        Self {
            key: e.key(),
            value: e.value(),
            next_key_hash: e.next_key_hash(),
            version: e.version(),
            serial_number: e.serial_number(),
        }
    }

    /// Calculates the total serialized length of this entry.
    ///
    /// # Arguments
    ///
    /// * `deactived_sn_count` - Number of deactivated serial numbers to include.
    ///
    /// # Returns
    ///
    /// The total byte length required for serialization, including padding.
    pub fn get_serialized_len(&self, deactived_sn_count: usize) -> usize {
        let length = ENTRY_BASE_LENGTH + self.key.len() + self.value.len();
        length.div_ceil(8) * 8 + deactived_sn_count * 8
    }

    /// Serializes this entry into the provided buffer.
    ///
    /// # Arguments
    ///
    /// * `b` - The buffer to write the serialized entry to.
    /// * `deactived_sn_list` - List of deactivated serial numbers to include.
    ///
    /// # Returns
    ///
    /// The number of bytes written to the buffer.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is too small or if `next_key_hash` is not exactly 32 bytes.
    pub fn dump(&self, b: &mut [u8], deactived_sn_list: &[u64]) -> usize {
        if b.len() < self.get_serialized_len(deactived_sn_list.len()) {
            panic!("Not enough space for dumping");
        }

        if self.version != NULL_ENTRY_VERSION {
            assert!(!self.key.is_empty());
        }

        assert!(self.key.len() <= u8::MAX as usize);
        // self.value.len() <= u24::MAX
        assert_eq!(self.value.len() & 0xFF00_0000, 0);
        let first32 = (self.value.len() << 8 | self.key.len()) as u32;

        assert!(deactived_sn_list.len() <= u8::MAX as usize);
        b[DSN_IDX] = deactived_sn_list.len() as u8;

        LittleEndian::write_u32(&mut b[..DSN_IDX], first32);
        let mut i = 5;
        unsafe {
            let dst = b.as_mut_ptr();
            ptr::copy_nonoverlapping(self.key.as_ptr(), dst.add(i), self.key.len());
            i += self.key.len();
            ptr::copy_nonoverlapping(self.value.as_ptr(), dst.add(i), self.value.len());
            i += self.value.len();
        }

        if self.next_key_hash.len() != 32 {
            panic!("NextKeyHash is not 32-byte");
        }
        unsafe {
            ptr::copy_nonoverlapping(self.next_key_hash.as_ptr(), b.as_mut_ptr().add(i), 32);
        }
        i += 32;
        LittleEndian::write_i64(&mut b[i..i + 8], self.version);
        i += 8;
        LittleEndian::write_u64(&mut b[i..i + 8], self.serial_number);
        i += 8;

        for &sn in deactived_sn_list {
            LittleEndian::write_u64(&mut b[i..i + 8], sn);
            i += 8;
        }

        let pad = (8 - (i & 7)) & 7;
        if pad != 0 {
            unsafe { ptr::write_bytes(b.as_mut_ptr().add(i), 0, pad) };
            i += pad;
        }
        if cfg!(feature = "tee_cipher") {
            i += TAG_SIZE;
        }
        i
    }

    /// Compare this entry with target bytes directly without serializing to intermediate buffer.
    ///
    /// Returns `true` when this entry's serialized form exactly matches `target`.
    #[inline(always)]
    pub fn dump_compare(&self, deactived_sn_list: &[u64], target: &[u8]) -> bool {
        let total_len = self.get_serialized_len(deactived_sn_list.len());
        if target.len() != total_len {
            return false;
        }

        // Compare header: first32 (key_len + value_len) and dsn_count
        let first32 = (self.value.len() << 8 | self.key.len()) as u32;
        if LittleEndian::read_u32(&target[..DSN_IDX]) != first32 {
            return false;
        }
        if target[DSN_IDX] != deactived_sn_list.len() as u8 {
            return false;
        }

        let mut i = 5;

        // Compare key
        if self.key != &target[i..i + self.key.len()] {
            return false;
        }
        i += self.key.len();

        // Compare value
        if self.value != &target[i..i + self.value.len()] {
            return false;
        }
        i += self.value.len();

        // Compare next_key_hash (32 bytes)
        if self.next_key_hash != &target[i..i + 32] {
            return false;
        }
        i += 32;

        // Compare version
        if LittleEndian::read_i64(&target[i..i + 8]) != self.version {
            return false;
        }
        i += 8;

        // Compare serial_number
        if LittleEndian::read_u64(&target[i..i + 8]) != self.serial_number {
            return false;
        }
        i += 8;

        // Compare deactivated serial numbers
        for &sn in deactived_sn_list {
            if LittleEndian::read_u64(&target[i..i + 8]) != sn {
                return false;
            }
            i += 8;
        }

        // Compare padding (should be zeros)
        let pad = (8 - (i & 7)) & 7;
        if pad != 0 {
            for j in 0..pad {
                if target[i + j] != 0 {
                    return false;
                }
            }
        }

        true
    }

    /// Computes the hash of this entry's key.
    ///
    /// For sentry entries (entries with empty values), returns a special hash
    /// using the first two bytes of the key. Otherwise, returns the full hash of the key.
    ///
    /// # Returns
    ///
    /// A 32-byte hash of the key.
    pub fn key_hash(key: &[u8], is_sentry_entry: bool) -> Hash32 {
        fn sentry_entry_key_hash(key: &[u8]) -> Hash32 {
            let mut res: Hash32 = [0; 32];
            res[0] = key[0]; // first byte of key
            res[1] = key[1]; // second byte of key
            res
        }

        // nullentry: key is empty
        if key.is_empty() {
            return [0; 32];
        }

        if is_sentry_entry {
            return sentry_entry_key_hash(key);
        }

        entry_key_hash(key)
    }
}

/// Creates a sentry entry for the given shard and serial number.
///
/// # Arguments
///
/// * `shard_id` - The shard ID (must be < `SHARD_COUNT`).
/// * `sn` - The serial number (must fit in 16 bits after shard encoding).
/// * `bz` - Buffer to write the serialized sentry entry.
///
/// # Returns
///
/// An `EntryBz` containing the serialized sentry entry.
///
/// # Panics
///
/// Panics if `shard_id` is >= `SHARD_COUNT` or if `sn` is too large.
pub fn sentry_entry(shard_id: usize, sn: u64, bz: &mut [u8]) -> EntryBz {
    if shard_id >= SHARD_COUNT || (sn as usize) >= (1 << 16) / SHARD_COUNT {
        panic!("SentryEntry Overflow");
    }
    let first16 = ((shard_id * (1 << 16) / SHARD_COUNT) | (sn as usize)) as u16;
    let mut key: [u8; 32] = [0; 32];
    BigEndian::write_u16(&mut key[0..2], first16);
    let mut next_key_hash: [u8; 32];
    if first16 == 0xFFFF {
        next_key_hash = [0xFF; 32];
    } else {
        next_key_hash = [0; 32];
        BigEndian::write_u16(&mut next_key_hash[0..2], first16 + 1);
    }
    let e = Entry {
        key: &key[..],
        value: &[] as &[u8],
        next_key_hash: &next_key_hash[..],
        version: 0,
        serial_number: sn,
    };
    let i = e.dump(bz, &[] as &[u64]);
    EntryBz { bz: &bz[..i] }
}

/// Computes the hash for a key, excluding null and sentry entries.
pub fn entry_key_hash<T: AsRef<[u8]>>(key: T) -> Hash32 {
    let mut hash = [0u8; 32];
    entry_key_hash_inplace(&mut hash, key.as_ref());
    hash
}

/// Computes the hash for a key, excluding null and sentry entries.
pub fn entry_key_hash_inplace(output: &mut Hash32, key: &[u8]) {
    blake3_hash_inplace(output, key);
}

/// Creates a null entry representing an empty entry.
///
/// Null entries have empty key and value, `NULL_ENTRY_VERSION` as version,
/// and `u64::MAX` as serial number.
///
/// # Arguments
///
/// * `bz` - Buffer to write the serialized null entry.
///
/// # Returns
///
/// An `EntryBz` containing the serialized null entry.
pub fn null_entry(bz: &mut [u8]) -> EntryBz {
    let next_key_hash: [u8; 32] = [0; 32];
    let e = Entry {
        key: &[] as &[u8],
        value: &[] as &[u8],
        next_key_hash: &next_key_hash[..],
        version: NULL_ENTRY_VERSION,
        serial_number: u64::MAX,
    };
    let i = e.dump(bz, &[] as &[u64]);
    EntryBz { bz: &bz[..i] }
}

/// Serializes an entry to bytes for testing purposes.
///
/// This is a convenience function that combines entry serialization with
/// buffer management for testing scenarios.
///
/// # Arguments
///
/// * `e` - The entry to serialize.
/// * `deactived_sn_list` - List of deactivated serial numbers.
/// * `bz` - Buffer to write the serialized entry.
///
/// # Returns
///
/// An `EntryBz` containing the serialized entry.
pub fn entry_to_bytes<'a>(
    e: &'a Entry<'a>,
    deactived_sn_list: &'a [u64],
    bz: &'a mut [u8],
) -> EntryBz<'a> {
    let total_len = e.get_serialized_len(deactived_sn_list.len());
    e.dump(&mut bz[..], deactived_sn_list);
    EntryBz {
        bz: &bz[..total_len],
    }
}

impl<'a> EntryBz<'a> {
    /// Extracts the key length from the entry header bytes.
    ///
    /// # Arguments
    ///
    /// * `len_bytes` - The first few bytes of the entry header.
    ///
    /// # Returns
    ///
    /// The length of the key in bytes.
    pub fn get_key_len(len_bytes: &[u8]) -> usize {
        len_bytes[0] as usize
    }

    /// Extracts both key and value lengths from the entry header bytes.
    ///
    /// # Arguments
    ///
    /// * `len_bytes` - The first few bytes of the entry header.
    ///
    /// # Returns
    ///
    /// A tuple of (key_length, value_length) in bytes.
    pub fn get_kv_len(len_bytes: &[u8]) -> (usize, usize) {
        let first32 = LittleEndian::read_u32(&len_bytes[..DSN_IDX]);
        let key_len = EntryBz::get_key_len(len_bytes);
        let value_len = (first32 >> 8) as usize;
        (key_len, value_len)
    }

    /// Calculates the total entry length from the header bytes.
    ///
    /// # Arguments
    ///
    /// * `len_bytes` - The first few bytes of the entry header.
    ///
    /// # Returns
    ///
    /// The total serialized length of the entry including padding.
    pub fn get_entry_len(len_bytes: &[u8]) -> usize {
        let (key_len, value_len) = EntryBz::get_kv_len(len_bytes);
        let dsn_count = len_bytes[DSN_IDX] as usize;
        (ENTRY_BASE_LENGTH + key_len + value_len).div_ceil(8) * 8 + dsn_count * 8
    }

    /// Returns the total length of the serialized entry.
    pub fn len(&self) -> usize {
        self.bz.len()
    }

    /// Returns whether the entry is empty.
    pub fn is_empty(&self) -> bool {
        self.bz.is_empty()
    }

    /// Returns the length of the payload data (excluding padding and tags).
    pub fn payload_len(&self) -> usize {
        let (key_len, value_len) = EntryBz::get_kv_len(self.bz);
        let dsn_count = self.bz[4] as usize;
        ENTRY_FIXED_LENGTH + key_len + value_len + dsn_count * 8
    }

    /// Computes the hash of this entry's payload.
    ///
    /// # Returns
    ///
    /// A 32-byte hash of the entry payload.
    pub fn hash(&self) -> Hash32 {
        let mut hash = [0u8; 32];
        blake2b_hash_inplace(&mut hash, &self.bz[..self.payload_len()]);
        hash
    }

    /// Returns a reference to the value bytes of this entry.
    pub fn value(&self) -> &'a [u8] {
        let (start, len) = self.get_value_start_and_len();
        &self.bz[start..start + len]
    }

    /// Returns the starting position and length of the value in the serialized data.
    ///
    /// # Returns
    ///
    /// A tuple of (start_position, value_length).
    pub fn get_value_start_and_len(&self) -> (usize, usize) {
        let (key_len, value_len) = EntryBz::get_kv_len(self.bz);
        let start = 5 + key_len;
        (start, value_len)
    }

    /// Returns a reference to the key bytes of this entry.
    pub fn key(&self) -> &[u8] {
        let key_len = self.bz[0] as usize;
        &self.bz[5..5 + key_len]
    }

    /// Computes the hash of this entry's key.
    ///
    /// Handles special cases for null entries (empty key) and sentry entries (empty value).
    ///
    /// # Returns
    ///
    /// A 32-byte hash of the key.
    pub fn key_hash(&self) -> Hash32 {
        // sentry entry: value is empty
        Entry::key_hash(self.key(), self.is_sentry_entry())
    }

    /// Returns the starting position of the next key hash in the serialized data.
    fn next_key_hash_start(&self) -> usize {
        let (key_len, value_len) = EntryBz::get_kv_len(&self.bz[..DSN_IDX]);
        5 + key_len + value_len
    }

    /// Returns a reference to the next key hash bytes.
    pub fn next_key_hash(&self) -> &[u8] {
        let start = self.next_key_hash_start();
        &self.bz[start..start + 32]
    }

    pub fn shard_id(&self) -> usize {
        let nkh = self.next_key_hash();
        let mut hi2 = ((nkh[0] as usize) << 8) | (nkh[1] as usize);
        let mut lo30_all_zero = true;
        for &b in &nkh[2..] {
            if b != 0 {
                lo30_all_zero = false;
                break;
            }
        }
        if lo30_all_zero {
            hi2 -= 1;
        }
        byte0_to_shard_id((hi2 >> 8) as u8)
    }

    /// Returns the version number of this entry.
    pub fn version(&self) -> i64 {
        let start = self.next_key_hash_start() + 32;
        read_u64_with_alignment(self.bz, start) as i64
    }

    /// Returns the serial number of this entry.
    pub fn serial_number(&self) -> u64 {
        let start = self.next_key_hash_start() + 40;
        read_u64_with_alignment(self.bz, start)
    }

    /// Returns the number of deactivated serial numbers in this entry.
    pub fn dsn_count(&self) -> usize {
        self.bz[DSN_IDX] as usize
    }

    /// Returns whether this entry is a sentry entry.
    pub fn is_sentry_entry(&self) -> bool {
        self.value().is_empty()
    }

    /// Returns the nth deactivated serial number.
    ///
    /// # Arguments
    ///
    /// * `n` - The index of the deactivated serial number to retrieve.
    ///
    /// # Returns
    ///
    /// The deactivated serial number at index `n`.
    ///
    /// # Panics
    ///
    /// Panics if `n` is >= `dsn_count()`.
    pub fn get_deactived_sn(&self, n: usize) -> u64 {
        assert!(n < self.dsn_count());
        let (key_len, value_len) = EntryBz::get_kv_len(self.bz);
        let start = ENTRY_FIXED_LENGTH + key_len + value_len + n * 8;
        read_u64_with_alignment(self.bz, start)
    }

    /// Returns an iterator over the deactivated serial numbers.
    ///
    /// # Returns
    ///
    /// A `DSNIter` that yields (index, serial_number) pairs.
    pub fn dsn_iter(&'a self) -> DSNIter<'a> {
        DSNIter {
            e: self,
            count: self.dsn_count(),
            idx: 0,
        }
    }
}

/// Reads a 64-bit unsigned integer from a buffer.
///
/// # Arguments
///
/// * `buf` - The buffer to read from.
/// * `offset` - The byte offset within the buffer.
///
/// # Returns
///
/// The 64-bit unsigned integer read from the buffer.
fn read_u64_with_alignment(buf: &[u8], offset: usize) -> u64 {
    LittleEndian::read_u64(&buf[offset..offset + 8])
}

/// Iterator over deactivated serial numbers in an entry.
///
/// This iterator yields (index, serial_number) pairs for each deactivated
/// serial number stored in the entry.
pub struct DSNIter<'a> {
    e: &'a EntryBz<'a>,
    count: usize,
    idx: usize,
}

impl Iterator for DSNIter<'_> {
    type Item = (usize, u64);

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx >= self.count {
            return None;
        }
        let sn = self.e.get_deactived_sn(self.idx);
        let idx = self.idx;
        self.idx += 1;
        Some((idx, sn))
    }
}

/// Converts an entry to a vector of bytes.
///
/// This is a convenience function that allocates a new vector and serializes
/// the entry into it.
///
/// # Arguments
///
/// * `e` - The entry to serialize.
/// * `deactived_sn_list` - List of deactivated serial numbers.
///
/// # Returns
///
/// A vector containing the serialized entry.
pub fn entry_to_vec(e: &Entry, deactived_sn_list: &[u64]) -> Vec<u8> {
    let total_len = (ENTRY_BASE_LENGTH + e.key.len() + e.value.len()).div_ceil(8) * 8
        + deactived_sn_list.len() * 8;
    let mut v = vec![0; total_len];
    e.dump(&mut v[..], deactived_sn_list);
    v
}

/// Compares a serialized entry with an entry struct for equality.
///
/// This function is useful for testing and validation purposes.
///
/// # Arguments
///
/// * `bz` - The serialized entry bytes.
/// * `e` - The entry struct to compare against.
/// * `deactived_sn_list` - List of deactivated serial numbers.
///
/// # Returns
///
/// `true` if the serialized entry matches the entry struct, `false` otherwise.
pub fn entry_equal(bz: &[u8], e: &Entry, deactived_sn_list: &[u64]) -> bool {
    &entry_to_vec(e, deactived_sn_list)[..] == bz
}

#[cfg(test)]
mod entry_bz_tests {
    use alloc::boxed::Box;

    use super::*;

    #[test]
    fn test_dump() {
        let key = "key".as_bytes();
        let val = "value".as_bytes();
        let next_key_hash: Hash32 = [0xab; 32];
        let deactived_sn_list: [u64; 4] = [0xf1, 0xf2, 0xf3, 0xf4];

        let entry = Entry {
            key,
            value: val,
            next_key_hash: &next_key_hash,
            version: 12345,
            serial_number: 99999,
        };

        let mut buf: [u8; 1024] = [0; 1024];
        let n = entry.dump(&mut buf, &deactived_sn_list);
        let mut len = 96;
        if cfg!(feature = "tee_cipher") {
            len += TAG_SIZE;
        }
        assert_eq!(len, n);

        #[rustfmt::skip]
        assert_eq!(
            hex::encode(&buf[0..n]),
            [
                "03",         // key len
                "050000",     // val len
                "04",         // deactived_sn_list len
                "6b6579",     // key
                "76616c7565", // val
                "abababababababababababababababababababababababababababababababab", // next key hash
                "3930000000000000", // version
                "9f86010000000000", // serial number
                "f100000000000000", // deactived_sn_list
                "f200000000000000",
                "f300000000000000",
                "f400000000000000",
                "000000",     // padding
                #[cfg(feature = "tee_cipher")]
                "00000000000000000000000000000000" // AES-GCM tag placeholder
        ].join(""),
        );
    }
}
