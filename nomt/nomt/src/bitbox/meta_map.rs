//! in-memory metadata for each bucket. this is also persisted on disk.

const EMPTY: u8 = 0b0000_0000;
const TOMBSTONE: u8 = 0b0111_1111;
const FULL_MASK: u8 = 0b1000_0000;

fn full_entry(hash: u64) -> u8 {
    (hash >> 57) as u8 ^ FULL_MASK
}

pub struct MetaMap {
    buckets: usize,
    bitvec: Vec<u8>,
}

impl MetaMap {
    // Create a new meta-map from an existing vector.
    pub fn from_bytes(meta_bytes: Vec<u8>, buckets: usize) -> Self {
        assert_eq!(meta_bytes.len() % 4096, 0);
        MetaMap {
            buckets,
            bitvec: meta_bytes,
        }
    }

    pub fn full_count(&self) -> usize {
        self.bitvec
            .iter()
            .filter(|&&byte| byte & FULL_MASK != 0)
            .count()
    }

    pub fn len(&self) -> usize {
        self.buckets
    }

    pub fn set_full(&mut self, bucket: usize, hash: u64) {
        self.bitvec[bucket] = full_entry(hash);
    }

    pub fn set_tombstone(&mut self, bucket: usize) {
        self.bitvec[bucket] = TOMBSTONE;
    }

    // true means definitely empty.
    pub fn hint_empty(&self, bucket: usize) -> bool {
        self.bitvec[bucket] == EMPTY
    }

    // true means definitely a tombstone.
    pub fn hint_tombstone(&self, bucket: usize) -> bool {
        self.bitvec[bucket] == TOMBSTONE
    }

    // returns true if it's definitely not a match.
    pub fn hint_not_match(&self, bucket: usize, raw_hash: u64) -> bool {
        self.bitvec[bucket] != full_entry(raw_hash)
    }

    // get the page index of a bucket in the meta-map.
    pub fn page_index(&self, bucket: usize) -> usize {
        bucket / 4096
    }

    // get a page-sized slice of the metamap. This is guaranteed to have len 4096
    pub fn page_slice(&self, page_index: usize) -> &[u8] {
        let start = page_index * 4096;
        let end = start + 4096;
        &self.bitvec[start..end]
    }
}
