use super::task::TaskHub;
use super::tasksmanager::TasksManager;
use crate::entryfile::EntryCache;
use atomptr::AtomPtr;
use qmdb_common::task::task::Task;
use qmdb_common::task::taskid::split_task_id;
use qmdb_common::utils::changeset::ChangeSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

pub struct BlockPairTaskHub<T: Task> {
    tasks_in_blk0: AtomPtr<Arc<TasksManager<T>>>,
    tasks_in_blk1: AtomPtr<Arc<TasksManager<T>>>,
    height0: AtomicI64,
    height1: AtomicI64,
    cache0: AtomPtr<Arc<EntryCache>>,
    cache1: AtomPtr<Arc<EntryCache>>,
}

impl<T: Task> Default for BlockPairTaskHub<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Task> BlockPairTaskHub<T> {
    pub fn new() -> Self {
        Self {
            tasks_in_blk0: AtomPtr::new(Arc::new(TasksManager::<T>::default())),
            tasks_in_blk1: AtomPtr::new(Arc::new(TasksManager::<T>::default())),
            height0: AtomicI64::new(-1),
            height1: AtomicI64::new(-1),
            cache0: AtomPtr::new(Arc::new(EntryCache::new_uninit())),
            cache1: AtomPtr::new(Arc::new(EntryCache::new_uninit())),
        }
    }

    pub fn free_slot_count(&self) -> usize {
        let mut count = 0;
        if self.height0.load(Ordering::Acquire) < 0 {
            count += 1;
        }
        if self.height1.load(Ordering::Acquire) < 0 {
            count += 1;
        }
        count
    }

    pub fn end_block(&self, height: i64) {
        let height0 = self.height0.load(Ordering::Acquire);
        if height0 == height {
            self.height0.store(-1, Ordering::Release);
            return;
        }
        let height1 = self.height1.load(Ordering::Acquire);
        if height1 == height {
            self.height1.store(-1, Ordering::Release);
            return;
        }
        panic!("no data found for height");
    }

    pub fn start_block(
        &self,
        height: i64,
        tasks_in_blk: Arc<TasksManager<T>>,
        cache: Arc<EntryCache>,
    ) {
        let height0 = self.height0.load(Ordering::Acquire);
        if height0 < 0 {
            let old = self.tasks_in_blk0.swap(tasks_in_blk);
            drop(old);
            let old = self.cache0.swap(cache);
            drop(old);
            // Publish swapped pointers (tasks/cache) to readers.
            self.height0.store(height, Ordering::Release);
            return;
        }
        let height1 = self.height1.load(Ordering::Acquire);
        if height1 < 0 {
            let old = self.tasks_in_blk1.swap(tasks_in_blk);
            drop(old);
            let old = self.cache1.swap(cache);
            drop(old);
            // Publish swapped pointers (tasks/cache) to readers.
            self.height1.store(height, Ordering::Release);
            return;
        }
        panic!("no data found for height");
    }
}

impl<T: Task> TaskHub for BlockPairTaskHub<T> {
    fn get_entry_cache(&self, height: i64) -> Arc<EntryCache> {
        let height0 = self.height0.load(Ordering::Acquire);
        if height0 == height {
            return Arc::clone(&self.cache0.get_ref());
        }
        let height1 = self.height1.load(Ordering::Acquire);
        if height1 == height {
            return Arc::clone(&self.cache1.get_ref());
        }
        panic!("no data found for height {height}");
    }

    fn get_change_sets(&self, task_id: i64) -> Arc<Vec<ChangeSet>> {
        let height0 = self.height0.load(Ordering::Acquire);
        let (height, idx) = split_task_id(task_id);
        if height0 == height {
            return self
                .tasks_in_blk0
                .get_ref()
                .as_ref()
                .get_tasks_change_sets(idx);
        }
        let height1 = self.height1.load(Ordering::Acquire);
        if height1 == height {
            return self
                .tasks_in_blk1
                .get_ref()
                .as_ref()
                .get_tasks_change_sets(idx);
        }
        panic!("no data found for height {height}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entryfile::EntryCache;
    use crate::test_helper::SimpleTask;
    use parking_lot::RwLock;
    use qmdb_common::task::taskid::join_task_id;
    use std::sync::Arc;

    #[test]
    fn test_initialize() {
        let hub: BlockPairTaskHub<SimpleTask> = BlockPairTaskHub::new();
        assert_eq!(hub.free_slot_count(), 2);
    }

    #[test]
    fn test_start_end_block() {
        let hub: BlockPairTaskHub<SimpleTask> = BlockPairTaskHub::new();
        let tasks_in_blk = Arc::new(TasksManager::default());
        let cache = Arc::new(EntryCache::new_uninit());

        hub.start_block(1, tasks_in_blk.clone(), cache.clone());
        assert_eq!(hub.free_slot_count(), 1);
        assert_eq!(hub.height0.load(Ordering::SeqCst), 1);
        assert_eq!(hub.height1.load(Ordering::SeqCst), -1);

        hub.start_block(2, tasks_in_blk.clone(), cache.clone());

        hub.end_block(1);
        assert_eq!(hub.free_slot_count(), 1);

        assert!(std::panic::catch_unwind(move || {
            hub.end_block(1);
        })
        .is_err());
    }

    #[test]
    fn test_get_change_sets() {
        let hub: BlockPairTaskHub<SimpleTask> = BlockPairTaskHub::new();
        let changeset = ChangeSet::new();
        let tasks_in_blk = vec![RwLock::new(Some(SimpleTask::new(vec![changeset])))];
        let tasks_manager = Arc::new(TasksManager::new(tasks_in_blk));
        let cache = Arc::new(EntryCache::new_uninit());

        hub.start_block(1, tasks_manager, cache.clone());

        let change_sets = hub.get_change_sets(join_task_id(1, 0, false));
        assert_eq!(change_sets.len(), 1);

        hub.end_block(1);

        assert!(std::panic::catch_unwind(move || {
            hub.get_change_sets(0);
        })
        .is_err());
    }
}
