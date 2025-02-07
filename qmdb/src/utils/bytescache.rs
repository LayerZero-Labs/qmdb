use crate::def::{BIG_BUF_SIZE, BYTES_CACHE_SHARD_COUNT};
use crate::utils::new_big_buf_boxed;
use crate::utils::BigBuf;
use parking_lot::RwLock;
use std::collections::hash_map;
use std::collections::HashMap;
use std::hash::Hash;

pub fn new_cache_pos() -> u64 {
    u64::MAX
}
pub fn make_cache_pos(idx: u32, offset: u32) -> u64 {
    ((idx as u64) << 32) | (offset as u64)
}
pub fn split_cache_pos(idx_and_offset: u64) -> (usize, usize) {
    let idx = (idx_and_offset >> 32) as usize;
    let offset = ((idx_and_offset << 32) >> 32) as usize;
    (idx, offset)
}

struct BytesCacheShard<KT: Hash + Eq + Clone> {
    pos_map: HashMap<KT, u64>,
    buf_list: Vec<Box<BigBuf>>,
    free_list: Vec<Box<BigBuf>>,
    curr_buf_idx: u32,
    curr_offset: u32,
}

impl<KT: Hash + Eq + Clone> BytesCacheShard<KT> {
    pub fn new() -> Self {
        Self {
            pos_map: HashMap::new(),
            buf_list: Vec::new(),
            free_list: Vec::new(),
            curr_buf_idx: 0,
            curr_offset: 0,
        }
    }

    pub fn clear(&mut self) {
        self.pos_map.clear();
        self.free_list.append(&mut self.buf_list);
        self.free_list.truncate(self.free_list.len() * 7 / 8); //keep 7/8 bufs
        self.curr_buf_idx = 0;
        self.curr_offset = 0;
    }

    fn allocate_big_buf(&mut self) -> Box<BigBuf> {
        if self.free_list.is_empty() {
            return new_big_buf_boxed();
        }
        self.free_list.pop().unwrap()
    }

    pub fn insert(&mut self, cache_key: &KT, cache_pos: u64) {
        self.pos_map.insert(cache_key.clone(), cache_pos);
    }

    pub fn allocate_if_missing(&mut self, cache_key: &KT, cache_pos: u64) -> bool {
        match self.pos_map.entry(cache_key.clone()) {
            hash_map::Entry::Occupied(_) => false,
            hash_map::Entry::Vacant(v) => {
                v.insert(cache_pos);
                true
            }
        }
    }

    pub fn fill(&mut self, bz: &[u8]) -> (u32, u32) {
        let size = bz.len();
        if self.buf_list.is_empty() {
            let new_buf = self.allocate_big_buf();
            self.buf_list.push(new_buf);
        }
        if self.curr_offset as usize + size > BIG_BUF_SIZE {
            let new_buf = self.allocate_big_buf();
            self.buf_list.push(new_buf);
            self.curr_buf_idx = self.buf_list.len() as u32 - 1;
            self.curr_offset = 0;
        }
        let buf = &mut self.buf_list[self.curr_buf_idx as usize];
        let target = &mut buf[self.curr_offset as usize..];
        target[..size].copy_from_slice(&bz[..size]);
        let buf_idx = self.curr_buf_idx;
        let offset = self.curr_offset;
        self.curr_offset += size as u32;
        (buf_idx, offset)
    }

    fn _contains(&self, cache_key: &KT) -> bool {
        if let Some(cache_pos) = self.pos_map.get(cache_key) {
            if *cache_pos == u64::MAX {
                return false; //cache miss
            }
            return true; //cache hit
        }
        false //cache miss
    }

    fn _read_bytes_at<F>(&self, cache_key: &KT, mut access: F) -> bool
    where
        F: FnMut(&[u8]),
    {
        if let Some(cache_pos) = self.pos_map.get(cache_key) {
            if *cache_pos == u64::MAX {
                return false; //cache miss
            }
            let (buf_idx, offset) = split_cache_pos(*cache_pos);
            access(&self.buf_list[buf_idx][offset..]);
            return true; //cache hit
        }
        false //cache miss
    }
}

pub struct BytesCache<KT: Hash + Eq + Clone> {
    shards: Vec<RwLock<BytesCacheShard<KT>>>,
}

impl<KT: Hash + Eq + Clone> Default for BytesCache<KT> {
    fn default() -> Self {
        Self::new()
    }
}

impl<KT: Hash + Eq + Clone> BytesCache<KT> {
    pub fn new() -> Self {
        let mut shards = Vec::with_capacity(BYTES_CACHE_SHARD_COUNT);
        for _ in 0..BYTES_CACHE_SHARD_COUNT {
            shards.push(RwLock::new(BytesCacheShard::<KT>::new()));
        }
        Self { shards }
    }

    pub fn clear(&self) {
        for idx in 0..BYTES_CACHE_SHARD_COUNT {
            let mut shard = self.shards[idx].write();
            shard.clear();
        }
    }

    pub fn insert(&self, cache_key: &KT, idx: usize, bz: &[u8]) {
        let mut shard = self.shards[idx].write();
        let (idx, offset) = shard.fill(bz);
        let pos = make_cache_pos(idx, offset);
        shard.insert(cache_key, pos);
    }

    pub fn insert_if_missing(&self, cache_key: &KT, idx: usize, bz: &[u8]) {
        let mut shard = self.shards[idx].write();
        if shard._contains(cache_key) {
            return;
        }
        let (idx, offset) = shard.fill(bz);
        let pos = make_cache_pos(idx, offset);
        shard.insert(cache_key, pos);
    }

    pub fn allocate_if_missing(&self, cache_key: &KT, idx: usize, cache_pos: u64) -> bool {
        let mut shard = self.shards[idx].write();
        shard.allocate_if_missing(cache_key, cache_pos)
    }

    //pub fn from_idx_and_offset(idx: u32, offset: u32) -> CachePos {
    pub fn fill(&self, idx: usize, bz: &[u8]) -> (u32, u32) {
        let mut shard = self.shards[idx].write();
        shard.fill(bz)
    }

    pub fn lookup<F>(&self, cache_key: &KT, idx: usize, access: F) -> bool
    where
        F: FnMut(&[u8]),
    {
        let shard = self.shards[idx].read();
        shard._read_bytes_at(cache_key, access)
    }
}

#[cfg(test)]
mod cache_pos_test {
    use super::*;

    #[test]
    fn test_idx_and_offset() {
        let mut cache_pos = new_cache_pos();
        assert_eq!(u64::MAX, cache_pos);

        cache_pos = make_cache_pos(0x1234, 0x5678);
        assert_eq!(0x123400005678, cache_pos);

        assert_eq!((0x1234, 0x5678), split_cache_pos(0x123400005678));
    }
}

#[cfg(test)]
mod cache_shard_test {
    use super::*;

    #[test]
    fn test_fill() {
        let mut shard = BytesCacheShard::<i64>::new();

        let (buf_idx, offset) = shard.fill("ab".repeat(10000).as_bytes());
        assert_eq!(0, buf_idx);
        assert_eq!(0, offset);
        assert_eq!(0, shard.curr_buf_idx);
        assert_eq!(20000, shard.curr_offset);

        let (buf_idx, offset) = shard.fill("cde".repeat(10000).as_bytes());
        assert_eq!(0, buf_idx);
        assert_eq!(20000, offset);
        assert_eq!(0, shard.curr_buf_idx);
        assert_eq!(50000, shard.curr_offset);

        let (buf_idx, offset) = shard.fill("fg".repeat(20000).as_bytes());
        assert_eq!(1, buf_idx);
        assert_eq!(0, offset);
        assert_eq!(1, shard.curr_buf_idx);
        assert_eq!(40000, shard.curr_offset);
    }

    #[test]
    fn test_read() {
        let mut shard = BytesCacheShard::<i64>::new();
        shard.fill("123456".repeat(10000).as_bytes());
        shard.fill("abcdef".repeat(10000).as_bytes());

        let key0 = 123;
        let cache_pos0 = make_cache_pos(0, 1000);
        shard.allocate_if_missing(&key0, cache_pos0);

        let key1 = 456;
        let cache_pos1 = make_cache_pos(1, 1111);
        shard.allocate_if_missing(&key1, cache_pos1);

        let mut buf: [u8; 20] = [0; 20];

        shard._read_bytes_at(&key0, |data| {
            buf.copy_from_slice(&data[..20]);
        });
        assert_eq!("3536313233343536313233343536313233343536", hex::encode(buf));

        shard._read_bytes_at(&key1, |data| {
            buf.copy_from_slice(&data[..20]);
        });
        assert_eq!("6263646566616263646566616263646566616263", hex::encode(buf));
    }
}

#[cfg(test)]
mod cache_test {
    use super::*;

    #[test]
    fn test_cache() {
        let cache = BytesCache::<i64>::new();
        let (idx1, offset1) = cache.fill(11, "hahaha".as_bytes());
        let (idx2, offset2) = cache.fill(22, "wawawa".as_bytes());
        cache.allocate_if_missing(&111, 11, make_cache_pos(idx1, offset1));
        cache.allocate_if_missing(&222, 22, make_cache_pos(idx2, offset2));

        let mut buf: [u8; 6] = [0; 6];
        cache.lookup(&111, 11, |data| {
            buf.copy_from_slice(&data[..6]);
        });
        assert_eq!("686168616861", hex::encode(buf));
    }
}
