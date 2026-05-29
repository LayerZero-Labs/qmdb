use byteorder::{ByteOrder, LittleEndian};

pub const MERGE_THRES: usize = 20_000;
pub const MERGE_RATIO: usize = 5;
pub const MERGE_DIV: usize = 100;
pub const MERGER_WAIT: u64 = 100;

pub const L: usize = 14;
pub const PAGE_SIZE: usize = 32;
#[cfg(feature = "tee_cipher")]
pub const PAGE_BYTES: usize = PAGE_SIZE * L + crate::def::TAG_SIZE;
#[cfg(not(feature = "tee_cipher"))]
pub const PAGE_BYTES: usize = PAGE_SIZE * L;
pub const WR_BUF_SIZE: usize = PAGE_BYTES * 16 * 1024;
pub const RD_BUF_SIZE: usize = PAGE_BYTES * 8 * 1024;
pub const TEMP_FILE_COUNT: usize = 64;
pub const UNIT_COUNT: usize = 1usize << 16;
pub const UNIT_GROUP_SIZE: usize = UNIT_COUNT / TEMP_FILE_COUNT;

pub fn to_key_pos(k: u64, pos: i64) -> [u8; L] {
    let mut res = [0u8; L];
    LittleEndian::write_u64(&mut res[6..], (pos << 16) as u64);
    LittleEndian::write_u64(&mut res[..8], k);
    res
}

pub fn get_key_pos(arr: &[u8]) -> (u64, i64) {
    let pos = LittleEndian::read_u64(&arr[6..]) as i64;
    let k = LittleEndian::read_u64(&arr[..8]);
    (k, pos >> 16)
}

#[cfg(test)]
mod tests {
    use crate::indexer::hybrid::def::to_key_pos;

    #[test]
    fn test_t_arr14() {
        assert_eq!(
            "efcdab9078563412665544332211",
            hex::encode(to_key_pos(0x1234567890abcdef, 0x112233445566))
        )
    }
}
