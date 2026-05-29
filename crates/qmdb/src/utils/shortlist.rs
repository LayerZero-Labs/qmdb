/// A space-efficient list implementation that optimizes storage for small lists.
///
/// # Design
///
/// The `ShortList` is designed to be memory efficient for small lists (up to 2 elements) by using a fixed-size array,
/// and automatically switches to a heap-allocated vector for larger lists. This hybrid approach provides:
///
/// - Optimal memory usage for small lists by avoiding heap allocation
/// - Efficient storage of up to 2 64-bit integers in a compact array format
/// - Automatic expansion to a vector when more than 2 elements are needed
/// - Prevention of duplicate elements
///
/// # Implementation Details
///
/// The implementation uses two variants:
/// - `Array`: A fixed-size array of 14 bytes that can store up to 2 64-bit integers
/// - `Vector`: A heap-allocated vector for storing more than 2 elements
///
/// The `Array` variant uses a clever byte packing technique to store two 64-bit integers
/// in 14 bytes by sharing overlapping bytes between the two values.
use byteorder::{BigEndian, ByteOrder, LittleEndian};

/// A space-efficient list that can store unique 64-bit integers.
///
/// The list automatically switches between an array-based implementation for small lists (≤2 elements)
/// and a vector-based implementation for larger lists.
#[derive(Clone, Debug)]
pub enum ShortList {
    /// Array-based storage for up to 2 elements
    Array {
        /// Number of elements currently stored (0-2)
        size: u8,
        /// Fixed-size array storing the elements in a compact format
        arr: [u8; 14],
    },
    /// Vector-based storage for more than 2 elements
    Vector(Box<Vec<i64>>),
}

impl Default for ShortList {
    fn default() -> Self {
        Self::new()
    }
}

impl ShortList {
    /// Creates a new empty `ShortList`.
    ///
    /// The list is initialized in array mode with no elements.
    pub fn new() -> Self {
        Self::Array {
            size: 0,
            arr: [0u8; 14],
        }
    }

    /// Creates a new `ShortList` containing a single element.
    ///
    /// # Arguments
    ///
    /// * `elem` - The element to initialize the list with.
    pub fn from(elem: i64) -> Self {
        let mut res = Self::Array {
            size: 0,
            arr: [0u8; 14],
        };
        res.append(elem);
        res
    }

    /// Checks if the list contains a specific element.
    ///
    /// # Arguments
    ///
    /// * `elem` - The element to search for.
    ///
    /// # Returns
    ///
    /// * `true` if the element is found in the list, `false` otherwise.
    pub fn contains(&self, elem: i64) -> bool {
        for (_, v) in self.enumerate() {
            if v == elem {
                return true;
            }
        }
        false
    }

    /// Returns the number of elements in the list.
    ///
    /// # Returns
    ///
    /// * The number of elements currently stored in the list.
    pub fn len(&self) -> usize {
        match self {
            Self::Array { size, arr: _ } => *size as usize,
            Self::Vector(v) => v.len(),
        }
    }

    /// Retrieves the element at the specified index.
    ///
    /// # Arguments
    ///
    /// * `idx` - The index of the element to retrieve.
    ///
    /// # Returns
    ///
    /// * The element at the specified index.
    ///
    /// # Panics
    ///
    /// * If the index is out of bounds.
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

    /// Appends a new element to the list if it's not already present.
    ///
    /// If the list is in array mode and adding the element would exceed
    /// the array capacity (2 elements), it automatically converts to vector mode.
    ///
    /// # Arguments
    ///
    /// * `elem` - The element to append.
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

    /// Removes all elements from the list.
    ///
    /// After calling this method, the list will be empty but retain its
    /// current storage mode (Array or Vector).
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

    /// Returns an iterator over the list's elements.
    ///
    /// # Returns
    ///
    /// * An iterator that yields pairs of (index, value) for each element.
    pub fn enumerate(&self) -> ShortListIter {
        ShortListIter { l: self, idx: 0 }
    }

    /// Checks if the list is empty.
    ///
    /// # Returns
    ///
    /// * `true` if the list contains no elements, `false` otherwise.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// An iterator over the elements of a `ShortList`.
///
/// This iterator yields pairs of (index, value) for each element in the list.
pub struct ShortListIter<'a> {
    /// Reference to the list being iterated
    l: &'a ShortList,
    /// Current iteration position
    idx: usize,
}

impl Iterator for ShortListIter<'_> {
    type Item = (usize, i64);

    /// Advances the iterator and returns the next element.
    ///
    /// # Returns
    ///
    /// * `Some((index, value))` if there are more elements
    /// * `None` if iteration is complete
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
