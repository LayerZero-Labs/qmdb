pub mod activebits;
pub mod bytescache;
pub mod changeset;
pub mod codec;
pub mod hasher;
pub mod ringchannel;
pub mod shortlist;
pub mod slice;

use crate::def::{BIG_BUF_SIZE, SHARD_COUNT};

pub type BigBuf = [u8]; // size is BIG_BUF_SIZE

pub fn new_big_buf_boxed() -> Box<[u8]> {
    vec![0u8; BIG_BUF_SIZE].into_boxed_slice()
}

pub fn byte0_to_shard_id(byte0: u8) -> usize {
    (byte0 as usize) * SHARD_COUNT / 256
}

#[derive(
    Debug, PartialEq, Clone, serde::Serialize, serde::Deserialize, bincode::Encode, bincode::Decode,
)]
pub struct OpRecord {
    pub op_type: u8,
    pub num_active: usize,
    pub oldest_active_sn: u64,
    pub shard_id: usize,
    pub next_sn: u64,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub rd_list: Vec<Vec<u8>>,
    pub wr_list: Vec<Vec<u8>>,
    pub dig_list: Vec<Vec<u8>>, //old entries in compaction
    pub put_list: Vec<Vec<u8>>, //new entries in compaction
}

impl OpRecord {
    pub fn new(op_type: u8) -> OpRecord {
        OpRecord {
            op_type,
            num_active: 0,
            oldest_active_sn: 0,
            shard_id: 0,
            next_sn: 0,
            key: Vec::with_capacity(0),
            value: Vec::with_capacity(0),
            rd_list: Vec::with_capacity(2),
            wr_list: Vec::with_capacity(2),
            dig_list: Vec::with_capacity(2),
            put_list: Vec::with_capacity(2),
        }
    }
}

#[cfg(test)]
mod tests {

    use super::codec::*;
    use super::hasher::*;

    #[test]
    fn test_hash2() {
        assert_eq!(
            hex::encode(hash2(8, "hello", "world")),
            "8e6fc50a3f98a3c314021b89688ca83a9b5697ca956e211198625fc460ddf1e9"
        );

        assert_eq!(
            hex::encode(hash2x(8, "world", "hello", true)),
            "8e6fc50a3f98a3c314021b89688ca83a9b5697ca956e211198625fc460ddf1e9"
        );
    }

    #[test]
    fn test_node_hash_inplace() {
        let mut target: [u8; 32] = [0; 32];
        node_hash_inplace(8, &mut target, "hello", "world");
        assert_eq!(
            hex::encode(target),
            "8e6fc50a3f98a3c314021b89688ca83a9b5697ca956e211198625fc460ddf1e9"
        );
    }

    #[test]
    fn test_encode_decode_n64() {
        let v = vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        assert_eq!(decode_le_i64(&v), -8613303245920329199);
        assert_eq!(decode_le_u64(&v), 0x8877665544332211);
        assert_eq!(encode_le_i64(-8613303245920329199), v);
        assert_eq!(encode_le_u64(0x8877665544332211), v);
    }
}
