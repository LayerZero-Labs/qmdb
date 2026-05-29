#![allow(clippy::too_many_arguments)]

/// A module for managing and tracking changes to key-value pairs across multiple shards.
///
/// # Design
///
/// The `ChangeSet` system provides efficient tracking and application of changes to a key-value store,
/// with support for:
/// - Multiple operation types (create, read, write, delete)
/// - Sharded data organization
/// - Efficient storage of keys, values, and their hashes
/// - Operation sorting and batching
/// - Change history tracking
///
/// # Implementation Details
///
/// The implementation uses a compact storage format where:
/// - All keys, values, and hashes are stored in a single contiguous buffer
/// - Operations reference data by offset within this buffer
/// - Operations are organized by shard for efficient processing
/// - Changes can be applied selectively by shard or operation type
use crate::def::{OP_CREATE, OP_DELETE, SHARD_COUNT};
use crate::utils::{byte0_to_shard_id, extend_with_align8, OpRecord};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cmp::Ordering;

/// A single operation within a changeset.
///
/// Each operation references its data (key, value, hash) by offset within
/// the changeset's data buffer. All data fields are 8-byte aligned.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChangeOp {
    /// Type of operation (create, read, write, delete)
    pub op_type: u8,
    /// Shard ID this operation belongs to
    shard_id: u8,
    /// Length of the key
    key_len: u16,
    /// Length of the value
    value_len: u16,
    /// Length of the old value (for tracking changes)
    old_value_len: u16,
    /// Offset of the key in the data buffer (8-byte aligned)
    key_start: u32,
    /// Offset of the value in the data buffer (8-byte aligned)
    value_start: u32,
    /// Offset of the old value in the data buffer (8-byte aligned)
    old_value_start: u32,
    /// Offset of the key hash in the data buffer (8-byte aligned)
    key_hash_start: u32,
    /// Optional operation record for additional metadata
    rec: Option<Box<OpRecord>>,
}

impl ChangeOp {
    fn add(
        buf: &mut Vec<u8>,
        op_type: u8,
        key_hash: &[u8; 32],
        key: &[u8],
        value: &[u8],
        old_value: &[u8],
        rec: Option<Box<OpRecord>>,
    ) -> ChangeOp {
        // assert!(key.len() <= u16::MAX as usize);
        // assert!(value.len() <= u16::MAX as usize);
        // assert!(old_value.len() <= u16::MAX as usize);

        // Each field is written at an 8-byte aligned offset
        let key_start = buf.len();
        buf.extend_from_slice(key);
        extend_with_align8(buf);

        let value_start = buf.len();
        buf.extend_from_slice(value);
        extend_with_align8(buf);

        let old_value_start = buf.len();
        buf.extend_from_slice(old_value);
        extend_with_align8(buf);

        let key_hash_start = buf.len();
        buf.extend_from_slice(&key_hash[..]);
        // key_hash is 32 bytes, already 8-byte aligned

        ChangeOp {
            op_type,
            shard_id: byte0_to_shard_id(key_hash[0]) as u8,
            key_len: key.len() as u16,
            value_len: value.len() as u16,
            old_value_len: old_value.len() as u16,
            key_start: key_start as u32,
            value_start: value_start as u32,
            old_value_start: old_value_start as u32,
            key_hash_start: key_hash_start as u32,
            rec,
        }
    }

    #[inline(always)]
    fn read<'a>(
        &'a self,
        buf: &'a [u8],
    ) -> (
        u8,                        /* op_type */
        &'a [u8; 32],              /* key_hash */
        &'a [u8],                  /* key */
        &'a [u8],                  /* value */
        &'a [u8],                  /* old_value */
        Option<&'a Box<OpRecord>>, /* rec */
    ) {
        let key_start = self.key_start as usize;
        let value_start = self.value_start as usize;
        let old_value_start = self.old_value_start as usize;
        let key_hash_start = self.key_hash_start as usize;

        // SAFETY: key_hash is always 32 bytes, stored at key_hash_start (8-byte aligned)
        let key_hash: &[u8; 32] =
            unsafe { &*(buf.as_ptr().add(key_hash_start) as *const [u8; 32]) };
        let key = &buf[key_start..key_start + self.key_len as usize];
        let value = &buf[value_start..value_start + self.value_len as usize];
        let old_value = &buf[old_value_start..old_value_start + self.old_value_len as usize];
        (
            self.op_type,
            key_hash,
            key,
            value,
            old_value,
            self.rec.as_ref(),
        )
    }

    /// Get the shard ID this operation belongs to.
    #[inline]
    pub fn shard_id(&self) -> usize {
        self.shard_id as usize
    }

    /// Get the key hash (32 bytes) from the data buffer.
    #[inline]
    pub fn key_hash<'a>(&self, buf: &'a [u8]) -> &'a [u8; 32] {
        let start = self.key_hash_start as usize;
        // SAFETY: key_hash is always 32 bytes, stored at key_hash_start (8-byte aligned)
        unsafe { &*(buf.as_ptr().add(start) as *const [u8; 32]) }
    }
}

/// A collection of changes to be applied to a key-value store.
///
/// The `ChangeSet` efficiently stores multiple operations and their associated data
/// in a format optimized for both storage and application of changes.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ChangeSet {
    /// Contiguous buffer storing all keys, values, and key hashes
    pub data: Vec<u8>,
    /// List of operations referencing data in the buffer
    pub op_list: Vec<ChangeOp>,
    /// Starting index of operations for each shard
    shard_starts: [u32; SHARD_COUNT],
    /// Number of operations in each shard
    shard_op_count: [u32; SHARD_COUNT],
    /// Whether the changeset contains any create or delete operations
    has_create_del_op: bool,
    sorted: bool,
}

impl core::fmt::Debug for ChangeSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Print semantic content rather than internal buffer layout
        write!(
            f,
            "ChangeSet {{ \"sorted\": {}, \"has_create_del\": {}, \"op_count\": {}, \"ops\": [",
            self.sorted,
            self.has_create_del_op,
            self.op_list.len()
        )?;

        let mut first = true;
        for (index, op) in self.op_list.iter().enumerate() {
            if !first {
                write!(f, ", ")?;
            }
            first = false;

            let (op_type, key_hash, key, value, old_value, _rec) = op.read(&self.data);
            let shard_id = byte0_to_shard_id(key_hash[0]);
            write!(
                f,
                "{{ \"index\": {}, \"op_type\": {}, \"shard_id\": {}, \"key_hash\": \"{}\", \"key\": \"{}\", \"value\": \"{}\", \"old_value\": \"{}\" }}",
                index,
                op_type,
                shard_id,
                hex::encode(key_hash),
                hex::encode(key),
                hex::encode(value),
                hex::encode(old_value)
            )?;
        }

        write!(f, "] }}")
    }
}

impl PartialEq for ChangeSet {
    fn eq(&self, other: &Self) -> bool {
        self.eq_impl(other, true)
    }
}

impl Eq for ChangeSet {}

impl Default for ChangeSet {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangeSet {
    /// Compare two ChangeSets ignoring old_value field.
    /// Useful for comparing fullnode (track_old_value=true) and stateless (track_old_value=false) results.
    #[inline]
    pub fn eq_without_old_value(&self, other: &Self) -> bool {
        self.eq_impl(other, false)
    }

    /// Internal comparison implementation.
    /// If `compare_old_value` is true, compares old_value fields.
    /// If `compare_old_value` is false, requires other's old_value to be empty.
    fn eq_impl(&self, other: &Self, compare_old_value: bool) -> bool {
        // Only meaningful for sorted ChangeSets
        if !self.sorted || !other.sorted {
            return false;
        }

        if self.op_list.len() != other.op_list.len() {
            return false;
        }

        for (lhs, rhs) in self.op_list.iter().zip(other.op_list.iter()) {
            if lhs.op_type != rhs.op_type {
                return false;
            }

            let (lhs_op_type, lhs_key_hash, lhs_key, lhs_value, lhs_old_value, _) =
                lhs.read(&self.data);
            let (rhs_op_type, rhs_key_hash, rhs_key, rhs_value, rhs_old_value, _) =
                rhs.read(&other.data);

            let base_match = lhs_op_type == rhs_op_type
                && lhs_key_hash == rhs_key_hash
                && lhs_key == rhs_key
                && lhs_value == rhs_value;

            let old_value_match = if compare_old_value {
                lhs_old_value == rhs_old_value
            } else {
                // When not comparing old_value, other's old_value must be empty
                rhs_old_value.is_empty()
            };

            if !base_match || !old_value_match {
                return false;
            }
        }

        true
    }

    /// Creates a new empty changeset with pre-allocated capacity.
    pub fn new() -> Self {
        Self {
            data: Vec::with_capacity(1000),
            op_list: Vec::with_capacity(10),
            shard_starts: [u32::MAX; SHARD_COUNT],
            shard_op_count: [0u32; SHARD_COUNT],
            has_create_del_op: false,
            sorted: false,
        }
    }

    /// Creates a new empty changeset without pre-allocated capacity.
    pub fn new_uninit() -> Self {
        Self {
            data: Vec::with_capacity(0),
            op_list: Vec::with_capacity(0),
            shard_starts: [u32::MAX; SHARD_COUNT],
            shard_op_count: [0u32; SHARD_COUNT],
            has_create_del_op: false,
            sorted: true,
        }
    }

    /// Creates a new empty changeset with estimated capacity based on operation counts.
    ///
    /// # Arguments
    ///
    /// * `account_op_count` - Expected number of account operations
    /// * `storage_op_count` - Expected number of storage slot operations
    /// * `track_old_value` - Whether old values will be tracked (increases buffer size)
    ///
    /// # Capacity Estimation
    ///
    /// Account operation:
    /// - key: 24 bytes (20 addr + padding), 8-byte aligned
    /// - value: 48 bytes (balance + nonce + code_hash), 8-byte aligned
    /// - old_value: 48 bytes (if tracking)
    /// - key_hash: 32 bytes
    /// - Total: 104 bytes (without old) / 152 bytes (with old)
    ///
    /// Storage operation:
    /// - key: 56 bytes (20 addr + 32 slot + padding), 8-byte aligned
    /// - value: 40 bytes (32 value + 7 dust + padding), 8-byte aligned
    /// - old_value: 40 bytes (if tracking)
    /// - key_hash: 32 bytes
    /// - Total: 128 bytes (without old) / 168 bytes (with old)
    pub fn with_capacity(
        account_op_count: usize,
        storage_op_count: usize,
        track_old_value: bool,
    ) -> Self {
        const ACCOUNT_BYTES_WITHOUT_OLD: usize = 104;
        const ACCOUNT_BYTES_WITH_OLD: usize = 152;
        const STORAGE_BYTES_WITHOUT_OLD: usize = 128;
        const STORAGE_BYTES_WITH_OLD: usize = 168;

        let data_capacity = if track_old_value {
            account_op_count * ACCOUNT_BYTES_WITH_OLD + storage_op_count * STORAGE_BYTES_WITH_OLD
        } else {
            account_op_count * ACCOUNT_BYTES_WITHOUT_OLD
                + storage_op_count * STORAGE_BYTES_WITHOUT_OLD
        };

        let op_count = account_op_count + storage_op_count;

        Self {
            data: Vec::with_capacity(data_capacity),
            op_list: Vec::with_capacity(op_count),
            shard_starts: [u32::MAX; SHARD_COUNT],
            shard_op_count: [0u32; SHARD_COUNT],
            has_create_del_op: false,
            sorted: false,
        }
    }

    /// Adds an operation from an existing operation record.
    ///
    /// # Arguments
    ///
    /// * `rec` - The operation record to add
    ///
    /// # Panics
    ///
    /// * If the shard ID derived from the key hash doesn't match the record's shard ID
    pub fn add_op_rec(&mut self, rec: OpRecord) {
        assert!(!self.sorted, "Already sorted");
        let rec_box = Box::new(rec);
        let key_hash = crate::entryfile::entry::entry_key_hash(&rec_box.key[..]);
        let (k, v) = (&rec_box.key[..], &rec_box.value[..]);

        let shard_id = byte0_to_shard_id(key_hash[0]) as u8;
        if shard_id != rec_box.shard_id as u8 {
            panic!(
                "mismatch: shard_id={} key_hash={:?} rec={:?}",
                shard_id, key_hash, *rec_box
            );
        }

        self.has_create_del_op |= matches!(rec_box.op_type, OP_DELETE | OP_CREATE);

        self.op_list.push(ChangeOp::add(
            &mut self.data,
            rec_box.op_type,
            &key_hash,
            &k.to_vec(),
            &v.to_vec(),
            &[],
            Some(rec_box),
        ));
    }

    /// Adds an operation with both new and old values.
    ///
    /// # Arguments
    ///
    /// * `op_type` - Type of operation (create, read, write, delete)
    /// * `shard_id` - ID of the shard this operation belongs to
    /// * `key_hash` - Hash of the key
    /// * `k` - Key bytes
    /// * `v` - New value bytes
    /// * `old_v` - Old value bytes
    /// * `rec` - Optional operation record for additional metadata
    pub fn add_op_with_old_value(
        &mut self,
        op_type: u8,
        key_hash: &[u8; 32],
        k: &[u8],
        v: &[u8],
        old_v: &[u8],
        rec: Option<Box<OpRecord>>,
    ) {
        assert!(!self.sorted, "Already sorted");
        self.has_create_del_op |= matches!(op_type, OP_DELETE | OP_CREATE);

        self.op_list.push(ChangeOp::add(
            &mut self.data,
            op_type,
            key_hash,
            k,
            v,
            old_v,
            rec,
        ));
    }

    /// Adds an operation without an old value.
    ///
    /// # Arguments
    ///
    /// * `op_type` - Type of operation (create, read, write, delete)
    /// * `shard_id` - ID of the shard this operation belongs to
    /// * `key_hash` - Hash of the key
    /// * `k` - Key bytes
    /// * `v` - Value bytes
    /// * `rec` - Optional operation record for additional metadata
    pub fn add_op(
        &mut self,
        op_type: u8,
        key_hash: &[u8; 32],
        k: &[u8],
        v: &[u8],
        rec: Option<Box<OpRecord>>,
    ) {
        assert!(!self.sorted, "Already sorted");
        self.has_create_del_op |= matches!(op_type, OP_DELETE | OP_CREATE);

        self.op_list.push(ChangeOp::add(
            &mut self.data,
            op_type,
            key_hash,
            k,
            v,
            &[],
            rec,
        ));
    }

    /// Sorts operations by shard ID, key hash, and operation type.
    ///
    /// This method also updates the shard start indices and operation counts.
    pub fn sort(&mut self) {
        assert!(!self.sorted, "Already sorted");
        self.sorted = true;
        self.op_list.sort_by(|a, b| {
            let res0 = a.shard_id.cmp(&b.shard_id);
            if res0 != Ordering::Equal {
                return res0;
            }

            let x = a.read(&self.data).1;
            let y = b.read(&self.data).1;
            let res1 = x.cmp(y);

            // cannot has same key hash in single changeset
            assert!(res1 != Ordering::Equal);
            res1
        });
        for idx in 0..self.op_list.len() {
            let shard_id = self.op_list[idx].shard_id as usize;
            self.shard_op_count[shard_id] += 1;
            if self.shard_starts[shard_id] == u32::MAX {
                self.shard_starts[shard_id] = idx as u32;
            }
        }
    }

    /// Applies operations in order, providing access to both old and new values.
    ///
    /// # Arguments
    ///
    /// * `access` - Function to call for each operation with:
    ///   - Operation type
    ///   - Key hash
    ///   - Key
    ///   - New value
    ///   - Old value
    ///   - Optional operation record
    #[inline(always)]
    pub fn for_each_op<F>(&self, mut access: F)
    where
        F: FnMut(u8, usize, &[u8; 32], &[u8], &[u8], &[u8], Option<&Box<OpRecord>>),
    {
        assert!(self.sorted, "Not sorted");
        for op in self.op_list.iter() {
            let (op_type, key_hash, key, value, old_value, rec) = op.read(&self.data);
            access(
                op_type,
                op.shard_id as usize,
                key_hash,
                key,
                value,
                old_value,
                rec,
            );
        }
    }

    /// Returns the number of operations in a specific shard.
    #[inline]
    pub fn op_count_in_shard(&self, shard_id: usize) -> usize {
        self.shard_op_count[shard_id] as usize
    }

    /// Checks if the changeset contains any create or delete operations.
    #[inline]
    pub fn has_create_del(&self) -> bool {
        self.has_create_del_op
    }

    /// Applies operations for a specific shard in order.
    pub fn run_in_shard<F>(&self, shard_id: usize, mut access: F)
    where
        F: FnMut(u8, &[u8; 32], &[u8], &[u8], Option<&Box<OpRecord>>),
    {
        assert!(self.sorted, "Not sorted");
        let shard_start = self.shard_starts[shard_id];
        if shard_start == u32::MAX {
            return;
        }
        let mut idx = shard_start as usize;
        while idx < self.op_list.len() {
            let op = &self.op_list[idx];
            if shard_id != op.shard_id as usize {
                break;
            }
            let (op_type, key_hash, key, value, _old_value, rec) = op.read(&self.data);
            access(op_type, key_hash, key, value, rec);
            idx += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::OpRecord;
    use alloc::boxed::Box;

    use alloc::vec;

    #[test]
    fn test_changeset_new() {
        let changeset = ChangeSet::new();
        assert_eq!(changeset.data.capacity(), 1000);
        assert_eq!(changeset.op_list.capacity(), 10);
        assert_eq!(changeset.shard_starts.len(), SHARD_COUNT);
        assert!(changeset
            .shard_starts
            .iter()
            .all(|&start| start == u32::MAX));
    }

    #[test]
    fn test_add_op() {
        let mut changeset = ChangeSet::new();
        let key_hash = [1u8; 32];
        let key = vec![1, 2, 3];
        let value = vec![4, 5, 6];
        let rec = Some(Box::new(OpRecord::new(1)));

        changeset.add_op(1, &key_hash, &key, &value, rec.clone());

        // With 8-byte alignment:
        // key: [1,2,3] at offset 0, padded to 8 bytes -> [1,2,3,0,0,0,0,0]
        // value: [4,5,6] at offset 8, padded to 8 bytes -> [4,5,6,0,0,0,0,0]
        // old_value: [] at offset 16, no padding needed (already aligned)
        // key_hash: [1;32] at offset 16
        assert_eq!(
            changeset.data,
            vec![
                1, 2, 3, 0, 0, 0, 0, 0, // key + padding
                4, 5, 6, 0, 0, 0, 0, 0, // value + padding
                // old_value is empty, no bytes
                1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // key_hash part 1
                1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // key_hash part 2
            ]
        );
        assert_eq!(changeset.op_list.len(), 1);

        let op = &changeset.op_list[0];
        assert_eq!(op.op_type, 1);
        assert_eq!(op.shard_id, 0);
        assert_eq!(op.key_start, 0);
        assert_eq!(op.key_len, 3);
        assert_eq!(op.value_start, 8);
        assert_eq!(op.value_len, 3);
        assert_eq!(op.old_value_start, 16);
        assert_eq!(op.old_value_len, 0);
        assert_eq!(op.key_hash_start, 16);
        assert_eq!(op.rec, rec);
    }

    #[test]
    fn test_for_each_op() {
        let mut changeset = ChangeSet::new();
        let key = vec![1, 2, 3];
        let value = vec![4, 5, 6];
        let rec = Some(Box::new(OpRecord::new(1)));

        changeset.add_op(0, &[0u8; 32], &key, &value, rec.clone());
        changeset.add_op(1, &[1u8; 32], &key, &value, rec.clone());
        changeset.sort();

        let mut count = 0;
        changeset.for_each_op(|op_type, _, kh, k, v, old_v, r| {
            assert_eq!(op_type, count);
            assert_eq!(kh, &[count; 32]);
            assert_eq!(k, &key[..]);
            assert_eq!(v, &value[..]);
            assert_eq!(old_v, &[] as &[u8; 0]);
            assert_eq!(r, rec.as_ref());
            count += 1;
        });
        assert_eq!(count, 2);
    }

    #[test]
    fn test_sort() {
        let mut changeset = ChangeSet::new();

        let key_hash1 = [1u8; 32];
        let key1 = vec![1, 2, 3];
        let value1 = vec![4, 5, 6];
        let key_hash2 = [2u8; 32];
        let key2 = vec![7, 8, 9];
        let value2 = vec![10, 11, 12];

        changeset.add_op(1, &key_hash2, &key2, &value2, None);
        changeset.add_op(1, &key_hash1, &key1, &value1, None);

        assert!(!changeset.sorted);
        changeset.sort();
        assert!(changeset.sorted);

        assert_eq!(changeset.op_list.len(), 2);
        assert_eq!(changeset.op_list[0].shard_id, 0);
        assert_eq!(changeset.op_list[1].shard_id, 0);
    }

    #[test]
    fn test_run_in_shard() {
        let mut changeset = ChangeSet::new();

        let key = vec![1, 2, 3];
        let value = vec![4, 5, 6];
        let rec = Some(Box::new(OpRecord::new(1)));

        changeset.add_op(3, &[100u8; 32], &key, &value, rec.clone());
        changeset.add_op(1, &[1u8; 32], &key, &value, rec.clone());
        changeset.add_op(2, &[0u8; 32], &key, &value, rec.clone());
        changeset.sort();

        assert_eq!(changeset.op_list[0].shard_id, 0);
        assert_eq!(changeset.op_list[0].op_type, 2);
        assert_eq!(changeset.op_list[1].shard_id, 0);
        assert_eq!(changeset.op_list[1].op_type, 1);
        assert_eq!(changeset.op_list[2].shard_id, 6);
        assert_eq!(changeset.op_list[2].op_type, 3);

        let mut accessed = false;
        changeset.run_in_shard(3, |_, _, _, _, _| {
            accessed = true;
        });
        assert!(!accessed);

        changeset.run_in_shard(6, |op_type, kh, k, v, r| {
            assert_eq!(op_type, 3);
            assert_eq!(kh, &[100u8; 32]);
            assert_eq!(k, &key[..]);
            assert_eq!(v, &value[..]);
            assert_eq!(r, rec.as_ref());
            accessed = true;
        });

        assert!(accessed);
    }

    #[test]
    fn test_changeset_eq_sorted_semantics() {
        let mut cs1 = ChangeSet::new();
        let mut cs2 = ChangeSet::new();

        // First changeset: add ops in order A then B
        let key_a = vec![0x01, 0x02];
        let val_a_new = vec![0x0A];
        let key_hash_a = [0x11u8; 32];
        cs1.add_op(1, &key_hash_a, &key_a, &val_a_new, None);

        let key_b = vec![0x03, 0x04, 0x05];
        let val_b_old = vec![0x20, 0x21];
        let val_b_new = vec![0x30, 0x31];
        let key_hash_b = [0x22u8; 32];
        cs1.add_op_with_old_value(2, &key_hash_b, &key_b, &val_b_new, &val_b_old, None);

        // Second changeset: add same logical ops but append to buffer in reverse order
        cs2.add_op_with_old_value(2, &key_hash_b, &key_b, &val_b_new, &val_b_old, None);
        cs2.add_op(1, &key_hash_a, &key_a, &val_a_new, None);

        cs1.sort();
        cs2.sort();

        assert_eq!(cs1, cs2);

        // Mutate content in cs2 to differ
        let mut cs3 = ChangeSet::new();
        let val_b_new_diff = vec![0x30, 0x32]; // different last byte
        cs3.add_op_with_old_value(2, &key_hash_b, &key_b, &val_b_new_diff, &val_b_old, None);
        cs3.add_op(1, &key_hash_a, &key_a, &val_a_new, None);
        cs3.sort();

        assert_ne!(cs1, cs3);
    }
}
