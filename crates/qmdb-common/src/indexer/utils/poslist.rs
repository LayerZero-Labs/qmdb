use alloc::vec::Vec;

#[derive(Debug)]
pub enum PosList {
    Array { size: usize, arr: [i64; 4] },
    Vector(Vec<i64>),
}

impl Default for PosList {
    fn default() -> Self {
        Self::Array {
            size: 0,
            arr: [0i64; 4],
        }
    }
}

impl PosList {
    pub fn contains(&self, elem: i64) -> bool {
        self.enumerate().any(|v| v == elem)
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Array { size, .. } => *size,
            Self::Vector(v) => v.len(),
        }
    }

    pub fn get(&self, idx: usize) -> Option<i64> {
        match self {
            Self::Array { size, arr } => {
                if idx < *size {
                    Some(arr[idx])
                } else {
                    None
                }
            }
            Self::Vector(v) => v.get(idx).copied(),
        }
    }

    pub fn append(&mut self, elem: i64) {
        match self {
            Self::Array { size, arr } => {
                if *size < arr.len() {
                    arr[*size] = elem;
                    *size += 1;
                } else {
                    let mut v = arr.to_vec();
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
            Self::Array { size, .. } => {
                *size = 0;
            }
            Self::Vector(v) => {
                v.clear();
            }
        }
    }

    pub fn enumerate(&self) -> PosListIter {
        PosListIter { l: self, idx: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub struct PosListIter<'a> {
    l: &'a PosList,
    idx: usize,
}

impl Iterator for PosListIter<'_> {
    type Item = i64;

    fn next(&mut self) -> Option<Self::Item> {
        let e = self.l.get(self.idx);
        if let Some(e) = e {
            self.idx += 1;
            return Some(e);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::vec;

    #[test]
    fn test_default() {
        let pos_list = PosList::default();
        assert!(matches!(pos_list, PosList::Array { size, arr } if size == 0 && arr == [0i64; 4]));
    }

    #[test]
    fn test_append_to_array() {
        let mut pos_list = PosList::default();
        pos_list.append(1);
        pos_list.append(2);
        pos_list.append(3);
        pos_list.append(4);
        assert!(
            matches!(pos_list, PosList::Array { size, arr } if size == 4 && arr == [1, 2, 3, 4])
        );
    }

    #[test]
    fn test_append_to_vector() {
        let mut pos_list = PosList::default();
        for i in 1..=5 {
            pos_list.append(i);
        }
        assert!(matches!(pos_list, PosList::Vector(v) if v == vec![1, 2, 3, 4, 5]));
    }

    #[test]
    fn test_contains() {
        let mut pos_list = PosList::default();
        pos_list.append(1);
        pos_list.append(2);
        assert!(pos_list.contains(1));
        assert!(!pos_list.contains(3));
    }

    #[test]
    fn test_len() {
        let mut pos_list = PosList::default();
        assert_eq!(pos_list.len(), 0);
        pos_list.append(1);
        assert_eq!(pos_list.len(), 1);
    }

    #[test]
    fn test_get() {
        let mut pos_list = PosList::default();
        pos_list.append(1);
        pos_list.append(2);
        assert_eq!(pos_list.get(0), Some(1));
        assert_eq!(pos_list.get(1), Some(2));
        assert_eq!(pos_list.get(2), None);
    }

    #[test]
    fn test_clear() {
        let mut pos_list = PosList::default();
        pos_list.append(1);
        pos_list.append(2);
        pos_list.clear();
        assert_eq!(pos_list.len(), 0);
    }

    #[test]
    fn test_is_empty() {
        let pos_list = PosList::default();
        assert!(pos_list.is_empty());
    }

    #[test]
    fn test_enumerate() {
        let mut pos_list = PosList::default();
        pos_list.append(1);
        pos_list.append(2);
        let mut iter = pos_list.enumerate();
        assert_eq!(iter.next(), Some(1));
        assert_eq!(iter.next(), Some(2));
        assert_eq!(iter.next(), None);

        pos_list.append(3);
        pos_list.append(4);
        pos_list.append(5);

        let mut iter = pos_list.enumerate();
        assert_eq!(iter.next(), Some(1));
        assert_eq!(iter.next(), Some(2));
        assert_eq!(iter.next(), Some(3));
        assert_eq!(iter.next(), Some(4));
        assert_eq!(iter.next(), Some(5));
        assert_eq!(iter.next(), None);
    }
}
