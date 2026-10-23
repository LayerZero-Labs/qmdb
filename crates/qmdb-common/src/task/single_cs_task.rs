use crate::{
    def::{OP_CREATE, OP_DELETE, OP_WRITE},
    task::task::Task,
    utils::{byte0_to_shard_id, changeset::ChangeSet},
};
use alloc::{boxed::Box, vec};
use alloc::{sync::Arc, vec::Vec};
use core::cell::RefCell;
use once_cell::race::OnceBox;

#[derive(Debug)]
pub struct SingleCsTask {
    change_sets: OnceBox<Arc<Vec<ChangeSet>>>,
    tmp_cs: RefCell<Option<ChangeSet>>,
}

unsafe impl Send for SingleCsTask {}
unsafe impl Sync for SingleCsTask {}

impl Task for SingleCsTask {
    fn get_change_sets(&self) -> Arc<Vec<ChangeSet>> {
        self.change_sets.get_or_init(|| {
            let mut cs = self.tmp_cs.borrow_mut().take().unwrap();
            cs.sort();
            let vec_cs = vec![cs];
            Box::new(Arc::new(vec_cs))
        });
        self.change_sets.get().unwrap().clone()
    }
}

impl SingleCsTask {
    pub fn new() -> Self {
        Self {
            change_sets: OnceBox::new(),
            tmp_cs: RefCell::new(Some(ChangeSet::new())),
        }
    }

    pub fn create(self, k: &[u8], v: &[u8]) -> Self {
        self.add_op(OP_CREATE, k, v)
    }

    pub fn write(self, k: &[u8], v: &[u8]) -> Self {
        self.add_op(OP_WRITE, k, v)
    }

    pub fn delete(self, k: &[u8], v: &[u8]) -> Self {
        self.add_op(OP_DELETE, k, v)
    }

    pub fn add_op(self, op: u8, k: &[u8], v: &[u8]) -> Self {
        let kh = crate::entryfile::entry::entry_key_hash(k);
        let _shard_id = byte0_to_shard_id(kh[0]) as u8;
        let mut cs = self.tmp_cs.borrow_mut();
        cs.as_mut().unwrap().add_op(op, &kh, k, v, Option::None);
        drop(cs);
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::task::{single_cs_task::SingleCsTask, task::Task};

    #[test]
    fn test_task_builder() {
        let task = SingleCsTask::new()
            .create(b"k1", b"v1")
            .create(b"k2", b"v2")
            .create(b"k3", b"v3");
        assert_eq!(task.get_change_sets().len(), 1);
        assert_eq!(task.get_change_sets()[0].op_list.len(), 3);
        assert!(task.tmp_cs.borrow().is_none());
    }
}
