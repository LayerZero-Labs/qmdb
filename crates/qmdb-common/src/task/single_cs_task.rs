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

    //     /// Creates a new EOA (Externally Owned Account)
    //     pub fn create_eoa(&mut self, address: Address, balance: U256, nonce: u64) -> &mut Self {
    //         let account_info = AccountInfo {
    //             balance,
    //             nonce,
    //             code_hash: KECCAK_EMPTY,
    //             code: Some(Bytecode::new()),
    //         };
    //         let encoded = encode_account_info(&account_info);
    //         self.task_builder.create(&address[..], encoded.as_slice());
    //         self
    //     }

    //     /// Creates a new Contract Account
    //     pub fn create_contract(
    //         &mut self,
    //         address: Address,
    //         balance: U256,
    //         nonce: u64,
    //         bytecode: Bytecode,
    //     ) -> &mut Self {
    //         let account_info = AccountInfo {
    //             balance,
    //             nonce,
    //             code_hash: bytecode_hash(bytecode),
    //             code: Some(bytecode),
    //         };
    //         let encoded = encode_account_info(&account_info);
    //         self.task_builder.create(&address[..], encoded.as_slice());
    //         self
    //     }

    //     /// Updates an existing account's state
    //     pub fn update_account(
    //         &mut self,
    //         address: Address,
    //         balance: U256,
    //         nonce: u64,
    //         code_hash: revm::primitives::B256,
    //         code: Option<Bytecode>,
    //     ) -> &mut Self {
    //         let account_info = AccountInfo {
    //             balance,
    //             nonce,
    //             code_hash,
    //             code,
    //         };
    //         let encoded = encode_account_info(&account_info);
    //         self.task_builder.write(&address[..], encoded.as_slice());
    //         self
    //     }

    //     /// Deletes an account
    //     pub fn delete_account(&mut self, address: Address) -> &mut Self {
    //         self.task_builder.delete(&address[..], &[]);
    //         self
    //     }

    //     /// Creates a new storage slot
    //     pub fn create_storage(
    //         &mut self,
    //         address: Address,
    //         slot: U256,
    //         value: U256,
    //         dust: u128,
    //     ) -> &mut Self {
    //         let addr_idx = join_address_index(&address, &slot);
    //         let mut encoded_value = value.to_be_bytes_vec();
    //         SlotEntryValue::encode(&mut encoded_value, dust);
    //         self.task_builder.create(&addr_idx[..], &encoded_value);
    //         self
    //     }

    //     /// Updates an existing storage slot
    //     pub fn update_storage(
    //         &mut self,
    //         address: Address,
    //         slot: U256,
    //         value: U256,
    //         dust: u128,
    //     ) -> &mut Self {
    //         let addr_idx = join_address_index(&address, &slot);
    //         let mut encoded_value = value.to_be_bytes_vec();
    //         SlotEntryValue::encode(&mut encoded_value, dust);
    //         self.task_builder.write(&addr_idx[..], &encoded_value);
    //         self
    //     }
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
