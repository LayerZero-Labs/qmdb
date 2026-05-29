use crate::entryfile::entry::{Entry, EntryBz};

use super::entrymutator::{EntryIndexer, EntryMutationInspector};

/// A no-op implementation of the EntryIndexer trait
/// A helper Indexer implementation that performs no operations, used to simplify
/// EntryMutator implementations by providing a no-op indexing option when actual
/// indexing is not needed
pub struct NoopIndexer;

impl EntryIndexer for NoopIndexer {
    fn add_kv(&self, _key: &[u8], _position: i64, _serial_number: u64) {
        // No-op
    }

    fn change_kv(
        &self,
        _key: &[u8],
        _old_position: i64,
        _new_position: i64,
        _old_serial_number: u64,
        _new_serial_number: u64,
    ) {
        // No-op
    }

    fn erase_kv(&self, _key: &[u8], _position: i64, _serial_number: u64) {
        // No-op
    }
}

/// A no-op implementation of EntryMutationInspector that does nothing.
/// Used to make EntryMutator implementation concise when inspection is not needed.
pub struct NoopInspector;

impl EntryMutationInspector for NoopInspector {
    fn on_read_entry(&mut self, _entry: &EntryBz) {}
    fn on_append_entry(&mut self, _entry: &Entry) {}
    fn on_deactivate_entry(&mut self, _entry: &EntryBz) {}
}
