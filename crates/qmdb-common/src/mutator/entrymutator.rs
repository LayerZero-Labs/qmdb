use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crate::{
    entryfile::{
        entry::{Entry, EntryBz},
        readbuf::ReadBuf,
    },
    indexer::indexer_trait::IndexerTrait,
    task::taskid::split_task_id,
    utils::{hasher::Hash32, OpRecord},
};

use super::{NoopIndexer, NoopInspector};

/// Trait for storage operations on entries
pub trait EntryStorage {
    /// Look up the previous entry in the sorted key space - the entry whose next_key_hash points to the given key_hash.
    /// Returns the position of the previous entry if found.
    fn read_previous_entry<F>(
        &mut self,
        height: i64,
        key_hash: &Hash32,
        read_buf: &mut ReadBuf,
        predicate: F,
    ) -> i64
    where
        F: Fn(&EntryBz) -> bool;

    /// Look up the historical version of an entry with the given key_hash that matches the predicate.
    /// Returns the position of the prior entry if found.
    fn read_prior_entry<F>(
        &mut self,
        height: i64,
        key_hash: &Hash32,
        read_buf: &mut ReadBuf,
        predicate: F,
    ) -> i64
    where
        F: Fn(&EntryBz) -> bool;

    /// Append a new entry and return its position
    fn append_entry(&mut self, entry: &Entry, deactivated_sns: &[u64]) -> i64;

    /// Compact the entry file
    fn try_twice_compact(&mut self);
}

/// Trait for indexer operations required by EntryMutator
pub trait EntryIndexer {
    /// Add a new key-value entry to the index
    fn add_kv(&self, key: &[u8], position: i64, serial_number: u64);

    /// Change the position of an existing key-value entry
    fn change_kv(&self, k80: &[u8], pos_old: i64, pos_new: i64, sn_old: u64, sn_new: u64);

    /// Remove a key-value entry from the index
    fn erase_kv(&self, key: &[u8], position: i64, serial_number: u64);
}

/// Trait for inspecting entry mutations during database operations.
///
/// This trait provides hooks for monitoring entry operations such as
/// reading, appending, and deactivating entries.
pub trait EntryMutationInspector {
    /// Called when an entry is read from storage
    fn on_read_entry(&mut self, entry: &EntryBz);

    /// Called when a new entry is appended to storage
    fn on_append_entry(&mut self, entry: &Entry);

    /// Called when an entry is deactivated in storage
    fn on_deactivate_entry(&mut self, entry: &EntryBz);
}

/// Represents the result of an entry mutation operation
#[derive(Debug)]
pub struct EntryMutationResult {
    /// The number of actived(appended) entries due to the mutation
    pub num_active: u32,
    /// The number of deactivated entries due to the mutation
    pub num_deactive: u32,
}

/// Handles mutations of key-value entries with efficient indexing
pub struct EntryMutator<'a, S: EntryStorage, I: EntryIndexer, P: EntryMutationInspector> {
    storage: &'a mut S,
    indexer: &'a I,
    inspector: &'a mut P,
}

impl<'a, S: EntryStorage, I: EntryIndexer, P: EntryMutationInspector> EntryMutator<'a, S, I, P> {
    /// Create a new EntryMutator with the given storage and indexer
    pub fn new(storage: &'a mut S, indexer: &'a I, inspector: &'a mut P) -> Self {
        Self {
            storage,
            indexer,
            inspector,
        }
    }

    /// Create a new key-value entry
    pub fn create_kv(
        &mut self,
        read_entry_buf: &mut ReadBuf,
        key_hash: &Hash32,
        key: &[u8],
        value: &[u8],
        _shard_id: usize,
        version: i64,
        serial_number: u64,
        _r: Option<&Box<OpRecord>>,
    ) -> EntryMutationResult {
        // cannot create empty value
        assert!(!value.is_empty());

        let height = split_task_id(version).0;

        // Lookup previous entry
        let old_pos = self
            .storage
            .read_previous_entry(height, key_hash, read_entry_buf, |entry| {
                entry.key_hash() < *key_hash && &key_hash[..] < entry.next_key_hash()
            });

        let prev_entry = read_entry_buf.as_entry_bz();
        self.inspector.on_read_entry(&prev_entry);

        // Create new entry
        let new_entry = Entry {
            key,
            value,
            next_key_hash: prev_entry.next_key_hash(),
            version,
            serial_number,
        };
        let dsn_list: [u64; 0] = []; // No entries deactivated for new entry (stack allocation)
        let create_pos = self.storage.append_entry(&new_entry, &dsn_list);
        self.inspector.on_append_entry(&new_entry);

        // Update previous entry
        self.inspector.on_deactivate_entry(&prev_entry);

        let prev_changed = Entry {
            key: prev_entry.key(),
            value: prev_entry.value(),
            next_key_hash: &key_hash[..],
            version,
            serial_number: serial_number + 1,
        };
        let dsn_list = [prev_entry.serial_number()];
        let new_pos = self.storage.append_entry(&prev_changed, &dsn_list[..]);
        self.inspector.on_append_entry(&prev_changed);

        // Update indices
        self.indexer
            .add_kv(&key_hash[..], create_pos, serial_number);
        self.indexer.change_kv(
            &prev_entry.key_hash()[..10],
            old_pos,
            new_pos,
            prev_entry.serial_number(),
            serial_number + 1,
        );

        self.try_twice_compact();

        EntryMutationResult {
            num_active: 2,
            num_deactive: 1,
        }
    }

    /// Write (update) an existing key-value entry
    pub fn update_kv(
        &mut self,
        read_entry_buf: &mut ReadBuf,
        key_hash: &Hash32,
        key: &[u8],
        value: &[u8],
        _shard_id: usize,
        version: i64,
        serial_number: u64,
        _r: Option<&Box<OpRecord>>,
    ) -> EntryMutationResult {
        // cannot create empty value
        assert!(!value.is_empty());

        let height = split_task_id(version).0;

        // Lookup existing entry
        let old_pos = self
            .storage
            .read_prior_entry(height, key_hash, read_entry_buf, |entry| entry.key() == key);

        let old_entry = read_entry_buf.as_entry_bz();
        self.inspector.on_read_entry(&old_entry);

        // Update entry
        self.inspector.on_deactivate_entry(&old_entry);

        let new_entry = Entry {
            key,
            value,
            next_key_hash: old_entry.next_key_hash(),
            version,
            serial_number,
        };
        let dsn_list = [old_entry.serial_number()]; // Stack allocation
        let new_pos = self.storage.append_entry(&new_entry, &dsn_list);
        self.inspector.on_append_entry(&new_entry);

        // Update index
        self.indexer.change_kv(
            &key_hash[..],
            old_pos,
            new_pos,
            old_entry.serial_number(),
            serial_number,
        );

        self.try_twice_compact();

        EntryMutationResult {
            num_active: 1,
            num_deactive: 1,
        }
    }

    /// Delete an existing key-value entry
    pub fn delete_kv(
        &mut self,
        read_entry_buf: &mut ReadBuf,
        key_hash: &Hash32,
        key: &[u8],
        _shard_id: usize,
        version: i64,
        serial_number: u64,
        _r: Option<&Box<OpRecord>>,
    ) -> EntryMutationResult {
        let height = split_task_id(version).0;

        // Lookup entry to delete
        let del_entry_pos =
            self.storage
                .read_prior_entry(height, key_hash, read_entry_buf, |entry| entry.key() == key);

        let del_entry = read_entry_buf.as_entry_bz();
        self.inspector.on_read_entry(&del_entry);

        let del_entry_sn = del_entry.serial_number();
        let mut old_next_key_hash = [0; 32];
        old_next_key_hash.copy_from_slice(del_entry.next_key_hash());

        // Deactivate the entry to delete here, because read_previous_entry will reuse the read_entry_buf
        self.inspector.on_deactivate_entry(&del_entry);

        // Lookup previous entry
        read_entry_buf.clear();
        let prev_pos =
            self.storage
                .read_previous_entry(height, key_hash, read_entry_buf, |entry| {
                    entry.next_key_hash() == key_hash
                });

        let prev_entry = read_entry_buf.as_entry_bz();
        self.inspector.on_read_entry(&prev_entry);

        self.inspector.on_deactivate_entry(&prev_entry);

        // Update previous entry
        let prev_changed = Entry {
            key: prev_entry.key(),
            value: prev_entry.value(),
            next_key_hash: &old_next_key_hash[..],
            version,
            serial_number,
        };
        let dsn_list = [del_entry_sn, prev_entry.serial_number()]; // Stack allocation
        let new_pos = self.storage.append_entry(&prev_changed, &dsn_list);
        self.inspector.on_append_entry(&prev_changed);

        // Update indices
        self.indexer
            .erase_kv(&key_hash[..], del_entry_pos, del_entry_sn);
        let mut k80 = [0; 10];
        k80.copy_from_slice(&prev_entry.key_hash()[..10]);
        self.indexer.change_kv(
            &k80[..],
            prev_pos,
            new_pos,
            prev_entry.serial_number(),
            serial_number,
        );

        self.try_twice_compact();

        EntryMutationResult {
            num_active: 1,
            num_deactive: 2,
        }
    }

    /// Compact the entry file
    pub fn try_twice_compact(&mut self) {
        self.storage.try_twice_compact();
    }
}

/// Looks up an entry in a position list that satisfies the given predicate.
///
/// # Arguments
/// * `pos_list` - Iterator of positions to search through
/// * `read_entry` - Function to read an entry at a given position into a buffer
/// * `read_buf` - Buffer to use for reading entries
/// * `predicate` - Function that determines if an entry matches the search criteria
///
/// # Returns
/// * `Some(pos)` - Position of the first entry that satisfies the predicate
/// * `None` - If no entry satisfies the predicate
///
/// # Type Parameters
/// * `F` - Type of the predicate function
/// * `R` - Type of the read entry function
/// * `P` - Type of the position list iterator
///
/// # Example
/// ```no_run
/// # use qmdb_common::entryfile::readbuf::ReadBuf;
/// # use qmdb_common::entryfile::entry::EntryBz;
/// # use qmdb_common::mutator::entrymutator::lookup_entry_in_pos_list;
///
/// let positions = vec![1, 2, 3].into_iter();
/// let mut read_buf = ReadBuf::new();
/// let result = lookup_entry_in_pos_list(
///     positions,
///     |pos, buf| { /* read entry at pos into buf */ },
///     &mut read_buf,
///     |pos: i64, entry: &EntryBz| entry.key() == b"target_key"
/// );
/// ```
pub fn lookup_entry_in_pos_list<F, R, P>(
    pos_list: P,
    mut read_entry: R,
    read_buf: &mut ReadBuf,
    predicate: F,
) -> Option<i64>
where
    F: Fn(i64, &EntryBz) -> bool,
    R: FnMut(i64, &mut ReadBuf),
    P: Iterator<Item = i64>,
{
    for pos in pos_list {
        read_buf.clear();
        read_entry(pos, read_buf);
        let entry = read_buf.as_entry_bz();
        if predicate(pos, &entry) {
            return Some(pos);
        }
    }
    None
}

/// Builder for creating EntryMutator instances with different configurations.
///
/// This builder allows for step-by-step configuration of an EntryMutator,
/// starting with storage and optionally adding an indexer and inspector.
pub struct EntryMutatorBuilder<'a, S: EntryStorage, I: EntryIndexer, P: EntryMutationInspector> {
    storage: &'a mut S,
    indexer: &'a I,
    inspector: &'a mut P,
}

impl<'a, S: EntryStorage> EntryMutatorBuilder<'a, S, NoopIndexer, NoopInspector> {
    /// Creates a new EntryMutatorBuilder with the provided storage.
    ///
    /// By default, uses no-op implementations for indexer and inspector.
    pub fn new(storage: &'a mut S) -> Self {
        static NOOP_INDEXER: NoopIndexer = NoopIndexer;
        static mut NOOP_INSPECTOR: NoopInspector = NoopInspector;

        Self {
            storage,
            indexer: &NOOP_INDEXER,
            // SAFETY: NoopInspector is a zero-sized type (ZST) with no data and no-op methods.
            // Since it contains no data, there's no risk of data races or memory corruption.
            // The mutable reference is only used to satisfy the EntryMutationInspector trait bounds.
            inspector: unsafe { &mut NOOP_INSPECTOR },
        }
    }
}

impl<'a, S: EntryStorage, I: EntryIndexer, P: EntryMutationInspector>
    EntryMutatorBuilder<'a, S, I, P>
{
    /// Sets the indexer for the EntryMutator.
    ///
    /// # Arguments
    /// * `indexer` - The indexer to use for tracking entry positions
    pub fn with_indexer<M: EntryIndexer>(self, indexer: &'a M) -> EntryMutatorBuilder<'a, S, M, P> {
        EntryMutatorBuilder {
            storage: self.storage,
            indexer,
            inspector: self.inspector,
        }
    }

    /// Sets the inspector for the EntryMutator.
    ///
    /// # Arguments
    /// * `inspector` - The inspector to use for monitoring entry mutations
    pub fn with_inspector<Q: EntryMutationInspector>(
        self,
        inspector: &'a mut Q,
    ) -> EntryMutatorBuilder<'a, S, I, Q> {
        EntryMutatorBuilder {
            storage: self.storage,
            indexer: self.indexer,
            inspector,
        }
    }

    /// Builds the final EntryMutator instance.
    ///
    /// # Returns
    /// A configured EntryMutator ready for use
    pub fn build(self) -> EntryMutator<'a, S, I, P> {
        EntryMutator::new(self.storage, self.indexer, self.inspector)
    }
}

/// Implements EntryIndexer for any type that implements IndexerTrait.
///
/// This blanket implementation allows any IndexerTrait implementation
/// to be used as an EntryIndexer with EntryMutator.
impl<T: IndexerTrait> EntryIndexer for T {
    fn add_kv(&self, key: &[u8], position: i64, serial_number: u64) {
        IndexerTrait::add_kv(self, key, position, serial_number);
    }

    fn change_kv(&self, k80: &[u8], pos_old: i64, pos_new: i64, sn_old: u64, sn_new: u64) {
        IndexerTrait::change_kv(self, k80, pos_old, pos_new, sn_old, sn_new)
    }

    fn erase_kv(&self, key: &[u8], position: i64, serial_number: u64) {
        IndexerTrait::erase_kv(self, key, position, serial_number);
    }
}

#[cfg(test)]
mod tests {

    // TODO: Add tests for EntryMutator operations
}
