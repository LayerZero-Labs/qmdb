use byteorder::{BigEndian, ByteOrder};
use std::cmp::Ordering;

const BINARY_SEARCH_THRES: u16 = 8;

// KVList has a sorted KV vector. The same key can have multiple values. Its sorting rule is:
// pairs are sorted in ascending order of keys and the values are not considered.
//
// A KVList is initialized by calling 'append' multiple times with a sorted sequence of
// KV pairs. This sequence must also follow its sorting rule.
//
// After initialization, new KV pairs cannot be inserted to or removed from it. You
// can only change the values of KV pairs.
//
// The 'elements' of KVList do not store the first byte of keys, i.e., the lead_byte.
//
// A KVList has 2**X 'ranges', where X >= 8; Some ranges are empty. The KV pairs from a
// non-empty range share the same lead_byte. Different ranges may or may not have the same
// lead_byte. We call X as the number of indexing bits.
//
// A KVCursor points to a KV pair in the KVList. You can get the KV pair at it, and move it
// forward or backward. The 'find' function returns the first KVCursor whose key is no less
// than a given key.
//
// Please note KVList does not support duplicated values. Every value store in it must be unique.
// This is reasonable because qmdb's Entry positions are all unique.
//

#[derive(Default)]
pub struct KVList<const N: usize, const N_PLUS1: usize, const VL: usize> {
    pub binary_search_thres: u16,
    pub shard_id: u16,
    // How many leading bits are used for indexing 'offsets'
    // It must be greater than or equal to 8
    indexing_bits: u8,
    // It contains 2**indexing_bits offsets of elements
    offsets: Vec<u32>,
    elements: Vec<[u8; N]>,
}

// The Range struct is private. We do not want to expose it.
#[derive(Default, Clone, Debug)]
struct Range {
    valid: bool,
    lead_byte: u8,
    start: usize,    //the starting position in elements
    end: usize,      //the ending position in elements
    curr_idx: usize, //the current range starts at offsets[curr_idx]
    next_idx: usize, //the next range starts at offsets[next_idx]
}

#[derive(Clone, Debug)]
pub struct KVCursor {
    idx: usize,
    range: Range,
}

impl KVCursor {
    fn new(idx: usize, range: Range) -> Self {
        Self { idx, range }
    }

    pub fn valid(&self) -> bool {
        self.idx != usize::MAX
    }

    pub fn index(&self) -> usize {
        self.idx
    }
}

impl<const N: usize, const N_PLUS1: usize, const VL: usize> KVList<N, N_PLUS1, VL> {
    pub fn new(size: usize) -> Self {
        let useful_bits = 64 - (size as u64).leading_zeros();
        let indexing_bits = if useful_bits < 13 { 8 } else { useful_bits - 5 };
        assert!(
            N + 1 == N_PLUS1,
            "Invalid generic parameters: N = {N}, N_PLUS1 = {N_PLUS1}"
        );
        assert!(VL < N, "Invalid generic parameters: VL = {VL}");
        let mut elements = Vec::with_capacity(size + 1);
        // the first entry is just a dummy
        elements.push([0u8; N]);
        Self {
            binary_search_thres: BINARY_SEARCH_THRES,
            shard_id: 0,
            indexing_bits: indexing_bits as u8,
            offsets: vec![0; 1 << indexing_bits],
            elements,
        }
    }
    pub fn print(&self) {
        println!(
            "== ibts: {}, off: {:?}, elem: {:?}",
            self.indexing_bits, self.offsets, self.elements
        );
    }

    pub fn is_empty(&self) -> bool {
        self.elements.len() == 1 //only the dummy
    }

    pub fn get_first_cursor(&self) -> KVCursor {
        let range = self.get_first_range();
        let idx = if range.valid { range.start } else { usize::MAX };
        KVCursor { idx, range }
    }

    pub fn get_last_cursor(&self) -> KVCursor {
        let range = self.get_last_range();
        let idx = if range.valid {
            range.end - 1
        } else {
            usize::MAX
        };
        KVCursor { idx, range }
    }

    fn get_first_range(&self) -> Range {
        self.get_range_from(0)
    }

    fn get_last_range(&self) -> Range {
        for i in (0..self.offsets.len()).rev() {
            if self.offsets[i] != 0 {
                return Range {
                    valid: true,
                    lead_byte: (i >> (self.indexing_bits - 8)) as u8,
                    start: self.offsets[i] as usize,
                    end: self.elements.len(),
                    curr_idx: i,
                    next_idx: self.offsets.len(),
                };
            }
        }
        Range::default() //return invalid Range
    }

    fn get_range_from(&self, mut curr_idx: usize) -> Range {
        let mut range = Range::default();
        while curr_idx < self.offsets.len() && self.offsets[curr_idx] == 0 {
            curr_idx += 1;
        }
        if curr_idx == self.offsets.len() {
            return range; // no valid range found
        }
        range.curr_idx = curr_idx;
        self.fill_range(&mut range);
        range
    }

    // Fill the 'start', 'end', 'lead_byte', 'next_idx' and 'valid' fields of Range.
    fn fill_range(&self, range: &mut Range) {
        range.start = self.offsets[range.curr_idx] as usize;
        range.lead_byte = (range.curr_idx >> (self.indexing_bits - 8)) as u8;
        range.valid = true;
        let mut next_idx = range.curr_idx + 1;
        while next_idx < self.offsets.len() && self.offsets[next_idx] == 0 {
            next_idx += 1;
        }
        if next_idx == self.offsets.len() {
            range.end = self.elements.len();
        } else {
            range.end = self.offsets[next_idx] as usize;
        }
        range.next_idx = next_idx;
    }

    fn move_range_forward(&self, range: &mut Range) {
        assert!(
            range.valid,
            "Cannot move an invalid range forward {range:?}"
        );
        if range.next_idx == self.offsets.len() {
            range.valid = false;
            return;
        }
        range.curr_idx = range.next_idx;
        self.fill_range(range);
    }

    fn move_range_backward(&self, range: &mut Range) {
        assert!(
            range.valid,
            "Cannot move an invalid range backward {range:?}"
        );
        range.next_idx = range.curr_idx;
        range.end = range.start;

        loop {
            if range.curr_idx == 0 {
                range.valid = false;
                return;
            }
            range.curr_idx -= 1;
            if self.offsets[range.curr_idx] != 0 {
                break;
            }
        }
        range.start = self.offsets[range.curr_idx] as usize;
        range.lead_byte = (range.curr_idx >> (self.indexing_bits - 8)) as u8;
    }

    pub fn at(&self, cursor: &KVCursor) -> [u8; N_PLUS1] {
        assert!(cursor.valid(), "Cannot access an invalid cursor {cursor:?}");
        let mut res = [0u8; N_PLUS1];
        res[0] = cursor.range.lead_byte;
        res[1..].copy_from_slice(&self.elements[cursor.idx][..]);
        res
    }

    pub fn change(&mut self, cursor: &KVCursor, new_value: &[u8; VL]) {
        assert!(cursor.valid(), "Cannot access an invalid cursor {cursor:?}");
        if !cursor.valid() {
            panic!("cannot access an invalid cursor");
        }
        let target = &mut self.elements[cursor.idx][..];
        target[N - VL..].copy_from_slice(&new_value[..]);
    }

    pub fn move_forward(&self, cursor: &mut KVCursor) {
        assert!(cursor.valid(), "Cannot access an invalid cursor {cursor:?}");
        cursor.idx += 1;
        if cursor.idx == self.elements.len() {
            cursor.idx = usize::MAX;
            return;
        }
        if cursor.idx == cursor.range.end {
            self.move_range_forward(&mut cursor.range);
        }
    }

    pub fn move_backward(&self, cursor: &mut KVCursor) {
        assert!(cursor.valid(), "Cannot access an invalid cursor {cursor:?}");
        if cursor.idx == 1 {
            // idx=0 is the dummy, we cannot move to it
            cursor.idx = usize::MAX;
            return;
        }
        if cursor.idx == cursor.range.start {
            self.move_range_backward(&mut cursor.range);
        }
        cursor.idx -= 1;
    }

    // Extract the leading 'indexing_bits' from 'elem'
    fn get_idx(&self, elem: &[u8; N_PLUS1]) -> usize {
        let idx32 = BigEndian::read_u32(&elem[..4]);
        (idx32 >> (32 - self.indexing_bits)) as usize
    }

    // Append a new element and updates 'offsets_idx'
    // When 'offsets_idx' changes, fill a offset of 'elements' into 'offsets'
    pub fn append(&mut self, offsets_idx: &mut usize, elem: &[u8; N_PLUS1]) {
        //println!("BeforeAppend");
        //self.print();
        let new_idx = self.get_idx(elem);
        //println!("II new_idx={} offsets_idx={}", new_idx, *offsets_idx);
        if *offsets_idx != new_idx {
            self.offsets[new_idx] = self.elements.len() as u32;
            *offsets_idx = new_idx;
        }
        let mut bz = [0u8; N];
        bz[..].copy_from_slice(&elem[1..]);
        self.elements.push(bz);
        //println!("AfterAppend");
        //self.print();
    }

    pub fn find(&self, key: &[u8; N_PLUS1]) -> KVCursor {
        let start_idx = self.get_idx(key);

        //println!("HH start_idx={}", start_idx);
        let mut range = self.get_range_from(start_idx);
        //println!("HH range={:?}", range);
        if !range.valid {
            return KVCursor::new(usize::MAX, range);
        }
        if range.curr_idx != start_idx {
            return KVCursor::new(range.start, range);
        }

        let mut k = [0u8; N];
        k[..].copy_from_slice(&key[1..]);
        let mut idx = range.start;
        if range.end - range.start > self.binary_search_thres as usize {
            // use binary_search to make idx closer
            let target = &self.elements[range.start..range.end];
            idx += target
                .binary_search_by(|probe| {
                    match &probe[..N - VL].cmp(&k[..N - VL]) {
                        Ordering::Less => Ordering::Less,
                        //no Equal here because we want lower bound
                        _ => Ordering::Greater,
                    }
                })
                .err()
                .unwrap(); // can only get Err(i) because no Equal here
        }
        //println!("HH0 idx={idx}");
        // Refine using only the key prefix (values are not considered in ordering).
        while idx < range.end && self.elements[idx][..N - VL] < k[..N - VL] {
            idx += 1;
        }
        //println!("HH1 idx={idx}");
        if idx == range.end {
            self.move_range_forward(&mut range);
            idx = if range.valid { range.start } else { usize::MAX };
        }
        //println!("HH2 idx={idx}");
        KVCursor { idx, range }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_range() {
        let mut list = KVList::<12, 13, 6>::default();
        list.indexing_bits = 8;
        list.elements.resize(10, [1u8; 12]);
        list.elements.resize(20, [3u8; 12]);
        list.elements.resize(21, [5u8; 12]);
        list.offsets.extend_from_slice(&[0, 4, 0, 9, 12, 0, 0, 18]);
        //                               0  1  2  3  4   5  6  7
        let mut range = list.get_first_range();
        assert!(range.valid);
        assert_eq!(1, range.lead_byte);
        assert_eq!(4, range.start);
        assert_eq!(9, range.end);
        list.move_range_forward(&mut range);
        assert_eq!(3, range.lead_byte);
        assert_eq!(9, range.start);
        assert_eq!(12, range.end);
        list.move_range_forward(&mut range);
        assert_eq!(4, range.lead_byte);
        assert_eq!(12, range.start);
        assert_eq!(18, range.end);
        list.move_range_backward(&mut range);
        assert_eq!(3, range.lead_byte);
        assert_eq!(9, range.start);
        assert_eq!(12, range.end);
        list.move_range_forward(&mut range);
        list.move_range_forward(&mut range);
        assert!(range.valid);
        assert_eq!(7, range.lead_byte);
        assert_eq!(18, range.start);
        assert_eq!(21, range.end);
        let mut invalid_range = range.clone();
        list.move_range_forward(&mut invalid_range);
        assert!(!invalid_range.valid);

        list.move_range_backward(&mut range);
        assert_eq!(4, range.lead_byte);
        assert_eq!(12, range.start);
        assert_eq!(18, range.end);
        list.move_range_backward(&mut range);
        assert_eq!(3, range.lead_byte);
        assert_eq!(9, range.start);
        assert_eq!(12, range.end);
        list.move_range_backward(&mut range);
        assert_eq!(1, range.lead_byte);
        assert_eq!(4, range.start);
        assert_eq!(9, range.end);
        list.move_range_backward(&mut range);
        assert!(!range.valid);
    }

    #[test]
    fn test_append() {
        let mut list = KVList::<3, 4, 1>::new(1 << 13);
        assert_eq!(9, list.indexing_bits);
        let elements: &[[u8; 4]] = &[
            [0x01, 0x00, 1, 1],
            [0x01, 0x8A, 2, 2],
            [0x04, 0x0B, 3, 3],
            [0x05, 0x0C, 4, 4],
            [0x09, 0x89, 5, 5],
            [0x0A, 0x80, 6, 6],
            [0x10, 0x07, 7, 7],
            [0x11, 0x80, 8, 8],
            [0x12, 0x00, 9, 9],
            [0x12, 0x80, 10, 10],
            [0x20, 0x80, 11, 11],
            [0x21, 0x00, 12, 12],
            [0x21, 0x00, 13, 200], //13
            [0x21, 0x00, 14, 201], //14
            [0x21, 0x00, 15, 202], //15
            [0x21, 0x00, 16, 203], //16
            [0x21, 0x00, 16, 204], //17
            [0x21, 0x00, 16, 205], //18
            [0x21, 0x00, 16, 206], //19
            [0x21, 0x00, 16, 207], //20
            [0x21, 0x00, 21, 208], //21
            [0x22, 0x80, 22, 22],
            [0x23, 0x00, 23, 23],
            [0x30, 0x80, 24, 24],
            [0x30, 0x80, 25, 210], //25
            [0x30, 0x80, 26, 211], //26
            [0x30, 0x80, 27, 212], //27
            [0x30, 0x80, 28, 213], //28
            [0x40, 0x80, 29, 29],
            [0x50, 0x80, 30, 30],
            [0x90, 0x00, 31, 31],
            [0xA0, 0x00, 32, 32],
            [0xFA, 0x80, 33, 33],
        ];
        let mut offsets_idx = usize::MAX;
        for e in elements {
            list.append(&mut offsets_idx, e);
        }
        for i in 0..(1 << list.indexing_bits) {
            match i * 8 {
                0x010 => assert_eq!(1, list.offsets[i]),
                0x018 => assert_eq!(2, list.offsets[i]),
                0x040 => assert_eq!(3, list.offsets[i]),
                0x050 => assert_eq!(4, list.offsets[i]),
                0x098 => assert_eq!(5, list.offsets[i]),
                0x0A8 => assert_eq!(6, list.offsets[i]),
                0x100 => assert_eq!(7, list.offsets[i]),
                0x118 => assert_eq!(8, list.offsets[i]),
                0x120 => assert_eq!(9, list.offsets[i]),
                0x128 => assert_eq!(10, list.offsets[i]),
                0x208 => assert_eq!(11, list.offsets[i]),
                0x210 => assert_eq!(12, list.offsets[i]),
                0x228 => assert_eq!(22, list.offsets[i]),
                0x230 => assert_eq!(23, list.offsets[i]),
                0x308 => assert_eq!(24, list.offsets[i]),
                0x408 => assert_eq!(29, list.offsets[i]),
                0x508 => assert_eq!(30, list.offsets[i]),
                0x900 => assert_eq!(31, list.offsets[i]),
                0xA00 => assert_eq!(32, list.offsets[i]),
                0xFA8 => assert_eq!(33, list.offsets[i]),
                _ => assert_eq!(0, list.offsets[i]),
            }
        }

        let mut cursor = list.get_first_cursor();
        for e in elements {
            assert_eq!(*e, list.at(&cursor));
            list.change(&cursor, &[100u8; 1]);
            list.move_forward(&mut cursor);
        }
        assert!(!cursor.valid());
        cursor = list.get_first_cursor();
        list.move_backward(&mut cursor);
        assert!(!cursor.valid());

        cursor = list.get_last_cursor();
        for i in (0..elements.len()).rev() {
            let mut e = elements[i];
            e[3] = 100;
            assert_eq!(e, list.at(&cursor));
            list.move_backward(&mut cursor);
        }
        assert!(!cursor.valid());

        cursor = list.get_first_cursor();
        for e in elements {
            let mut e = *e;
            e[3] = 100;
            assert_eq!(e, list.at(&cursor));
            list.move_forward(&mut cursor);
        }
        list.binary_search_thres = 6;
        assert_eq!(1, list.find(&[0x00, 0x00, 0, 0]).index());
        assert_eq!(1, list.find(&[0x01, 0x00, 1, 0]).index());
        assert_eq!(9, list.find(&[0x12, 0x00, 9, 0]).index());
        assert_eq!(12, list.find(&[0x21, 0x00, 12, 0]).index());
        assert_eq!(13, list.find(&[0x21, 0x00, 13, 0]).index());
        assert_eq!(14, list.find(&[0x21, 0x00, 14, 0]).index());
        assert_eq!(15, list.find(&[0x21, 0x00, 15, 0]).index());
        assert_eq!(16, list.find(&[0x21, 0x00, 16, 0]).index());
        assert_eq!(21, list.find(&[0x21, 0x00, 21, 0]).index());
        assert_eq!(22, list.find(&[0x21, 0x00, 22, 0]).index());
        assert_eq!(24, list.find(&[0x30, 0x80, 24, 0]).index());
        assert_eq!(25, list.find(&[0x30, 0x80, 25, 0]).index());
        assert_eq!(26, list.find(&[0x30, 0x80, 26, 0]).index());
        assert_eq!(27, list.find(&[0x30, 0x80, 27, 0]).index());
        assert_eq!(28, list.find(&[0x30, 0x80, 28, 0]).index());
        assert_eq!(29, list.find(&[0x30, 0x80, 29, 0]).index());
        assert_eq!(29, list.find(&[0x31, 0x80, 29, 0]).index());
        assert_eq!(33, list.find(&[0xA1, 0x00, 0, 0]).index());
        assert_eq!(33, list.find(&[0xFA, 0x80, 33, 0]).index());
        assert!(!list.find(&[0xFA, 0x80, 34, 0]).valid());
        assert!(!list.find(&[0xFB, 0x80, 33, 0]).valid());

        let k7 = &[0x12, 0x34, 0x56, 0];
        list.indexing_bits = 8;
        let mut idx = list.get_idx(k7);
        assert_eq!(0x12, idx);
        list.indexing_bits = 12;
        idx = list.get_idx(k7);
        assert_eq!(0x123, idx);
        list.indexing_bits = 20;
        idx = list.get_idx(k7);
        assert_eq!(0x12345, idx);
    }

    #[test]
    fn test_find_ignores_value_ordering() {
        let mut list = KVList::<3, 4, 1>::new(16);
        let mut offsets_idx = usize::MAX;
        let entries = [
            [0x10, 0x00, 0xAA, 0x01], // value = 0x01
            [0x10, 0x00, 0xAA, 0xF0], // same key prefix, larger value
        ];
        for e in &entries {
            list.append(&mut offsets_idx, e);
        }
        // Even if we search with the second entry (different value),
        // find() must return the first element with the same key prefix.
        let cursor = list.find(&entries[1]);
        assert!(cursor.valid());
        assert_eq!(entries[0], list.at(&cursor));
    }
}
