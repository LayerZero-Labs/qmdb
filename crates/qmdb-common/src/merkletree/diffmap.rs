//! Diff-based compressed array storage.
//!
//! This module provides `DiffMap`, a space-efficient data structure for storing arrays of 32-byte
//! values (typically hashes) where consecutive elements often have the same value. Instead of
//! storing every element, it only stores the points where values change.
//!
//! # Use Cases
//!
//! DiffMap is only used for NULL_MT_FOR_TWIG to reduce memory usage.
//!
//! # Format
//!
//! The diff format is a sorted array of `(index, value)` pairs where:
//! - `index` is the position where a new value starts
//! - `value` is the 32-byte hash that applies from that index until the next change
//!
//! # Example
//!
//! ```
//! // Original array: [A, A, A, B, B, C, C, C, C]
//! // Diff format:   [(0, A), (3, B), (5, C)]
//! // Meaning: A from index 0-2, B from index 3-4, C from index 5-8
//! ```

use alloc::boxed::Box;
use alloc::vec::Vec;

/// A space-efficient representation of an array of 32-byte values.
///
/// `DiffMap` stores only the points where values change in a sorted array, rather than
/// storing every element. This provides significant memory savings when the array contains
/// runs of identical values, which is common in QMDB's right-most uninitialized tree.
///
/// The underlying storage is a slice of `(index, value)` pairs sorted by index.
#[derive(Debug)]
pub struct DiffMap<'a> {
    /// The diff data as sorted `(index, value)` pairs.
    diff: &'a [(usize, [u8; 32])],
    /// The logical length of the represented array.
    len: usize,
}

impl<'a> DiffMap<'a> {
    /// Creates a new `DiffMap` from pre-computed diff data.
    ///
    /// # Arguments
    ///
    /// * `diff` - A slice of `(index, value)` pairs sorted by index. Each pair represents
    ///            a point where the value changes in the logical array.
    /// * `len` - The logical length of the array being represented.
    ///
    /// # Returns
    ///
    /// A new `DiffMap` instance.
    ///
    /// # Panics
    ///
    /// Panics if the diff slice is not sorted by index in ascending order.
    ///
    /// # Examples
    ///
    /// ```
    /// use qmdb_common::merkletree::diffmap::DiffMap;
    /// let diff = [(0, [1u8; 32]), (3, [2u8; 32])];
    /// let diffmap = DiffMap::from_diff(&diff, 5);
    /// // Represents: [1,1,1, 1,1,1, 1,1,1, 2,2,2, 2,2,2]
    /// //             [    0,     1,     2,     3,     4]
    /// ```
    pub fn from_diff(diff: &'a [(usize, [u8; 32])], len: usize) -> Self {
        assert!(
            diff.windows(2).all(|w| w[0].0 <= w[1].0),
            "Diff must be sorted by index"
        );
        Self { diff, len }
    }

    /// Gets the value at the specified index in the logical array.
    ///
    /// This method uses binary search to efficiently find the most recent change point
    /// before or at the requested index, then returns that value.
    ///
    /// # Arguments
    ///
    /// * `idx` - The index in the logical array to retrieve.
    ///
    /// # Returns
    ///
    /// A reference to the 32-byte value at the specified index.
    ///
    /// # Panics
    ///
    /// Panics if:
    /// - `idx` is >= the logical array length
    /// - No base value is found for the index (empty diff)
    ///
    /// # Examples
    ///
    /// ```
    /// use qmdb_common::merkletree::diffmap::DiffMap;
    /// let diff = [(0, [1u8; 32]), (3, [2u8; 32])];
    /// let diffmap = DiffMap::from_diff(&diff, 5);
    /// assert_eq!(diffmap.get(0), &[1u8; 32]); // First segment
    /// assert_eq!(diffmap.get(2), &[1u8; 32]); // Still first segment
    /// assert_eq!(diffmap.get(3), &[2u8; 32]); // Second segment starts
    /// assert_eq!(diffmap.get(4), &[2u8; 32]); // Still second segment
    /// ```
    pub fn get(&self, idx: usize) -> &[u8; 32] {
        assert!(idx < self.len, "Index out of bounds");
        let pos = self
            .diff
            .binary_search_by_key(&idx, |&(i, _)| i)
            .unwrap_or_else(|insert_point| {
                if insert_point == 0 {
                    panic!("No base value found for index {}", idx);
                }
                insert_point - 1
            });
        &self.diff[pos].1
    }

    /// Reconstructs the full array from the diff representation.
    ///
    /// This method expands the compressed diff format back into a full array
    /// containing all elements. Each position gets the value from the most
    /// recent change point before or at that position.
    ///
    /// # Returns
    ///
    /// A boxed slice containing the full reconstructed array.
    ///
    /// # Panics
    ///
    /// Panics if the diff is empty (no base value to fill with).
    ///
    /// # Examples
    ///
    /// ```
    /// use qmdb_common::merkletree::diffmap::DiffMap;
    /// let diff = [(0, [1u8; 32]), (3, [2u8; 32])];
    /// let diffmap = DiffMap::from_diff(&diff, 5);
    /// let full_array = diffmap.to_vec();
    /// assert_eq!(full_array.len(), 5);
    /// assert_eq!(full_array[0], [1u8; 32]);
    /// assert_eq!(full_array[1], [1u8; 32]);
    /// assert_eq!(full_array[2], [1u8; 32]);
    /// assert_eq!(full_array[3], [2u8; 32]);
    /// assert_eq!(full_array[4], [2u8; 32]);
    /// ```
    pub fn to_vec(&self) -> Box<[[u8; 32]]> {
        if self.diff.is_empty() {
            panic!("Empty diff, cannot fill");
        }

        let mut data = Vec::with_capacity(self.len);

        let mut diff_iter = self.diff.iter().peekable();
        let mut cur_value = &diff_iter.next().unwrap().1;
        let mut next_change = diff_iter.peek().map(|(idx, _)| *idx);

        for i in 0..self.len {
            if next_change == Some(i) {
                let (_, v) = diff_iter.next().unwrap();
                cur_value = v;
                next_change = diff_iter.peek().map(|(idx, _)| *idx);
            }
            data.push(*cur_value);
        }

        data.into_boxed_slice()
    }

    /// Generates a diff representation from a full array of values.
    ///
    /// This method analyzes the input array and creates a compressed diff format
    /// by identifying runs of identical values and storing only the change points.
    ///
    /// # Arguments
    ///
    /// * `data` - A slice of 32-byte values to compress into diff format.
    ///
    /// # Returns
    ///
    /// A vector of `(index, value)` pairs representing the diff. Each pair
    /// indicates where a new value starts in the array.
    ///
    /// # Examples
    ///
    /// ```
    /// use qmdb_common::merkletree::diffmap::DiffMap;
    /// let data = [[1u8; 32], [1u8; 32], [2u8; 32], [2u8; 32], [3u8; 32]];
    /// let diff = DiffMap::generate_diff(&data);
    /// // Results in: [(0, [1u8; 32]), (2, [2u8; 32]), (4, [3u8; 32])]
    /// assert_eq!(diff.len(), 3);
    /// assert_eq!(diff[0].0, 0); // First value starts at index 0
    /// assert_eq!(diff[1].0, 2); // Second value starts at index 2
    /// assert_eq!(diff[2].0, 4); // Third value starts at index 4
    /// ```
    pub fn generate_diff(data: &[[u8; 32]]) -> Vec<(usize, [u8; 32])> {
        if data.is_empty() {
            return Vec::new();
        }

        let mut diff = Vec::with_capacity(data.len());
        let mut last = &data[0];
        diff.push((0, *last));

        for (i, val) in data.iter().enumerate().skip(1) {
            if val != last {
                diff.push((i, *val));
                last = val;
            }
        }

        diff
    }
}

#[cfg(test)]
mod tests {
    use crate::merkletree::diffmap::DiffMap;

    #[test]
    fn test_diff_map() {
        let data = [[1u8; 32], [1u8; 32], [2u8; 32], [2u8; 32], [3u8; 32]];
        let diff = DiffMap::generate_diff(&data);
        assert_eq!(diff.len(), 3);
        assert_eq!(diff[0].0, 0);
        assert_eq!(diff[0].1, [1u8; 32]);
        assert_eq!(diff[1].0, 2);
        assert_eq!(diff[1].1, [2u8; 32]);
        assert_eq!(diff[2].0, 4);
        assert_eq!(diff[2].1, [3u8; 32]);

        let diff_map = DiffMap::from_diff(&diff, data.len());
        let out = diff_map.to_vec();

        assert_eq!(out[0], [1u8; 32]);
        assert_eq!(out[1], [1u8; 32]);
        assert_eq!(out[2], [2u8; 32]);
        assert_eq!(out[3], [2u8; 32]);
        assert_eq!(out[4], [3u8; 32]);

        assert!(diff_map.get(0) == &data[0]);
        assert!(diff_map.get(1) == &data[1]);
        assert!(diff_map.get(2) == &data[2]);
        assert!(diff_map.get(3) == &data[3]);
        assert!(diff_map.get(4) == &data[4]);
    }
}
