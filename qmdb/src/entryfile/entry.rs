use crate::def::{
    BIG_BUF_SIZE, ENTRY_BASE_LENGTH, ENTRY_FIXED_LENGTH, NULL_ENTRY_VERSION, SHARD_COUNT,
    SHARD_DIV, TAG_SIZE,
};
use crate::utils::hasher::{self, Hash32};
use crate::utils::{new_big_buf_boxed, BigBuf};
use byteorder::{BigEndian, ByteOrder, LittleEndian};

#[derive(Debug)]
pub struct Entry<'a> {
    pub key: &'a [u8],
    pub value: &'a [u8],
    pub next_key_hash: &'a [u8],
    pub version: i64,
    pub serial_number: u64,
}

pub struct EntryBz<'a> {
    pub bz: &'a [u8],
}

impl<'a> Entry<'a> {
    pub fn from_bz(e: &'a EntryBz) -> Entry<'a> {
        Self {
            key: e.key(),
            value: e.value(),
            next_key_hash: e.next_key_hash(),
            version: e.version(),
            serial_number: e.serial_number(),
        }
    }

    pub fn get_serialized_len(&self, deactived_sn_count: usize) -> usize {
        let length = ENTRY_BASE_LENGTH + self.key.len() + self.value.len();
        ((length + 7) / 8) * 8 + deactived_sn_count * 8
    }

    // 1B KeyLength
    // 3B-valueLength
    // 1B DeactivedSNList length
    // ----------- encryption start
    // Key
    // Value
    // 32B NextKeyHash
    // 8B Height
    // 8B LastHeight
    // 8B SerialNumber
    // DeactivedSerialNumList (list of 8B-int)
    // ----------- encryption end
    // padding-zero-bytes
    // AES-GCM tag placeholder (if feature = "tee_cipher")
    pub fn dump(&self, b: &mut [u8], deactived_sn_list: &[u64]) -> usize {
        if b.len() < self.get_serialized_len(deactived_sn_list.len()) {
            panic!("Not enough space for dumping");
        }
        let first32 = (self.value.len() * 256 + self.key.len()) as u32;
        b[4] = deactived_sn_list.len() as u8;
        LittleEndian::write_u32(&mut b[..4], first32);
        let mut i = 5;
        b[i..i + self.key.len()].copy_from_slice(self.key);
        i += self.key.len();
        b[i..i + self.value.len()].copy_from_slice(self.value);
        i += self.value.len();

        if self.next_key_hash.len() != 32 {
            panic!("NextKeyHash is not 32-byte");
        }
        b[i..i + 32].copy_from_slice(self.next_key_hash);
        i += 32;
        LittleEndian::write_i64(&mut b[i..i + 8], self.version);
        i += 8;
        LittleEndian::write_u64(&mut b[i..i + 8], self.serial_number);
        i += 8;

        for &sn in deactived_sn_list {
            LittleEndian::write_u64(&mut b[i..i + 8], sn);
            i += 8;
        }

        while i % 8 != 0 {
            b[i] = 0;
            i += 1;
        }
        if cfg!(feature = "tee_cipher") {
            i += TAG_SIZE;
        }
        i
    }
}

pub fn sentry_entry(shard_id: usize, sn: u64, bz: &mut [u8]) -> EntryBz {
    if shard_id >= SHARD_COUNT || (sn as usize) >= (1 << 16) / SHARD_COUNT {
        panic!("SentryEntry Overflow");
    }
    let first16 = ((shard_id * (1 << 16) / SHARD_COUNT) | (sn as usize)) as u16;
    let mut key: [u8; 32] = [0; 32];
    BigEndian::write_u16(&mut key[0..2], first16);
    let mut next_key_hash: [u8; 32];
    if first16 == 0xFFFF {
        next_key_hash = [0xFF; 32];
    } else {
        next_key_hash = [0; 32];
        BigEndian::write_u16(&mut next_key_hash[0..2], first16 + 1);
    }
    let e = Entry {
        key: &key[..],
        value: &[] as &[u8],
        next_key_hash: &next_key_hash[..],
        version: 0,
        serial_number: sn,
    };
    let i = e.dump(bz, &[] as &[u64]);
    EntryBz { bz: &bz[..i] }
}

pub fn null_entry(bz: &mut [u8]) -> EntryBz {
    let next_key_hash: [u8; 32] = [0; 32];
    let e = Entry {
        key: &[] as &[u8],
        value: &[] as &[u8],
        next_key_hash: &next_key_hash[..],
        version: NULL_ENTRY_VERSION,
        serial_number: u64::MAX,
    };
    let i = e.dump(bz, &[] as &[u64]);
    EntryBz { bz: &bz[..i] }
}

pub fn entry_to_bytes<'a>(
    e: &'a Entry<'a>,
    deactived_sn_list: &'a [u64],
    bz: &'a mut [u8],
) -> EntryBz<'a> {
    let total_len = e.get_serialized_len(deactived_sn_list.len());
    e.dump(&mut bz[..], deactived_sn_list);
    EntryBz {
        bz: &bz[..total_len],
    }
}

pub fn get_kv_len(len_bytes: &[u8]) -> (usize, usize) {
    let first32 = LittleEndian::read_u32(&len_bytes[..4]);
    let key_len = (first32 & 0xff) as usize;
    let value_len = (first32 >> 8) as usize;
    (key_len, value_len)
}

impl<'a> EntryBz<'a> {
    pub fn get_entry_len(len_bytes: &[u8]) -> usize {
        let (key_len, value_len) = get_kv_len(len_bytes);
        let dsn_count = len_bytes[4] as usize;
        ((ENTRY_BASE_LENGTH + key_len + value_len + 7) / 8) * 8 + dsn_count * 8
    }

    pub fn len(&self) -> usize {
        self.bz.len()
    }

    pub fn payload_len(&self) -> usize {
        let (key_len, value_len) = get_kv_len(self.bz);
        let dsn_count = self.bz[4] as usize;
        ENTRY_FIXED_LENGTH + key_len + value_len + dsn_count * 8
    }

    pub fn hash(&self) -> Hash32 {
        hasher::hash(&self.bz[..self.payload_len()])
    }

    pub fn value(&self) -> &[u8] {
        let (key_len, value_len) = get_kv_len(self.bz);
        &self.bz[5 + key_len..5 + key_len + value_len]
    }

    pub fn key(&self) -> &[u8] {
        let key_len = self.bz[0] as usize;
        &self.bz[5..5 + key_len]
    }

    pub fn key_hash(&self) -> Hash32 {
        if self.value().is_empty() {
            let mut res: Hash32 = [0; 32];
            res[0] = self.bz[5]; // first byte of key
            res[1] = self.bz[6]; // second byte of key
            return res;
        }
        hasher::hash(self.key())
    }

    fn next_key_hash_start(&self) -> usize {
        let (key_len, value_len) = get_kv_len(&self.bz[0..4]);
        5 + key_len + value_len
    }

    pub fn next_key_hash(&self) -> &[u8] {
        let start = self.next_key_hash_start();
        &self.bz[start..start + 32]
    }

    pub fn version(&self) -> i64 {
        let start = self.next_key_hash_start() + 32;
        LittleEndian::read_i64(&self.bz[start..start + 8])
    }

    pub fn serial_number(&self) -> u64 {
        let start = self.next_key_hash_start() + 40;
        LittleEndian::read_u64(&self.bz[start..start + 8])
    }

    pub fn dsn_count(&self) -> usize {
        self.bz[4] as usize
    }

    pub fn get_deactived_sn(&self, n: usize) -> u64 {
        let (key_len, value_len) = get_kv_len(self.bz);
        let start = ENTRY_FIXED_LENGTH + key_len + value_len + n * 8;
        LittleEndian::read_u64(&self.bz[start..start + 8])
    }

    pub fn dsn_iter(&'a self) -> DSNIter<'a> {
        DSNIter {
            e: self,
            count: self.dsn_count(),
            idx: 0,
        }
    }
}

pub struct DSNIter<'a> {
    e: &'a EntryBz<'a>,
    count: usize,
    idx: usize,
}

impl Iterator for DSNIter<'_> {
    type Item = (usize, u64);

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx >= self.count {
            return None;
        }
        let sn = self.e.get_deactived_sn(self.idx);
        let idx = self.idx;
        self.idx += 1;
        Some((idx, sn))
    }
}

#[derive(Default)]
pub struct EntryVec {
    buf_list: Vec<Box<BigBuf>>,
    pos_list: Vec<Vec<usize>>, // size = SHARD_COUNT
    next_pos: usize,
}

impl EntryVec {
    pub fn from_bytes(bz: &[u8]) -> Self {
        let mut off = 0;
        let next_pos = LittleEndian::read_u64(&bz[off..]) as usize;
        off += 8;
        let mut pos_list = Vec::with_capacity(SHARD_COUNT);
        for _ in 0..SHARD_COUNT {
            let n = LittleEndian::read_u64(&bz[off..]) as usize;
            off += 8;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                let pos = LittleEndian::read_u64(&bz[off..]) as usize;
                off += 8;
                v.push(pos);
            }
            pos_list.push(v);
        }

        let mut buf_list: Vec<Box<[u8]>> = Vec::with_capacity(0);
        let total_len = bz.len();
        for _ in 0..next_pos.div_ceil(BIG_BUF_SIZE) {
            let mut buf = new_big_buf_boxed();
            let size = std::cmp::min(total_len - off, BIG_BUF_SIZE);
            buf[0..size].copy_from_slice(&bz[off..off + size]);
            buf_list.push(buf);
            off += size;
        }
        assert_eq!(off, total_len);
        Self {
            buf_list,
            pos_list,
            next_pos,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut size = (1 + SHARD_COUNT) * 8;
        for i in 0..SHARD_COUNT {
            size += self.pos_list[i].len() * 8;
        }
        let mut bz = Vec::with_capacity(size + self.next_pos);
        bz.resize(size, 0);

        let mut off = 0;
        LittleEndian::write_u64(&mut bz[off..], self.next_pos as u64);
        off += 8;
        for i in 0..SHARD_COUNT {
            LittleEndian::write_u64(&mut bz[off..], self.pos_list[i].len() as u64);
            off += 8;
            for &pos in self.pos_list[i].iter() {
                LittleEndian::write_u64(&mut bz[off..], pos as u64);
                off += 8;
            }
        }
        for (i, buf) in self.buf_list.iter().enumerate() {
            let size = std::cmp::min(self.next_pos - i * BIG_BUF_SIZE, BIG_BUF_SIZE);
            bz.extend_from_slice(&buf[..size]);
        }
        assert_eq!(size + self.next_pos, bz.len());
        bz
    }

    pub fn new() -> Self {
        Self {
            buf_list: vec![new_big_buf_boxed()],
            pos_list: (0..SHARD_COUNT).map(|_| vec![]).collect(),
            next_pos: 0,
        }
    }

    pub fn total_bytes(&self) -> usize {
        self.next_pos
    }

    pub fn batch_add_entry_data(&mut self, data: Vec<Vec<u8>>) {
        for item in data {
            self.add_entry_bz(&EntryBz { bz: &item });
        }
    }

    pub fn add_entry_bz(&mut self, e: &EntryBz) -> usize {
        let key_hash = e.key_hash();
        let shard_id = key_hash[0] as usize * 256 / SHARD_DIV;
        let length = e.len();
        let offset = self._add_entry(shard_id, length);
        let buf = self.buf_list.last_mut().unwrap();
        buf[offset..offset + length].copy_from_slice(e.bz);
        self.pos_list[shard_id].len() - 1
    }

    fn _add_entry(&mut self, shard_id: usize, length: usize) -> usize {
        let mut offset = self.next_pos % BIG_BUF_SIZE;
        if offset + length > BIG_BUF_SIZE {
            self.next_pos = self.buf_list.len() * BIG_BUF_SIZE;
            offset = 0;
            self.buf_list.push(new_big_buf_boxed());
        }
        self.pos_list[shard_id].push(self.next_pos);
        self.next_pos += length;
        offset
    }

    pub fn add_entry(&mut self, kh: &Hash32, e: &Entry, dsn_list: &[u64]) -> usize {
        let shard_id = kh[0] as usize * 256 / SHARD_DIV;
        let length = e.get_serialized_len(dsn_list.len());
        let offset = self._add_entry(shard_id, length);
        let buf = self.buf_list.last_mut().unwrap();
        e.dump(&mut buf[offset..], dsn_list);
        self.pos_list[shard_id].len() - 1
    }

    fn get_entry_by_pos(&self, pos: usize) -> EntryBz {
        let idx = pos / BIG_BUF_SIZE;
        let offset = pos % BIG_BUF_SIZE;
        let buf = self.buf_list.get(idx).unwrap();
        let length = EntryBz::get_entry_len(&buf[offset..offset + 5]);
        EntryBz {
            bz: &buf[offset..offset + length],
        }
    }

    pub fn get_entry(&self, shard_id: usize, i: usize) -> EntryBz {
        self.get_entry_by_pos(self.pos_list[shard_id][i])
    }

    pub fn enumerate(&self, shard_id: usize) -> EntryBzIter<'_> {
        EntryBzIter {
            v: self,
            idx: 0,
            shard_id,
        }
    }
}

pub struct EntryBzIter<'a> {
    v: &'a EntryVec,
    idx: usize,
    shard_id: usize,
}

impl<'a> Iterator for EntryBzIter<'a> {
    type Item = (usize, EntryBz<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx >= self.v.pos_list[self.shard_id].len() {
            return None;
        }
        let e = self.v.get_entry(self.shard_id, self.idx);
        let idx = self.idx;
        self.idx += 1;
        Some((idx, e))
    }
}

pub fn entry_to_vec(e: &Entry, deactived_sn_list: &[u64]) -> Vec<u8> {
    let total_len = ((ENTRY_BASE_LENGTH + e.key.len() + e.value.len() + 7) / 8) * 8
        + deactived_sn_list.len() * 8;
    let mut v = Vec::with_capacity(total_len);
    v.resize(total_len, 0);
    e.dump(&mut v[..], deactived_sn_list);
    v
}

pub fn entry_equal(bz: &[u8], e: &Entry, deactived_sn_list: &[u64]) -> bool {
    &entry_to_vec(e, deactived_sn_list)[..] == bz
}

#[cfg(test)]
mod entry_bz_tests {

    use crate::utils::byte0_to_shard_id;

    use super::*;

    #[test]
    fn test_dump() {
        let key = "key".as_bytes();
        let val = "value".as_bytes();
        let next_key_hash: Hash32 = [0xab; 32];
        let deactived_sn_list: [u64; 4] = [0xf1, 0xf2, 0xf3, 0xf4];

        let entry = Entry {
            key,
            value: val,
            next_key_hash: &next_key_hash,
            version: 12345,
            serial_number: 99999,
        };

        let mut buf: [u8; 1024] = [0; 1024];
        let n = entry.dump(&mut buf, &deactived_sn_list);
        let mut len = 96;
        if cfg!(feature = "tee_cipher") {
            len += TAG_SIZE;
        }
        assert_eq!(len, n);

        #[rustfmt::skip]
        assert_eq!(
            hex::encode(&buf[0..n]),
            [
                "03",         // key len
                "050000",     // val len
                "04",         // deactived_sn_list len
                "6b6579",     // key
                "76616c7565", // val
                "abababababababababababababababababababababababababababababababab", // next key hash
                "3930000000000000", // version
                "9f86010000000000", // serial number
                "f100000000000000", // deactived_sn_list
                "f200000000000000",
                "f300000000000000",
                "f400000000000000",
                "000000",     // padding
                #[cfg(feature = "tee_cipher")]
                "00000000000000000000000000000000" // AES-GCM tag placeholder
        ].join(""),
        );
    }

    #[test]
    fn test_entry_vec() {
        let mut entry_vec = EntryVec::new();

        assert_eq!(entry_vec.next_pos, 0);
        assert_eq!(entry_vec.buf_list.len(), 1);
        assert_eq!(entry_vec.pos_list.len(), SHARD_COUNT);

        let mut entry = Entry {
            key: "key".as_bytes(),
            value: &vec![0; 1000],
            next_key_hash: &[0x11; 32],
            version: -1,
            serial_number: 0,
        };
        let shard_id = byte0_to_shard_id(hasher::hash(entry.key)[0]);
        let len = 70;
        for i in 0..len {
            entry.serial_number = i;
            let dsn_list = [i + 1];
            let len = entry_vec.add_entry(&hasher::hash(entry.key), &entry, &dsn_list);
            assert_eq!(len, i as usize);
        }
        for i in len..len * 2 {
            entry.serial_number = i;
            let dsn_list = [i + 1];
            let length = entry.get_serialized_len(dsn_list.len());
            let mut buf = vec![0; length];
            entry.dump(&mut buf, &dsn_list);
            let len = entry_vec.add_entry_bz(&EntryBz { bz: &buf });
            assert_eq!(len, i as usize);
        }

        let bytes = entry_vec.to_bytes();
        let new_entry_vec = EntryVec::from_bytes(&bytes);

        #[cfg(not(feature = "tee_cipher"))]
        assert_eq!(150224, new_entry_vec.total_bytes());
        #[cfg(feature = "tee_cipher")]
        assert_eq!(152672, new_entry_vec.total_bytes());

        let sns = vec![0, 61, 122];
        for i in sns {
            let e = new_entry_vec.get_entry(shard_id, i);
            assert_eq!(e.serial_number(), i as u64);
            assert_eq!(e.dsn_iter().collect::<Vec<_>>(), vec![(0, i as u64 + 1)]);
        }

        let enumerated: Vec<(usize, EntryBz)> = new_entry_vec.enumerate(shard_id).collect();
        assert_eq!(enumerated.len(), 140);

        let enumerated: Vec<(usize, EntryBz)> = new_entry_vec.enumerate(0).collect();
        assert_eq!(enumerated.len(), 0);
    }
}
