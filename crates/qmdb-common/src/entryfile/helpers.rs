use alloc::vec;
use alloc::vec::Vec;

use crate::entryfile::entry::Entry;

pub struct EntryBuilder {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub next_key_hash: [u8; 32],
    pub version: i64,
    pub serial_number: u64,
}

impl EntryBuilder {
    pub fn kv<T: AsRef<[u8]>>(k: T, v: T) -> Self {
        EntryBuilder {
            key: Vec::from(k.as_ref()),
            value: Vec::from(v.as_ref()),
            next_key_hash: [0; 32],
            version: 0,
            serial_number: 0,
        }
    }

    pub fn ver(&mut self, v: i64) -> &Self {
        self.version = v;
        self
    }
    pub fn sn(&mut self, v: u64) -> &Self {
        self.serial_number = v;
        self
    }
    pub fn next_kh(&mut self, kh: [u8; 32]) -> &Self {
        self.next_key_hash = kh;
        self
    }

    pub fn build(&self) -> Entry {
        Entry {
            key: &self.key[..],
            value: &self.value[..],
            next_key_hash: &self.next_key_hash[..],
            version: self.version,
            serial_number: self.serial_number,
        }
    }

    pub fn build_and_dump(&self, dsn_list: &[u64]) -> Vec<u8> {
        let entry = self.build();
        let size = entry.get_serialized_len(dsn_list.len());
        let mut bz = vec![0; size];
        entry.dump(&mut bz, dsn_list);
        bz
    }
}
