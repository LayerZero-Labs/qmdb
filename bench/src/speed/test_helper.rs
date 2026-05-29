use super::def::{L, PAGE_SIZE};
use byteorder::{BigEndian, ByteOrder};
use parking_lot::RwLock;
use qmdb::{indexer::Indexer, test_helper::SimpleTask};
use qmdb_common::def::{OP_WRITE, SHARD_COUNT};
use qmdb_common::indexer::indexer_trait::IndexerTrait;
use qmdb_common::task::task::Task;
use std::sync::Arc;
use std::{fs::File, os::unix::fs::FileExt};

#[allow(dead_code)]
fn fake_indexer(file: &Arc<File>, page_count: u64, task_list: &[RwLock<Option<SimpleTask>>]) {
    rayon::scope(|s| {
        for item in task_list.iter() {
            let task_opt = item.read();
            let task = task_opt.as_ref().unwrap();
            for change_set in task.get_change_sets().iter() {
                for shard_id in 0..SHARD_COUNT {
                    change_set.run_in_shard(shard_id, |op, key_hash: &[u8; 32], _k, _v, _r| {
                        if op != OP_WRITE {
                            return;
                        }
                        let x = BigEndian::read_u64(&key_hash[..8]) % page_count;
                        let f = file.clone();
                        s.spawn(move |_| {
                            let mut page = [0u8; L * PAGE_SIZE];
                            let page_off = x * (page.len() as u64);
                            f.read_at(&mut page[..], page_off).unwrap();
                        });
                    });
                }
            }
        }
    });
}

#[allow(dead_code)]
fn warmup_indexer(indexer: &Arc<Indexer>, height: i64, task_list: &[RwLock<Option<SimpleTask>>]) {
    rayon::scope(|s| {
        for item in task_list.iter() {
            let task_opt = item.read();
            let task = task_opt.as_ref().unwrap();
            for change_set in task.get_change_sets().iter() {
                for shard_id in 0..SHARD_COUNT {
                    change_set.run_in_shard(shard_id, |op, key_hash: &[u8; 32], _k, _v, _r| {
                        let kh = *key_hash;
                        let idx = indexer.clone();
                        s.spawn(move |_| {
                            let _ = idx.for_each(height, op, &kh[..]);
                        });
                    });
                }
            }
        }
    });
}
