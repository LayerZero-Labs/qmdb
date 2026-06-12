use parking_lot::RwLock;
/// A high-performance, sharded byte cache implementation for efficient storage and retrieval of byte slices.
///
/// # Design
///
/// The cache is designed with the following key features:
/// - Sharded architecture for concurrent access
/// - Memory-efficient buffer management with reuse
/// - Fixed-size buffer allocation strategy
/// - Cache position encoding for efficient storage
///
/// The implementation uses a two-level structure:
/// - `BytesCache`: The main cache interface with multiple shards
/// - `BytesCacheShard`: Individual shards managing buffer allocation and storage
///
/// # Memory Management
///
/// The cache uses a combination of strategies to manage memory efficiently:
/// - Buffer reuse through a free list
/// - Fixed-size buffers to reduce fragmentation
/// - Automatic buffer allocation when needed
/// - Configurable shard count for scalability
use qmdb_common::def::{BIG_BUF_SIZE, BYTES_CACHE_SHARD_COUNT};
use qmdb_common::utils::{new_big_buf_boxed, BigBuf};
use std::collections::HashMap;
use std::hash::Hash;
use std::time::Instant;

const VALUE_LENGTH_BITS: usize = 16;
const IDX_BITS: usize = 16;
const OFFSET_BITS: usize = 32;

/// Creates a cache position by combining buffer index and offset.
///
/// # Arguments
///
/// * `idx` - The buffer index
/// * `offset` - The offset within the buffer
/// * `value_len` - The length of the value to be stored
///
/// # Returns
///
/// * A u64 value encoding both the index and offset
pub fn make_cache_pos(idx: u32, offset: u32, value_len: usize) -> u64 {
    assert!(idx >> IDX_BITS == 0, "idx={idx} is too large");
    assert!(
        value_len >> VALUE_LENGTH_BITS == 0,
        "value_len={value_len} is too large"
    );
    ((value_len as u64) << (IDX_BITS + OFFSET_BITS))
        | ((idx as u64) << OFFSET_BITS)
        | (offset as u64)
}

/// Splits a cache position into its component buffer index and offset.
///
/// # Arguments
///
/// * `idx_and_offset` - The combined cache position value
///
/// # Returns
///
/// * A tuple containing the buffer index and offset as usize values
pub fn split_cache_pos(cache_pos: u64) -> (usize, usize, usize) {
    let value_len = (cache_pos >> (IDX_BITS + OFFSET_BITS)) as usize;
    let idx = ((cache_pos << VALUE_LENGTH_BITS) >> (OFFSET_BITS + VALUE_LENGTH_BITS)) as usize;
    let offset =
        ((cache_pos << (IDX_BITS + VALUE_LENGTH_BITS)) >> (IDX_BITS + VALUE_LENGTH_BITS)) as usize;
    (idx, offset, value_len)
}

pub fn clear_cache_pos_value_len(cache_pos: u64) -> u64 {
    cache_pos << VALUE_LENGTH_BITS >> VALUE_LENGTH_BITS
}

/// A shard of the bytes cache that manages a subset of the cached data.
///
/// Each shard maintains its own buffer pool and position mapping to reduce contention
/// in multi-threaded scenarios.
pub struct BytesCacheShard<KT: Hash + Eq + Clone> {
    /// Maps cache keys to their positions in the buffers
    pub pos_map: HashMap<KT, u64>,
    /// List of active buffers containing cached data
    pub buf_list: Vec<Box<BigBuf>>,
    /// List of unused buffers available for reuse
    free_list: Vec<Box<BigBuf>>,
    /// Index of the current buffer being written to
    pub curr_buf_idx: u32,
    /// Current write position within the current buffer
    pub curr_offset: u32,
}

impl<KT: Hash + Eq + Clone> Default for BytesCacheShard<KT> {
    fn default() -> Self {
        Self::new()
    }
}

impl<KT: Hash + Eq + Clone> BytesCacheShard<KT> {
    /// Creates a new empty cache shard.
    pub fn new() -> Self {
        Self {
            pos_map: HashMap::new(),
            buf_list: Vec::new(),
            free_list: Vec::new(),
            curr_buf_idx: 0,
            curr_offset: 0,
        }
    }

    /// Clears all cached data from the shard.
    ///
    /// This operation moves all buffers to the free list and retains 7/8 of them
    /// for future use to balance memory usage and allocation overhead.
    pub fn clear(&mut self) {
        self.pos_map.clear();
        self.free_list.append(&mut self.buf_list);
        self.free_list.truncate(self.free_list.len() * 7 / 8); //keep 7/8 bufs
        self.curr_buf_idx = 0;
        self.curr_offset = 0;
    }

    /// Allocates a new big buffer, either from the free list or by creating a new one.
    ///
    /// # Returns
    ///
    /// * A boxed BigBuf either from the free list or newly allocated
    fn allocate_big_buf(&mut self) -> Box<BigBuf> {
        if self.free_list.is_empty() {
            return new_big_buf_boxed();
        }
        self.free_list.pop().unwrap()
    }

    /// Inserts a cache position for a given key.
    ///
    /// # Arguments
    ///
    /// * `cache_key` - The key to associate with the position
    /// * `cache_pos` - The position value to store
    pub fn insert(&mut self, cache_key: &KT, cache_pos: u64) {
        self.pos_map.insert(cache_key.clone(), cache_pos);
    }

    /// Fills a buffer with the provided bytes and returns its position.
    ///
    /// This method handles buffer allocation and management automatically,
    /// creating new buffers when needed and managing the write position.
    ///
    /// # Arguments
    ///
    /// * `bz` - The bytes to store in the cache
    ///
    /// # Returns
    ///
    /// * A tuple of (buffer_index, offset) indicating where the bytes were stored
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

    /// Checks if a key exists in the cache.
    ///
    /// # Arguments
    ///
    /// * `cache_key` - The key to check
    ///
    /// # Returns
    ///
    /// * `true` if the key exists and has a valid position, `false` otherwise
    fn _contains(&self, cache_key: &KT) -> bool {
        self.pos_map.contains_key(cache_key)
    }

    /// Reads bytes at a cached position and passes them to the provided function.
    ///
    /// # Arguments
    ///
    /// * `cache_key` - The key to look up
    /// * `access` - A function that will be called with the cached bytes if found
    ///
    /// # Returns
    ///
    /// * `true` if the key was found and the access function was called, `false` otherwise
    fn _read_bytes_at<F>(&self, cache_key: &KT, mut access: F)
    where
        F: FnMut(&[u8]),
    {
        if let Some(cache_pos) = self.pos_map.get(cache_key) {
            let (buf_idx, offset, len) = split_cache_pos(*cache_pos);
            access(&self.buf_list[buf_idx][offset..offset + len]);
        }
    }
}

/// A sharded cache for storing and retrieving byte slices.
///
/// This cache provides concurrent access through multiple shards, each protected
/// by a read-write lock. The sharding strategy helps reduce contention in
/// multi-threaded scenarios.
pub struct BytesCache<KT: Hash + Eq + Clone> {
    /// Vector of shards, each protected by a RwLock
    pub shards: Vec<RwLock<BytesCacheShard<KT>>>,
}

impl<KT: Hash + Eq + Clone> Default for BytesCache<KT> {
    fn default() -> Self {
        Self::new()
    }
}

impl<KT: Hash + Eq + Clone> BytesCache<KT> {
    /// Creates a new bytes cache with the configured number of shards.
    pub fn new() -> Self {
        let mut shards = Vec::with_capacity(BYTES_CACHE_SHARD_COUNT);
        for _ in 0..BYTES_CACHE_SHARD_COUNT {
            shards.push(RwLock::new(BytesCacheShard::<KT>::new()));
        }
        Self { shards }
    }

    /// Clears all cached data from all shards.
    pub fn clear(&self) {
        for idx in 0..BYTES_CACHE_SHARD_COUNT {
            let mut shard = self.shards[idx].write();
            shard.clear();
        }
    }

    /// Inserts bytes into the cache at the specified shard index.
    ///
    /// # Arguments
    ///
    /// * `cache_key` - The key to associate with the bytes
    /// * `idx` - The shard index to use
    /// * `bz` - The bytes to cache
    pub fn insert(&self, cache_key: &KT, idx: usize, bz: &[u8]) {
        let mut shard = self.shards[idx].write();
        let (idx, offset) = shard.fill(bz);
        let pos = make_cache_pos(idx, offset, bz.len());
        shard.insert(cache_key, pos);
    }
    // pub fn insert(&self, cache_key: &KT, idx: usize, bz: &[u8]) {
    //     let start_time = Instant::now();
    //     let size = bz.len();

    //     // [探针 1] 记录拿读锁的时间
    //     let t0 = Instant::now();
    //     let needs_alloc = {
    //         let shard = self.shards[idx].read();
    //         let needs_new_buf = shard.buf_list.is_empty() 
    //             || (shard.curr_offset as usize + size > BIG_BUF_SIZE);
    //         needs_new_buf && shard.free_list.is_empty()
    //     }; 
    //     let t_read_lock_micros = t0.elapsed().as_micros();

    //     // [探针 2] 记录向系统申请内存的时间
    //     let t1 = Instant::now();
    //     let mut pre_allocated_buf = None;
    //     if needs_alloc {
    //         pre_allocated_buf = Some(new_big_buf_boxed());
    //     }
    //     let t_alloc_micros = t1.elapsed().as_micros();

    //     // [探针 3] 记录拿写锁的时间
    //     let t2 = Instant::now();
    //     let mut shard = self.shards[idx].write();
    //     let t_write_lock_micros = t2.elapsed().as_micros();

    //     // [探针 4] 记录纯粹写数据的耗时
    //     let t3 = Instant::now();
    //     if let Some(new_buf) = pre_allocated_buf {
    //         shard.free_list.push(new_buf);
    //     }

    //     let (buf_idx, offset) = shard.fill(bz);
    //     let pos = make_cache_pos(buf_idx, offset, bz.len());
    //     shard.insert(cache_key, pos);
    //     let t_work_micros = t3.elapsed().as_micros();

    //     let total_ms = start_time.elapsed().as_millis();
        
    //     // 耗时 > 50ms 报警
    //     if total_ms > 50 {
    //         println!(
    //             "⚠️ CACHE INSERT SLOW | total={}ms | read_lock={}ms | alloc={}ms | write_lock={}ms | work={}ms",
    //             total_ms,
    //             t_read_lock_micros as f64 / 1000.0,
    //             t_alloc_micros as f64 / 1000.0,
    //             t_write_lock_micros as f64 / 1000.0,
    //             t_work_micros as f64 / 1000.0
    //         );
    //     }
    // }

    /// Inserts bytes into the cache only if the key doesn't already exist.
    ///
    /// # Arguments
    ///
    /// * `cache_key` - The key to check and potentially store
    /// * `idx` - The shard index to use
    /// * `bz` - The bytes to cache if the key is new
    pub fn insert_if_missing(&self, cache_key: &KT, idx: usize, bz: &[u8]) {
        let mut shard = self.shards[idx].write();
        if shard._contains(cache_key) {
            return;
        }
        let (idx, offset) = shard.fill(bz);
        let pos = make_cache_pos(idx, offset, bz.len());
        shard.insert(cache_key, pos);
    }
    // pub fn insert_if_missing(&self, cache_key: &KT, idx: usize, bz: &[u8]) {
    //     let size = bz.len();

    //     // 1. 同样先用读锁探测
    //     let needs_alloc = {
    //         let shard = self.shards[idx].read();
    //         // 如果 key 已经存在，连内存都不用申请了，直接返回
    //         if shard._contains(cache_key) {
    //             return;
    //         }
    //         let needs_new_buf = shard.buf_list.is_empty() 
    //             || (shard.curr_offset as usize + size > BIG_BUF_SIZE);
    //         needs_new_buf && shard.free_list.is_empty()
    //     };

    //     // 2. 无锁状态下慢吞吞地分配内存
    //     let mut pre_allocated_buf = None;
    //     if needs_alloc {
    //         pre_allocated_buf = Some(new_big_buf_boxed());
    //     }

    //     // 3. 拿写锁干活
    //     let mut shard = self.shards[idx].write();
        
    //     // 双重检查：因为在我们分配内存的这段时间里，可能别的线程已经把这个 key 塞进去了
    //     if shard._contains(cache_key) {
    //         // 如果别人已经塞了，我们申请的 new_buf 就当做慈善捐给 free_list 了
    //         if let Some(new_buf) = pre_allocated_buf {
    //             shard.free_list.push(new_buf);
    //         }
    //         return;
    //     }

    //     if let Some(new_buf) = pre_allocated_buf {
    //         shard.free_list.push(new_buf);
    //     }

    //     let (buf_idx, offset) = shard.fill(bz);
    //     let pos = make_cache_pos(buf_idx, offset, bz.len());
    //     shard.insert(cache_key, pos);
    // }

    /// Looks up cached bytes by key and passes them to the provided function.
    ///
    /// # Arguments
    ///
    /// * `cache_key` - The key to look up
    /// * `idx` - The shard index to use
    /// * `access` - A function that will be called with the cached bytes if found
    ///
    /// # Returns
    ///
    /// * `true` if the key was found and the access function was called, `false` otherwise
    pub fn lookup<F>(&self, cache_key: &KT, idx: usize, access: F)
    where
        F: FnMut(&[u8]),
    {
        let shard = self.shards[idx].read();
        shard._read_bytes_at(cache_key, access)
    }
    // pub fn lookup<F>(&self, cache_key: &KT, idx: usize, mut access: F)
    // where
    //     F: FnMut(&[u8]),
    // {
    //     let start_time = Instant::now();
        
    //     // [探针 1] 记录拿读锁的时间
    //     let t0 = Instant::now();
    //     let shard = self.shards[idx].read();
    //     let t_read_lock_micros = t0.elapsed().as_micros();
        
    //     // [探针 2] 记录纯查找耗时
    //     let t1 = Instant::now();
    //     shard._read_bytes_at(cache_key, access);
    //     let t_work_micros = t1.elapsed().as_micros();
        
    //     let total_ms = start_time.elapsed().as_millis();
        
    //     // 耗时 > 50ms 报警
    //     if total_ms > 50 {
    //         println!(
    //             "⚠️ CACHE LOOKUP SLOW | total={}ms | read_lock={}ms | work={}ms",
    //             total_ms,
    //             t_read_lock_micros as f64 / 1000.0,
    //             t_work_micros as f64 / 1000.0
    //         );
    //     }
    // }

    pub fn contains(&self, cache_key: &KT, idx: usize) -> bool {
        let shard = self.shards[idx].read();
        shard._contains(cache_key)
    }
}

#[cfg(test)]
mod cache_shard_test {
    use super::*;

    #[test]
    fn test_shard() {
        let mut shard = BytesCacheShard::<i64>::new();

        let (buf_idx, offset) = shard.fill(&[1; BIG_BUF_SIZE - 1]);
        assert_eq!(0, buf_idx);
        assert_eq!(0, offset);
        assert_eq!(0, shard.curr_buf_idx);
        assert_eq!(BIG_BUF_SIZE - 1, shard.curr_offset as usize);
        let cache_pos = make_cache_pos(buf_idx, offset, BIG_BUF_SIZE - 1);
        shard.insert(&1, cache_pos);

        let (buf_idx, offset) = shard.fill(&[2]);
        assert_eq!(0, buf_idx);
        assert_eq!(BIG_BUF_SIZE - 1, offset as usize);
        assert_eq!(0, shard.curr_buf_idx);
        assert_eq!(BIG_BUF_SIZE, shard.curr_offset as usize);
        let cache_pos = make_cache_pos(buf_idx, offset, 1);
        shard.insert(&2, cache_pos);

        let _ = shard.fill(&[3; BIG_BUF_SIZE - 1]);
        let (buf_idx, offset) = shard.fill(&[4; BIG_BUF_SIZE - 1]);
        assert_eq!(2, buf_idx);
        assert_eq!(0, offset);
        assert_eq!(2, shard.curr_buf_idx);
        assert_eq!(BIG_BUF_SIZE - 1, shard.curr_offset as usize);
        let cache_pos = make_cache_pos(buf_idx, offset, BIG_BUF_SIZE - 1);
        shard.insert(&4, cache_pos);

        let mut found = false;
        shard._read_bytes_at(&1, |data| {
            assert_eq!([1; BIG_BUF_SIZE - 1], data[..BIG_BUF_SIZE - 1]);
            found = true;
        });
        assert!(found);

        let mut found = false;
        shard._read_bytes_at(&2, |data| {
            assert_eq!([2], data[..1]);
            found = true;
        });
        assert!(found);

        let found = shard._contains(&3);
        assert!(!found);

        let mut found = false;
        shard._read_bytes_at(&4, |data| {
            assert_eq!([4; BIG_BUF_SIZE - 1], data[..BIG_BUF_SIZE - 1]);
            found = true;
        });
        assert!(found);
    }
}

#[cfg(test)]
mod cache_test {
    use super::*;

    #[test]
    fn test_cache() {
        let cache = BytesCache::<i64>::new();
        cache.insert(&11, 1, "hahaha".as_bytes());
        cache.insert(&22, 2, "wawawa".as_bytes());

        let mut buf: [u8; 6] = [0; 6];
        cache.lookup(&11, 1, |data| {
            buf.copy_from_slice(&data[..6]);
        });
        assert_eq!("686168616861", hex::encode(buf));
    }
}