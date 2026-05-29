use std::sync::Arc;

use qmdb_common::utils::changeset::ChangeSet;

use crate::entryfile::EntryCache;

pub trait TaskHub: Send + Sync {
    fn get_entry_cache(&self, height: i64) -> Arc<EntryCache>;
    fn get_change_sets(&self, task_id: i64) -> Arc<Vec<ChangeSet>>;
}
