use byteorder::{BigEndian, ByteOrder};

#[derive(Debug, Clone)]
pub struct ShuffleParam {
    pub total_bits: usize,
    pub entry_count: u64,
    pub entry_count_wo_del: u64,
    pub rotate_bits: usize,
    pub add_num: u64,
    pub xor_num: u64,
}

impl ShuffleParam {
    pub fn new(entry_count: u64, entry_count_wo_del: u64) -> Self {
        Self {
            total_bits: 64 - (entry_count - 1).leading_zeros() as usize,
            entry_count,
            entry_count_wo_del,
            rotate_bits: 0,
            add_num: 0,
            xor_num: 0,
        }
    }

    pub fn change(&self, mut x: u64) -> u64 {
        let mask = (1u64 << self.total_bits) - 1;
        x = x.reverse_bits() >> (64 - self.total_bits);
        x += self.add_num;
        x = (!x) & mask;
        x = (x >> self.rotate_bits) | (x << (self.total_bits - self.rotate_bits));
        x ^= self.xor_num;

        let mut buf = [0u8; 8];
        BigEndian::write_u64(&mut buf[..8], x);
        let hash = qmdb_common::entryfile::entry::entry_key_hash(&buf[..]);
        x = BigEndian::read_u64(&hash[..8]);

        x % self.entry_count_wo_del
    }
}
