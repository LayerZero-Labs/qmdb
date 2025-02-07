use byteorder::{BigEndian, ByteOrder, LittleEndian};

#[derive(Clone, Debug)]
pub enum ShortList {
    Array { size: u8, arr: [u8; 14] },
    Vector(Box<Vec<i64>>),
}

impl Default for ShortList {
    fn default() -> Self {
        Self::new()
    }
}

impl ShortList {
    pub fn new() -> Self {
        Self::Array {
            size: 0,
            arr: [0u8; 14],
        }
    }

    pub fn from(elem: i64) -> Self {
        let mut res = Self::Array {
            size: 0,
            arr: [0u8; 14],
        };
        res.append(elem);
        res
    }

    pub fn contains(&self, elem: i64) -> bool {
        for (_, v) in self.enumerate() {
            if v == elem {
                return true;
            }
        }
        false
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Array { size, arr: _ } => *size as usize,
            Self::Vector(v) => v.len(),
        }
    }

    pub fn get(&self, idx: usize) -> i64 {
        match self {
            Self::Array { size: _, arr } => {
                if idx == 0 {
                    (LittleEndian::read_u64(&arr[..8]) << 8 >> 8) as i64 // clear high 8 bits
                } else {
                    (BigEndian::read_u64(&arr[6..]) << 8 >> 8) as i64 // clear high 8 bits
                }
            }
            Self::Vector(v) => v[idx],
        }
    }

    pub fn append(&mut self, elem: i64) {
        for i in 0..self.len() {
            if elem == self.get(i) {
                return; //no duplication
            }
        }
        match self {
            Self::Array { size, arr } => {
                if 0 == *size {
                    LittleEndian::write_i64(&mut arr[..8], elem);
                    *size += 1;
                } else if 1 == *size {
                    let byte = arr[6];
                    BigEndian::write_i64(&mut arr[6..], elem);
                    arr[6] = byte;
                    *size += 1;
                } else {
                    let mut v = Box::new(vec![self.get(0), self.get(1)]);
                    v.push(elem);
                    *self = Self::Vector(v);
                }
            }
            Self::Vector(v) => {
                v.push(elem);
            }
        }
    }

    pub fn clear(&mut self) {
        match self {
            Self::Array { size, arr: _ } => {
                *size = 0;
            }
            Self::Vector(v) => {
                v.clear();
            }
        }
    }

    pub fn enumerate(&self) -> ShortListIter {
        ShortListIter { l: self, idx: 0 }
    }
}

pub struct ShortListIter<'a> {
    l: &'a ShortList,
    idx: usize,
}

impl Iterator for ShortListIter<'_> {
    type Item = (usize, i64);

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx >= self.l.len() {
            return None;
        }
        let e = self.l.get(self.idx);
        let idx = self.idx;
        self.idx += 1;
        Some((idx, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shortlist() {
        let mut shortlist = ShortList::new();

        shortlist.append(0x0101fffffffffff1);
        shortlist.append(0x0101fffffffffff2);
        shortlist.append(0x0001fffffffffff2);

        assert!(shortlist.len() == 2);
        assert!(shortlist.get(0) == 0x0001fffffffffff1);
        assert!(shortlist.get(1) == 0x0001fffffffffff2);
        assert!(shortlist.contains(0x0001fffffffffff1));
        assert!(shortlist.contains(0x0001fffffffffff2));
        assert!(!shortlist.contains(20));

        shortlist.append(15);
        shortlist.append(16);
        shortlist.append(15);

        assert!(shortlist.len() == 4);
        assert!(shortlist.get(0) == 0x0001fffffffffff1);
        assert!(shortlist.get(1) == 0x0001fffffffffff2);
        assert!(shortlist.get(2) == 15);
        assert!(shortlist.get(3) == 16);
        assert!(shortlist.contains(0x0001fffffffffff1));
        assert!(shortlist.contains(0x0001fffffffffff2));
        assert!(shortlist.contains(15));
        assert!(shortlist.contains(16));
        assert!(!shortlist.contains(20));
    }

    #[test]
    fn test_shortlist_clear() {
        let mut shortlist = ShortList::new();
        shortlist.append(5);

        shortlist.clear();

        assert_eq!(shortlist.len(), 0);
    }
}
