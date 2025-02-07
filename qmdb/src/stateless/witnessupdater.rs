use std::ops::Bound::{Excluded, Included};
use std::{collections::BTreeMap, ops::Bound};

use crate::def::{is_compactible, IN_BLOCK_IDX_MASK, SHARD_COUNT};
use crate::utils::hasher::Hash32;
use crate::{
    def::{OP_CREATE, OP_DELETE, OP_READ, OP_WRITE},
    entryfile::{Entry, EntryBz, EntryVec},
    utils::changeset::ChangeSet,
};

use super::witness::Witness;

pub struct WitnessListUpdater {
    witness_list: [Witness; SHARD_COUNT],
    kh2e_map: BTreeMap<Hash32, usize>,
    sn2e_map_list: [BTreeMap<u64, usize>; SHARD_COUNT],
    pub entry_vec: EntryVec,
    curr_version: i64,
    next_sn_list: Vec<u64>,
    oldest_active_sn_list: Vec<u64>,
    num_active_list: Vec<u64>,
    utilization_div: i64,
    utilization_ratio: i64,
    compact_thres: i64,
    temp_kv: Option<(Vec<u8>, Vec<u8>)>,
}

impl WitnessListUpdater {
    pub fn new(
        witness_list: [Witness; SHARD_COUNT],
        entry_vec: EntryVec,
        next_sn_list: Vec<u64>,
        oldest_active_sn_list: Vec<u64>,
        num_active_list: Vec<u64>,
        utilization_div: i64,
        utilization_ratio: i64,
        compact_thres: i64,
    ) -> Self {
        let mut kh2e_map = BTreeMap::new();
        let mut sn2e_map_list = [(); SHARD_COUNT].map(|_| BTreeMap::new());
        for shard_id in 0..SHARD_COUNT {
            for (idx, e) in entry_vec.enumerate(shard_id) {
                let key_hash = e.key_hash();
                kh2e_map.insert(key_hash, idx);
                sn2e_map_list[shard_id].insert(e.serial_number(), idx);
            }
        }

        Self {
            witness_list,
            kh2e_map,
            sn2e_map_list,
            entry_vec,
            curr_version: 0,
            next_sn_list,
            oldest_active_sn_list,
            num_active_list,
            utilization_div,
            utilization_ratio,
            compact_thres,
            temp_kv: Some((Vec::new(), Vec::new())),
        }
    }

    pub fn get_next_sn_list(&self) -> &[u64] {
        &self.next_sn_list[..]
    }

    pub fn get_oldest_active_sn_list(&self) -> &[u64] {
        &self.oldest_active_sn_list[..]
    }

    pub fn get_num_active_list(&self) -> &[u64] {
        &self.num_active_list[..]
    }

    pub fn apply_change(&mut self, change_set: &ChangeSet, task_id: i64) -> bool {
        if (task_id & IN_BLOCK_IDX_MASK) == 0 {
            self.curr_version = task_id;
        }
        for shard_id in 0..SHARD_COUNT {
            if !self._apply_change(shard_id, change_set) {
                return false;
            }
        }
        true
    }

    fn _apply_change(&mut self, shard_id: usize, change_set: &ChangeSet) -> bool {
        let mut ok = true;
        change_set.run_in_shard(shard_id, |op, key_hash, k, v, _r| {
            ok = ok
                && match op {
                    OP_WRITE => self.write_kv(shard_id, key_hash, k, v),
                    OP_CREATE => self.create_kv(shard_id, key_hash, k, v),
                    OP_DELETE => self.delete_kv(shard_id, key_hash),
                    OP_READ => true,
                    _ => {
                        panic!("WitnessUpdater: unsupported operation");
                    }
                }
        });
        ok
    }

    fn write_kv(&mut self, shard_id: usize, kh: &[u8; 32], key: &[u8], value: &[u8]) -> bool {
        let old_entry = self.get_entry(shard_id, kh);
        if old_entry.is_none() {
            return false;
        }
        let mut next_kh = [0u8; 32];
        let mut dsn_list = [0u64; 1];
        let old_entry = old_entry.unwrap();
        dsn_list[0] = old_entry.serial_number();
        next_kh.copy_from_slice(old_entry.next_key_hash());
        let new_entry = Entry {
            key,
            value,
            next_key_hash: &next_kh[..],
            version: self.curr_version,
            serial_number: self.next_sn_list[shard_id],
        };
        if !self.write_entry(shard_id, kh, &new_entry, &dsn_list) {
            return false;
        }
        self.next_sn_list[shard_id] += 1;
        if self._is_compactible(shard_id) && !self.compact(shard_id) {
            return false;
        }
        true
    }

    // avoid compiler error
    fn set_temp_kv(temp_kv: &mut (Vec<u8>, Vec<u8>), k: &[u8], v: &[u8]) {
        temp_kv.0.resize(k.len(), 0);
        temp_kv.0[..k.len()].copy_from_slice(k);
        temp_kv.1.resize(v.len(), 0);
        temp_kv.1[..v.len()].copy_from_slice(v);
    }

    fn create_kv(&mut self, shard_id: usize, kh: &[u8; 32], key: &[u8], value: &[u8]) -> bool {
        let mut temp_kv = self.temp_kv.take().unwrap();
        let prev_entry = self.get_prev_entry(shard_id, kh);
        if prev_entry.is_none() {
            return false;
        }
        let (prev_kh, prev_entry) = prev_entry.unwrap();
        let mut next_kh = [0u8; 32];
        let mut dsn_list = [0u64; 1];
        next_kh.copy_from_slice(prev_entry.next_key_hash());
        dsn_list[0] = prev_entry.serial_number();
        let new_entry = Entry {
            key,
            value,
            next_key_hash: &next_kh[..],
            version: self.curr_version,
            serial_number: self.next_sn_list[shard_id],
        };
        Self::set_temp_kv(&mut temp_kv, prev_entry.key(), prev_entry.value());
        let prev_changed = Entry {
            key: &temp_kv.0[..],
            value: &temp_kv.1[..],
            next_key_hash: kh,
            version: self.curr_version,
            serial_number: self.next_sn_list[shard_id] + 1,
        };
        if !self.write_entry(shard_id, kh, &new_entry, &[]) {
            return false;
        }
        if !self.write_entry(shard_id, &prev_kh, &prev_changed, &dsn_list[..]) {
            return false;
        }
        self.next_sn_list[shard_id] += 2;
        self.num_active_list[shard_id] += 1;
        self.temp_kv = Some(temp_kv);
        if self._is_compactible(shard_id) {
            if !self.compact(shard_id) {
                return false;
            }
            if !self.compact(shard_id) {
                return false;
            }
        }
        true
    }

    fn delete_kv(&mut self, shard_id: usize, kh: &[u8; 32]) -> bool {
        let mut temp_kv = self.temp_kv.take().unwrap();
        let mut dsn_list = [0u64; 2];
        let curr_entry = self.get_entry(shard_id, kh);
        if curr_entry.is_none() {
            return false;
        }
        let curr_entry = curr_entry.unwrap();
        dsn_list[0] = curr_entry.serial_number();
        let mut next_kh = [0u8; 32];
        next_kh.copy_from_slice(curr_entry.next_key_hash());
        self.remove_entry(kh);

        let prev_entry = self.get_prev_entry(shard_id, kh);
        if prev_entry.is_none() {
            return false;
        }
        let (prev_kh, prev_entry) = prev_entry.unwrap();
        Self::set_temp_kv(&mut temp_kv, prev_entry.key(), prev_entry.value());

        dsn_list[1] = prev_entry.serial_number();
        let prev_changed = Entry {
            key: &temp_kv.0[..],
            value: &temp_kv.1[..],
            next_key_hash: &next_kh[..],
            version: self.curr_version,
            serial_number: self.next_sn_list[shard_id],
        };
        if !self.write_entry(shard_id, &prev_kh, &prev_changed, &dsn_list[..]) {
            return false;
        }
        self.next_sn_list[shard_id] += 1;
        self.num_active_list[shard_id] -= 1;
        self.temp_kv = Some(temp_kv);
        true
    }

    fn compact(&mut self, shard_id: usize) -> bool {
        let mut temp_kv = self.temp_kv.take().unwrap();
        let mut dsn_list = [0u64; 1];

        let old_entry = self.get_oldest_active(shard_id);
        if old_entry.is_none() {
            return false;
        }
        let old_entry = old_entry.unwrap();
        Self::set_temp_kv(&mut temp_kv, old_entry.key(), old_entry.value());
        let mut next_kh = [0u8; 32];
        next_kh.copy_from_slice(old_entry.next_key_hash());
        dsn_list[0] = old_entry.serial_number();
        let kh = old_entry.key_hash();
        let new_entry = Entry {
            key: &temp_kv.0[..],
            value: &temp_kv.1[..],
            next_key_hash: &next_kh[..],
            version: old_entry.version(),
            serial_number: self.next_sn_list[shard_id],
        };
        self.oldest_active_sn_list[shard_id] = old_entry.serial_number() + 1;
        self.write_entry(shard_id, &kh, &new_entry, &dsn_list[..]);
        self.next_sn_list[shard_id] += 1;
        self.temp_kv = Some(temp_kv);
        true
    }

    fn _is_compactible(&self, shard_id: usize) -> bool {
        let active_count = self.num_active_list[shard_id] as usize;

        is_compactible(
            self.utilization_div,
            self.utilization_ratio,
            self.compact_thres,
            active_count,
            self.oldest_active_sn_list[shard_id],
            self.next_sn_list[shard_id],
        )
    }

    fn get_oldest_active(&self, shard_id: usize) -> Option<EntryBz> {
        let sn = self.oldest_active_sn_list[shard_id];
        if let Some(next_sn) = self.witness_list[shard_id].find_next_active_sn(sn) {
            self.get_entry_by_sn(shard_id, next_sn)
        } else {
            None
        }
    }

    pub fn get_entry(&self, shard_id: usize, kh: &Hash32) -> Option<EntryBz> {
        if let Some(idx) = self.kh2e_map.get(kh) {
            return Some(self.entry_vec.get_entry(shard_id, *idx));
        }
        None
    }

    fn get_entry_by_sn(&self, shard_id: usize, sn: u64) -> Option<EntryBz> {
        if let Some(idx) = self.sn2e_map_list[shard_id].get(&sn) {
            return Some(self.entry_vec.get_entry(shard_id, *idx));
        }
        None
    }

    pub fn get_prev_entry(&self, shard_id: usize, kh: &Hash32) -> Option<(Hash32, EntryBz)> {
        let range = (Included(&[0u8; 32]), Excluded(kh));
        let last_opt = self
            .kh2e_map
            .range::<Hash32, (Bound<&Hash32>, Bound<&Hash32>)>(range)
            .next_back();
        if let Some((kh, idx)) = last_opt {
            return Some((*kh, self.entry_vec.get_entry(shard_id, *idx)));
        }
        None
    }

    fn write_entry(&mut self, shard_id: usize, kh: &[u8; 32], e: &Entry, dsn_list: &[u64]) -> bool {
        let idx = self.entry_vec.add_entry(kh, e, dsn_list);
        let entry_bz = self.entry_vec.get_entry(shard_id, idx);
        self.kh2e_map.insert(*kh, idx);
        self.sn2e_map_list[shard_id].insert(entry_bz.serial_number(), idx);
        for sn in dsn_list.iter() {
            self.sn2e_map_list[shard_id].remove(sn);
        }
        self.witness_list[shard_id].add_entry(&entry_bz)
    }

    fn remove_entry(&mut self, kh: &[u8; 32]) {
        self.kh2e_map.remove(kh);
    }

    pub fn verify_non_inclusion_entry_list(&self, key_hash_list_arr: &Vec<Vec<Hash32>>) -> bool {
        for shard_id in 0..SHARD_COUNT {
            for key_hash in &key_hash_list_arr[shard_id] {
                if let Some((_, prev_entry)) = self.get_prev_entry(shard_id, key_hash) {
                    if !(&prev_entry.key_hash() < key_hash && prev_entry.next_key_hash() > key_hash)
                    {
                        return false;
                    }
                } else {
                    return false;
                }
            }
        }

        true
    }

    pub fn verify(
        &self,
        merkle_nodes_list: &Vec<Vec<u8>>,
        old_root_list: &[Hash32],
        new_root_list: &[Hash32],
    ) -> bool {
        for shard_id in 0..SHARD_COUNT {
            if !self.witness_list[shard_id].verify_witness(
                &merkle_nodes_list[shard_id],
                &old_root_list[shard_id],
                &new_root_list[shard_id],
            ) {
                return false;
            }
        }
        true
    }
}
