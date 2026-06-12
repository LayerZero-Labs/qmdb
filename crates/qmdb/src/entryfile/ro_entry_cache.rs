use std::collections::HashMap;

use qmdb_common::{
    def::{BIG_BUF_SIZE, BYTES_CACHE_SHARD_COUNT, SHARD_COUNT},
    entryfile::{entry::EntryBz, readbuf::ReadBuf},
};
use rayon::iter::{IntoParallelIterator, ParallelIterator};

use crate::utils::bytescache::split_cache_pos;

use super::{entrycache::get_var_u64, EntryCache};

struct RoEntryCacheShard {
    pos_map: HashMap<i64, u64>,
    bytes_cache_shard_buf_start_list: Vec<usize>,
    sn_to_file_pos_map: HashMap<u64, i64>,
    /*
        BYTES_CACHE_SHARD_COUNT * (used_shard_data.len() + pos_map's key&value + flattened buf_list len + flattened buf_list + total_entry_count
    */
    data: Vec<u8>,
}

pub struct RoEntryCache {
    shards: Vec<RoEntryCacheShard>,
}

impl RoEntryCache {
    pub fn new_uninit() -> Self {
        RoEntryCache {
            shards: Vec::with_capacity(SHARD_COUNT),
        }
    }

    pub fn sn_to_file_pos(&self, shard_id: usize, sn: u64) -> i64 {
        *self.shards[shard_id].sn_to_file_pos_map.get(&sn).unwrap()
    }

    pub fn from(data: Vec<Vec<u8>>) -> Self {
        let shards: Vec<RoEntryCacheShard> = data
            .into_par_iter()
            .map(|shard_data| {
                let total_entry_count =
                    u64::from_le_bytes(shard_data[shard_data.len() - 8..].try_into().unwrap())
                        as usize;

                let mut pos_map = HashMap::with_capacity(total_entry_count);
                let mut bytes_cache_shard_buf_start_list =
                    Vec::with_capacity(BYTES_CACHE_SHARD_COUNT);

                let mut offset = 0;
                for _ in 0..BYTES_CACHE_SHARD_COUNT {
                    let (total_entry_count, len) = get_var_u64(&shard_data.as_slice()[offset..]);
                    offset += len;

                    if total_entry_count == 0 {
                        bytes_cache_shard_buf_start_list.push(offset);
                        continue;
                    }

                    // rebuild the pos_map
                    for _ in 0..total_entry_count {
                        let (key, len) = get_var_u64(&shard_data.as_slice()[offset..]);
                        offset += len;
                        let (value, len) = get_var_u64(&shard_data.as_slice()[offset..]);
                        offset += len;
                        pos_map.insert(key as i64, value);
                    }

                    let (flattened_buf_list_len, len) =
                        get_var_u64(&shard_data.as_slice()[offset..]);
                    offset += len;

                    bytes_cache_shard_buf_start_list.push(offset);

                    offset += flattened_buf_list_len as usize;
                }
                assert!(offset == shard_data.len() - 8);

                // rebuild the sn_to_file_pos
                let mut sn_to_file_pos_map = HashMap::with_capacity(total_entry_count);
                for (file_pos, pos) in pos_map.iter() {
                    let byte_cache_shard_idx = EntryCache::pos2idx(*file_pos);
                    let (buf_idx, _offset, _len) = split_cache_pos(*pos);
                    // cache_pos not include the len
                    assert!(_len == 0);
                    let start = bytes_cache_shard_buf_start_list[byte_cache_shard_idx]
                        + buf_idx * BIG_BUF_SIZE
                        + _offset;
                    let len = EntryBz::get_entry_len(&shard_data[start..]);
                    let entry_bz = EntryBz {
                        bz: &shard_data[start..start + len],
                    };
                    let sn = entry_bz.serial_number();
                    sn_to_file_pos_map.insert(sn, *file_pos);
                }

                RoEntryCacheShard {
                    pos_map,
                    bytes_cache_shard_buf_start_list,
                    sn_to_file_pos_map,
                    data: shard_data,
                }
            })
            .collect();
        RoEntryCache { shards }
    }

    pub fn lookup(&self, shard_id: usize, file_pos: i64, buf: &mut ReadBuf) {
        let shard = &self.shards[shard_id];
        let pos_opt = shard.pos_map.get(&file_pos);
        let byte_cache_shard_idx = EntryCache::pos2idx(file_pos);

        if let Some(pos) = pos_opt {
            let (buf_idx, offset, _) = split_cache_pos(*pos);
            let new_offset = shard.bytes_cache_shard_buf_start_list[byte_cache_shard_idx]
                + buf_idx * BIG_BUF_SIZE
                + offset;
            let len = EntryBz::get_entry_len(&shard.data[new_offset..]);
            buf.initialize_from(&shard.data[new_offset..new_offset + len]);
        }
    }
}

#[cfg(test)]
mod tests {
    use qmdb_common::entryfile::{entry::EntryBz, helpers::EntryBuilder, readbuf::ReadBuf};

    use crate::entryfile::{ro_entry_cache::RoEntryCache, EntryCache};

    #[test]
    fn test_ro_entry_cache() {
        let entry_cache = EntryCache::new();
        let entry = EntryBuilder::kv("key1", "val1").build_and_dump(&[1]);
        let shard_id = 7;
        entry_cache.insert(shard_id, 1234, &EntryBz { bz: &entry[..] });

        let bz = entry_cache.serialize();
        let ro_entry_cache = RoEntryCache::from(bz);
        let mut read_buf = ReadBuf::new();
        ro_entry_cache.lookup(shard_id, 1234, &mut read_buf);
        assert_eq!(read_buf.as_slice(), entry);
        assert_eq!(ro_entry_cache.sn_to_file_pos(shard_id, 0), 1234);
    }
}
