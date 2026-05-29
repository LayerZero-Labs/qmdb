/// We just copy some code from https://docs.rs/byteorder/latest/byteorder/
/// such that it can be used in no_std environment.

/// LittleEndian is a module that provides functions to read and write u64, i64, u32, i32, u16, i16
pub struct LittleEndian {}

impl LittleEndian {
    pub fn read_u64(v: &[u8]) -> u64 {
        u64::from_le_bytes(v[0..8].try_into().unwrap())
    }

    pub fn write_u64(buf: &mut [u8], n: u64) {
        buf.copy_from_slice(&n.to_le_bytes());
    }

    pub fn read_i64(v: &[u8]) -> i64 {
        i64::from_le_bytes(v[0..8].try_into().unwrap())
    }

    pub fn write_i64(buf: &mut [u8], n: i64) {
        buf.copy_from_slice(&n.to_le_bytes());
    }

    pub fn read_u32(v: &[u8]) -> u32 {
        u32::from_le_bytes(v[0..4].try_into().unwrap())
    }

    pub fn write_u32(buf: &mut [u8], n: u32) {
        buf.copy_from_slice(&n.to_le_bytes());
    }

    pub fn read_i32(v: &[u8]) -> i32 {
        i32::from_le_bytes(v[0..4].try_into().unwrap())
    }

    pub fn write_i32(buf: &mut [u8], n: i32) {
        buf.copy_from_slice(&n.to_le_bytes());
    }

    pub fn read_u16(v: &[u8]) -> u16 {
        u16::from_le_bytes(v[0..2].try_into().unwrap())
    }

    pub fn write_u16(buf: &mut [u8], n: u16) {
        buf.copy_from_slice(&n.to_le_bytes());
    }

    pub fn read_i16(v: &[u8]) -> i16 {
        i16::from_le_bytes(v[0..2].try_into().unwrap())
    }

    pub fn write_i16(buf: &mut [u8], n: i16) {
        buf.copy_from_slice(&n.to_le_bytes());
    }
}

pub struct BigEndian {}
impl BigEndian {
    pub fn read_u64(v: &[u8]) -> u64 {
        u64::from_be_bytes(v[0..8].try_into().unwrap())
    }

    pub fn write_u64(buf: &mut [u8], n: u64) {
        buf.copy_from_slice(&n.to_be_bytes());
    }

    pub fn read_i64(v: &[u8]) -> i64 {
        i64::from_be_bytes(v[0..8].try_into().unwrap())
    }

    pub fn write_i64(buf: &mut [u8], n: i64) {
        buf.copy_from_slice(&n.to_be_bytes());
    }

    pub fn read_u32(v: &[u8]) -> u32 {
        u32::from_be_bytes(v[0..4].try_into().unwrap())
    }

    pub fn write_u32(buf: &mut [u8], n: u32) {
        buf.copy_from_slice(&n.to_be_bytes());
    }

    pub fn read_i32(v: &[u8]) -> i32 {
        i32::from_be_bytes(v[0..4].try_into().unwrap())
    }

    pub fn write_i32(buf: &mut [u8], n: i32) {
        buf.copy_from_slice(&n.to_be_bytes());
    }

    pub fn read_u16(v: &[u8]) -> u16 {
        u16::from_be_bytes(v[0..2].try_into().unwrap())
    }

    pub fn write_u16(buf: &mut [u8], n: u16) {
        buf.copy_from_slice(&n.to_be_bytes());
    }

    pub fn read_i16(v: &[u8]) -> i16 {
        i16::from_be_bytes(v[0..2].try_into().unwrap())
    }

    pub fn write_i16(buf: &mut [u8], n: i16) {
        buf.copy_from_slice(&n.to_be_bytes());
    }
}
