use crate::def::{
    is_compactible, DEFAULT_ENTRY_SIZE, IN_BLOCK_IDX_BITS, IN_BLOCK_IDX_MASK, OP_CREATE, OP_DELETE,
    OP_WRITE,
};
use crate::entryfile::{Entry, EntryBz, EntryCache, EntryFile};
use crate::indexer::Indexer;
use crate::utils::changeset::ChangeSet;
use crate::utils::{hasher, OpRecord};
use std::collections::HashMap;
use std::sync::Arc;

pub struct UpdateBuffer {
    pub entry_map: HashMap<i64, Vec<u8>>,
    pub start: i64,
    pub end: i64,
}

impl UpdateBuffer {
    pub fn new(start: i64) -> Self {
        Self {
            entry_map: Default::default(),
            start,
            end: start,
        }
    }

    pub fn reset(&mut self) {
        self.entry_map = Default::default();
        self.start = self.end;
    }

    pub fn get_entry_bz_at<F>(&mut self, file_pos: i64, mut access: F) -> bool
    where
        F: FnMut(EntryBz),
    {
        if file_pos < self.start {
            return true; // in disk
        }
        if file_pos > self.end {
            panic!("file_pose exceed self.end")
        }
        let entry_bz = EntryBz {
            bz: self.entry_map.get(&file_pos).unwrap(),
        };
        access(entry_bz);
        false
    }

    pub fn append(&mut self, entry: &Entry, deactived_serial_num_list: &[u64]) -> i64 {
        let size = entry.get_serialized_len(deactived_serial_num_list.len());
        let mut e = vec![0; size];
        let pos = self.end;
        entry.dump(&mut e, deactived_serial_num_list);
        self.entry_map.insert(pos, e);
        self.end += size as i64;
        pos
    }

    pub fn get_all_entry_bz(&self) -> Vec<EntryBz> {
        let mut res = vec![];
        let mut keys = self
            .entry_map
            .iter()
            .map(|(&key, _)| key)
            .collect::<Vec<_>>();
        keys.sort();
        for key in keys {
            let entry_bz = EntryBz {
                bz: self.entry_map.get(&key).unwrap(),
            };
            res.push(entry_bz);
        }
        res
    }
}

pub struct EntryUpdater {
    pub shard_id: usize,
    pub indexer: Arc<Indexer>,

    cache: Arc<EntryCache>,
    pub entry_file: Arc<EntryFile>,
    read_entry_buf: Vec<u8>,
    pub curr_version: i64,

    pub update_buffer: UpdateBuffer,
    pub sn_end: u64,
    pub sn_start: u64,
    pub compact_done_pos: i64,
    pub compact_start: i64,
    pub compact_thres: i64,
    pub utilization_div: i64,
    pub utilization_ratio: i64,
}

impl EntryUpdater {
    pub fn new(
        shard_id: usize,
        start_pos: i64,
        cache: Arc<EntryCache>,
        entry_file: Arc<EntryFile>,
        indexer: Arc<Indexer>,
        curr_version: i64,
        sn_end: u64,
        compact_start: i64,
        utilization_div: i64,
        utilization_ratio: i64,
        compact_thres: i64,
        sn_start: u64,
    ) -> Self {
        Self {
            shard_id,
            cache,
            entry_file,
            indexer,
            read_entry_buf: Vec::with_capacity(DEFAULT_ENTRY_SIZE),
            curr_version,
            sn_end,
            sn_start,
            compact_done_pos: compact_start,
            compact_start,
            utilization_div,
            utilization_ratio,
            compact_thres,
            update_buffer: UpdateBuffer::new(start_pos),
        }
    }
    pub fn run_task(&mut self, task_id: i64, change_sets: &Vec<ChangeSet>) {
        if (task_id & IN_BLOCK_IDX_MASK) == 0 {
            //task_index ==0 and new block start
            self.curr_version = task_id;
        }
        for change_set in change_sets {
            change_set.run_in_shard(self.shard_id, |op, key_hash, k, v, r| match op {
                OP_WRITE => self.write_kv(key_hash, k, v, r),
                OP_CREATE => self.create_kv(key_hash, k, v, r),
                OP_DELETE => self.delete_kv(key_hash, k, r),
                _ => {}
            });
            self.curr_version += 1;
        }
    }

    fn write_kv(
        &mut self,
        key_hash: &[u8; 32],
        key: &[u8],
        value: &[u8],
        _r: Option<&Box<OpRecord>>,
    ) {
        let height = self.curr_version >> IN_BLOCK_IDX_BITS;
        let mut old_pos = -1;
        let indexer = self.indexer.clone();
        indexer.for_each_value(height, key_hash, |file_pos| -> bool {
            self.read_entry(self.shard_id, file_pos);
            let old_entry = EntryBz {
                bz: &self.read_entry_buf[..],
            };
            if old_entry.key() == key {
                old_pos = file_pos;
            }
            old_pos >= 0
        });
        if old_pos < 0 {
            panic!("Write to non-exist key");
        }
        let old_entry = EntryBz {
            bz: &self.read_entry_buf[..],
        };
        let new_entry = Entry {
            key,
            value,
            next_key_hash: old_entry.next_key_hash(),
            version: self.curr_version,
            serial_number: self.sn_end,
        };
        self.sn_end += 1;
        let dsn_list: [u64; 1] = [old_entry.serial_number()];
        let new_pos = self.update_buffer.append(&new_entry, &dsn_list[..]);
        self.indexer.change_kv(key_hash, old_pos, new_pos, 0, 0);
        self.try_compact();
    }

    fn delete_kv(&mut self, key_hash: &[u8; 32], key: &[u8], _r: Option<&Box<OpRecord>>) {
        let height = self.curr_version >> IN_BLOCK_IDX_BITS;
        let mut del_entry_pos = -1;
        let mut del_entry_sn = 0;
        let mut old_next_key_hash = [0u8; 32];
        let indexer = self.indexer.clone();
        indexer.for_each_value(height, key_hash, |file_pos| -> bool {
            self.read_entry(self.shard_id, file_pos);
            let entry_bz = EntryBz {
                bz: &self.read_entry_buf[..],
            };
            if entry_bz.key() == key {
                del_entry_pos = file_pos;
                del_entry_sn = entry_bz.serial_number();
                old_next_key_hash.copy_from_slice(entry_bz.next_key_hash());
            }
            del_entry_pos >= 0 // break if del_entry_pos was assigned
        });
        if del_entry_pos < 0 {
            panic!("Delete non-exist key");
        }
        self.indexer.erase_kv(key_hash, del_entry_pos, 0);

        let mut prev_k80 = [0; 10];
        let mut old_pos = -1;
        indexer.for_each_adjacent_value(height, key_hash, |k80, file_pos| -> bool {
            if file_pos == del_entry_pos {
                return false; // skip myself entry, continue loop
            }
            self.read_entry(self.shard_id, file_pos);
            let prev_entry = EntryBz {
                bz: &self.read_entry_buf[..],
            };
            if prev_entry.next_key_hash() == key_hash {
                prev_k80.copy_from_slice(k80);
                old_pos = file_pos;
            }
            old_pos >= 0 // exit loop if old_pos was assigned
        });
        if old_pos < 0 {
            panic!("Cannot find prevEntry");
        }
        let prev_entry = EntryBz {
            bz: &self.read_entry_buf[..],
        };
        let prev_changed = Entry {
            key: prev_entry.key(),
            value: prev_entry.value(),
            next_key_hash: &old_next_key_hash[..],
            version: self.curr_version,
            serial_number: self.sn_end,
        };
        self.sn_end += 1;
        let deactived_sn_list: [u64; 2] = [del_entry_sn, prev_entry.serial_number()];
        let new_pos = self
            .update_buffer
            .append(&prev_changed, &deactived_sn_list[..]);
        self.indexer.change_kv(&prev_k80, old_pos, new_pos, 0, 0);
    }

    fn create_kv(
        &mut self,
        key_hash: &[u8; 32],
        key: &[u8],
        value: &[u8],
        _r: Option<&Box<OpRecord>>,
    ) {
        let height = self.curr_version >> IN_BLOCK_IDX_BITS;
        let mut old_pos = -1;
        let mut prev_k80 = [0; 10];
        let indexer = self.indexer.clone();
        indexer.for_each_adjacent_value(height, key_hash, |k80, file_pos| -> bool {
            self.read_entry(self.shard_id, file_pos);
            let prev_entry = EntryBz {
                bz: &self.read_entry_buf[..],
            };
            if prev_entry.key_hash() < *key_hash && &key_hash[..] < prev_entry.next_key_hash() {
                prev_k80.copy_from_slice(k80);
                old_pos = file_pos;
            }
            old_pos >= 0
        });
        if old_pos < 0 {
            panic!(
                "Write to non-exist key shard_id={} key={:?}",
                self.shard_id, key
            );
        };
        let prev_entry = EntryBz {
            bz: &self.read_entry_buf[..],
        };
        let new_entry = Entry {
            key,
            value,
            next_key_hash: prev_entry.next_key_hash(),
            version: self.curr_version,
            serial_number: self.sn_end,
        };
        self.sn_end += 1;
        let create_pos = self.update_buffer.append(&new_entry, &[]);
        let hash = hasher::hash(key);
        let prev_changed = Entry {
            key: prev_entry.key(),
            value: prev_entry.value(),
            next_key_hash: &hash[..],
            version: self.curr_version,
            serial_number: self.sn_end,
        };
        self.sn_end += 1;
        let deactivated_sn_list: [u64; 1] = [prev_entry.serial_number()];
        let new_pos = self
            .update_buffer
            .append(&prev_changed, &deactivated_sn_list[..]);
        self.indexer.add_kv(key_hash, create_pos, 0);
        self.indexer.change_kv(&prev_k80, old_pos, new_pos, 0, 0);
        self.try_compact();
        self.try_compact();
    }

    fn try_compact(&mut self) {
        if !is_compactible(
            self.utilization_div,
            self.utilization_ratio,
            self.compact_thres,
            self.indexer.len(self.shard_id),
            self.sn_start,
            self.sn_end,
        ) {
            return;
        }

        let mut bz: Vec<u8> = vec![0; DEFAULT_ENTRY_SIZE];
        loop {
            bz.resize(DEFAULT_ENTRY_SIZE, 0);
            let size = self.entry_file.read_entry(self.compact_start, &mut bz[..]);
            if bz.len() < size {
                bz.resize(size, 0);
                self.entry_file.read_entry(self.compact_start, &mut bz[..]);
            }
            let old_entry = EntryBz { bz: &bz[..size] };
            if self
                .indexer
                .key_exists(&old_entry.key_hash(), self.compact_start, 0)
            {
                let new_entry = Entry {
                    key: old_entry.key(),
                    value: old_entry.value(),
                    next_key_hash: old_entry.next_key_hash(),
                    version: old_entry.version(),
                    serial_number: self.sn_end,
                };
                self.sn_end += 1;
                let new_pos = self
                    .update_buffer
                    .append(&new_entry, &[old_entry.serial_number()]);
                self.indexer
                    .change_kv(&old_entry.key_hash(), self.compact_start, new_pos, 0, 0);
                self.sn_start = old_entry.serial_number() + 1;
                self.compact_done_pos = self.compact_start + size as i64;
                self.compact_start += size as i64;
                break;
            }
            self.compact_start += size as i64;
        }
    }

    pub fn get_all_entry_bz(&self) -> Vec<EntryBz> {
        self.update_buffer.get_all_entry_bz()
    }

    pub fn read_entry(&mut self, shard_id: usize, file_pos: i64) {
        let cache_hit = self.cache.lookup(shard_id, file_pos, |entry_bz| {
            self.read_entry_buf.resize(0, 0);
            self.read_entry_buf.extend_from_slice(entry_bz.bz);
        });
        if cache_hit {
            return;
        }
        let in_disk = self.update_buffer.get_entry_bz_at(file_pos, |entry_bz| {
            self.read_entry_buf.resize(0, 0);
            self.read_entry_buf.extend_from_slice(entry_bz.bz);
            self.cache.insert(shard_id, file_pos, &entry_bz);
        });
        if !in_disk {
            return;
        }
        self.read_entry_buf.resize(DEFAULT_ENTRY_SIZE, 0);
        let ef = &self.entry_file;
        let size = ef.read_entry(file_pos, &mut self.read_entry_buf[..]);
        self.read_entry_buf.resize(size, 0);
        if self.read_entry_buf.len() < size {
            ef.read_entry(file_pos, &mut self.read_entry_buf[..]);
        }
    }
}
