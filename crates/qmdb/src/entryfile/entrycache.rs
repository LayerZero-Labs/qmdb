use crate::utils::bytescache::{clear_cache_pos_value_len, BytesCache};
use byteorder::{BigEndian, ByteOrder};
use qmdb_common::def::{BIG_BUF_SIZE, BYTES_CACHE_SHARD_COUNT, SHARD_COUNT};
use qmdb_common::entryfile::entry::EntryBz;
use qmdb_common::entryfile::readbuf::ReadBuf;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};

pub struct EntryCache {
    bc: Vec<BytesCache<i64>>,
}

impl Default for EntryCache {
    fn default() -> Self {
        Self::new()
    }
}

impl EntryCache {
    pub fn new() -> Self {
        let mut bc = Vec::with_capacity(SHARD_COUNT);
        for _ in 0..SHARD_COUNT {
            bc.push(BytesCache::<i64>::new());
        }
        EntryCache { bc }
    }

    pub fn clear(&self) {
        for cache in self.bc.iter() {
            cache.clear();
        }
    }

    pub fn new_uninit() -> Self {
        EntryCache {
            bc: Vec::with_capacity(0),
        }
    }

    pub fn pos2idx(file_pos: i64) -> usize {
        // make the low 9 bits have more randomness
        let tmp = ((file_pos >> 8) ^ file_pos) as usize;
        tmp % BYTES_CACHE_SHARD_COUNT
    }

    pub fn insert(&self, shard_id: usize, file_pos: i64, entry_bz: &EntryBz) {
        // Get the actual entry size from the header and only store that many bytes.
        // entry_bz.bz may be larger (e.g., from a ReadBuf with extra capacity).
        let entry_size = EntryBz::get_entry_len(entry_bz.bz);
        debug_assert!(
            entry_size <= entry_bz.bz.len(),
            "entrycache insert: entry claims size {} but bz only has {} bytes",
            entry_size, entry_bz.bz.len()
        );
        if entry_size > entry_bz.bz.len() {
            // Safety: don't insert truncated data
            return;
        }
        let idx = Self::pos2idx(file_pos);
        // Only store the actual entry bytes, not the entire bz buffer
        self.bc[shard_id].insert(&file_pos, idx, &entry_bz.bz[..entry_size])
    }

    pub fn contains(&self, shard_id: usize, file_pos: i64) -> bool {
        let idx = Self::pos2idx(file_pos);
        self.bc[shard_id].contains(&file_pos, idx)
    }

    pub fn lookup(&self, shard_id: usize, file_pos: i64, buf: &mut ReadBuf) {
        let idx = Self::pos2idx(file_pos);
        self.bc[shard_id].lookup(&file_pos, idx, |cache_bz: &[u8]| {
            // With the fixed insert(), cache_bz should now exactly match entry size.
            // The entry header is stored at the beginning of cache_bz.
            let size = EntryBz::get_entry_len(cache_bz);
            assert!(
                size <= cache_bz.len(),
                "entrycache lookup: entry claims size {} but cache only has {} bytes (file_pos={})",
                size, cache_bz.len(), file_pos
            );
            buf.initialize_from(&cache_bz[..size]);
        });
    }

    pub fn serialize(&self) -> Vec<Vec<u8>> {
        let data: Vec<Vec<u8>> = self
            .bc
            .par_iter()
            .map(|bytes_cache| {
                let size = bytes_cache
                    .shards
                    .iter()
                    .map(|shard| {
                        let shard = shard.read();
                        8 // max: pos_map.len()
                    + shard.pos_map.len() * (8+8) // max: pos_map's key&value
                    + (shard.curr_buf_idx as usize * BIG_BUF_SIZE + shard.curr_offset as usize) // max: flattened(shard.buf_list).len()
                    + shard.curr_offset as usize // max: flattened(shard.buf_list)
                    })
                    .sum::<usize>()
                    + 8; // total entry count
                let mut shard_data = Vec::with_capacity(size);

                let mut total_entry_count = 0;
                for i in 0..BYTES_CACHE_SHARD_COUNT {
                    let bytes_cache_shard = bytes_cache.shards[i].read();
                    let pos_map = &bytes_cache_shard.pos_map;
                    let entry_count = pos_map.len();

                    // pos_map.len()
                    put_var_u64(&mut shard_data, entry_count as u64);
                    if entry_count == 0 {
                        continue;
                    }

                    // pos_map's key&value
                    total_entry_count += entry_count;
                    pos_map.iter().for_each(|(key, value)| {
                        put_var_u64(&mut shard_data, *key as u64);
                        // value len doesnot must include
                        put_var_u64(&mut shard_data, clear_cache_pos_value_len(*value));
                    });

                    // flattened(shard.buf_list).len()
                    let flattened_buf_list_len = (bytes_cache_shard.curr_buf_idx as usize
                        * BIG_BUF_SIZE
                        + bytes_cache_shard.curr_offset as usize)
                        as u64;
                    put_var_u64(&mut shard_data, flattened_buf_list_len);

                    // flattened(shard.buf_list)
                    for buf_idx in 0..bytes_cache_shard.curr_buf_idx as usize {
                        let buf = &bytes_cache_shard.buf_list[buf_idx];
                        shard_data.extend_from_slice(buf);
                    }
                    if bytes_cache_shard.curr_offset > 0 {
                        shard_data.extend_from_slice(
                            &bytes_cache_shard.buf_list[bytes_cache_shard.curr_buf_idx as usize]
                                [..bytes_cache_shard.curr_offset as usize],
                        );
                    }
                }

                // total entry count
                shard_data.extend_from_slice((total_entry_count as u64).to_le_bytes().as_slice());
                // TODO: check the size
                shard_data
            })
            .collect();
        data
    }
}

pub fn put_var_u64(buf: &mut Vec<u8>, n: u64) {
    fn count_leading_zeros(buf: &[u8]) -> usize {
        let mut n = 0;
        for &x in buf {
            if x == 0 {
                n += 1;
            } else {
                break;
            }
        }
        n
    }

    let mut tmp = [0u8; 8];
    BigEndian::write_u64(&mut tmp, n);
    let zeros = count_leading_zeros(&tmp);
    buf.push((8 - zeros) as u8);
    let bz = &tmp[zeros..];
    buf.extend_from_slice(bz);
}

pub fn get_var_u64(data: &[u8]) -> (u64, usize) {
    let mut buf = [0u8; 8];
    let n = data[0] as usize;
    let leading_zeros = 8 - n;
    buf[leading_zeros..].copy_from_slice(&data[1..n + 1]);
    (BigEndian::read_u64(&buf), n + 1)
}

#[cfg(test)]
mod tests {
    use qmdb_common::entryfile::helpers::EntryBuilder;

    use super::*;

    #[test]
    fn test_insert_lookup() {
        let cache = EntryCache::new();
        let shard_id = 7;

        let entry1 = EntryBuilder::kv("key1", "val1").build_and_dump(&[1]);
        let entry2 = EntryBuilder::kv("key2", "val2").build_and_dump(&[2]);
        let entry3 = EntryBuilder::kv("key3", "val3").build_and_dump(&[3]);
        cache.insert(shard_id, 1234, &EntryBz { bz: &entry1[..] });
        cache.insert(shard_id, 2345, &EntryBz { bz: &entry2[..] });
        cache.insert(shard_id, 1746, &EntryBz { bz: &entry3[..] });

        let mut buf = ReadBuf::new();
        cache.lookup(shard_id, 1234, &mut buf);
        assert_eq!(entry1.as_slice(), buf.as_slice());

        let mut buf = ReadBuf::new();
        cache.lookup(shard_id, 2345, &mut buf);
        assert_eq!(entry2.as_slice(), buf.as_slice());

        let mut buf = ReadBuf::new();
        cache.lookup(shard_id, 1746, &mut buf);
        assert_eq!(entry3.as_slice(), buf.as_slice());

        assert!(cache.contains(shard_id, 1234));
        assert!(!cache.contains(shard_id, 4321));
        assert!(!cache.contains(shard_id, 8888));
    }
}
