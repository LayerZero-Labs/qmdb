use std::collections::HashMap;

const SHIFT: i64 = 6; // Number of bits to represent positions within a 64-bit u64 value (2^6 = 64)
const MASK: i64 = (1 << SHIFT) - 1; // Mask to extract the lower 6 bits of an integer

/// Generates a bitmask with a single bit set at the given position.
#[inline]
fn bitmask(position: i64) -> u64 {
    1u64 << position
}

/// Extracts the high and low parts of the integer `value`.
///
/// # Arguments
///
/// * `value` - The integer to extract parts from.
///
/// # Returns
///
/// * A tuple containing the high and low parts of the integer.
#[inline]
fn into_parts(value: i64) -> (i64, i64) {
    let low = value & MASK;
    let high = value >> SHIFT;
    (high, low)
}

/// Checks if a specific bit is set in a `u64` value.
///
/// # Arguments
///
/// * `value` - The `u64` value to check.
/// * `position` - The bit position to check.
///
/// # Returns
///
/// * `true` if the bit at `position` is set, `false` otherwise.
#[inline]
fn is_bit_set(value: u64, position: i64) -> bool {
    (value & bitmask(position)) != 0
}

/// Sets a specific bit in a `u64` value.
///
/// # Arguments
///
/// * `value` - The `u64` value to modify.
/// * `position` - The bit position to set.
///
/// # Returns
///
/// * The modified `u64` value with the bit at `position` set.
#[inline]
fn set_bit(value: u64, position: i64) -> u64 {
    value | bitmask(position)
}

/// Unsets a specific bit in a `u64` value.
///
/// # Arguments
///
/// * `value` - The `u64` value to modify.
/// * `position` - The bit position to unset.
///
/// # Returns
///
/// * The modified `u64` value with the bit at `position` unset.
#[inline]
fn unset_bit(value: u64, position: i64) -> u64 {
    value & !bitmask(position)
}

/// A space-efficient set for storing integers using bit manipulation.
///
/// # Design
///
/// The `IntSet` struct is designed to be a space-efficient set for storing integers using bit manipulation.
/// Each integer is divided into two parts: the lower 6 bits and the remaining higher bits. The lower 6 bits
/// determine the position of the bit within a 64-bit `u64` value, and the higher bits are used as the key in
/// the `HashMap`.
///
/// # Space Efficiency
///
/// This approach reduces the space required to store the set, especially when the values are densely packed
/// within the range of 64 values. For example, values 0 to 63 will be stored in the same `u64` entry with the
/// key `0` in the `HashMap`, and values 64 to 127 will be stored in the same `u64` entry with the key `1` in
/// the `HashMap`.
///
/// By using bit manipulation and a `HashMap`, the `IntSet` struct provides an efficient way to store and manage
/// a set of integers with reduced memory usage. This design can reduce the memory usage to approximately 1/64th
/// of the space required by a traditional set implementation, as each `u64` entry can represent 64 different
/// integers.
///
/// # Example
///
/// ```
/// use qmdb::utils::intset::IntSet;
/// let mut set = IntSet::default();
/// set.add(10);
/// assert!(set.has(10));
/// set.remove(10);
/// assert!(!set.has(10));
/// ```
#[derive(Default)]
pub struct IntSet {
    data: HashMap<i64, u64>,
}

impl IntSet {
    /// Checks if the set contains the integer `value`.
    ///
    /// # Arguments
    ///
    /// * `value` - The integer to check for presence in the set.
    ///
    /// # Returns
    ///
    /// * `true` if the integer is present in the set, `false` otherwise.
    pub fn has(&self, value: i64) -> bool {
        let (high, low) = into_parts(value);
        if let Some(v) = self.data.get(&high) {
            return is_bit_set(*v, low);
        }
        false
    }

    /// Adds the integer `value` to the set.
    ///
    /// # Arguments
    ///
    /// * `value` - The integer to add to the set.
    ///
    /// # Returns
    ///
    /// * `true` if the value was added to the set, `false` if the value was already present.
    pub fn add(&mut self, value: i64) -> bool {
        let (high, low) = into_parts(value);
        if let Some(v) = self.data.get_mut(&high) {
            let old_v = *v;
            *v = set_bit(old_v, low);
            return *v != old_v;
        }
        self.data.insert(high, bitmask(low));
        true
    }

    /// Removes the integer `value` from the set.
    ///
    /// # Arguments
    ///
    /// * `value` - The integer to remove from the set.
    ///
    /// # Returns
    ///
    /// * `true` if the integer was present in the set and removed, `false` otherwise.
    pub fn remove(&mut self, value: i64) -> bool {
        let (high, low) = into_parts(value);
        let mut delete_it = false;
        if let Some(v) = self.data.get_mut(&high) {
            let old_v = *v;
            *v = unset_bit(old_v, low);
            if *v == 0 {
                delete_it = true;
            } else {
                return *v != old_v;
            }
        }
        if !delete_it {
            return false;
        }
        self.data.remove(&high);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_has() {
        let mut set = IntSet::default();
        // Test adding and checking presence of an element
        assert!(!set.has(10));
        assert!(set.add(10));
        assert!(set.has(10));
        // Test adding the same element again
        assert!(!set.add(10)); // Adding the same element should return false
    }

    #[test]
    fn test_remove() {
        let mut set = IntSet::default();
        // Test removing an element
        set.add(10);
        assert!(set.has(10));
        assert!(set.remove(10));
        assert!(!set.has(10));
        // Test removing a non-existent element
        assert!(!set.remove(10)); // Removing a non-existent element should return false
    }

    #[test]
    fn test_edge_cases() {
        let mut set = IntSet::default();
        // Test adding and removing edge case values
        assert!(set.add(0));
        assert!(set.has(0));
        assert!(set.add(63));
        assert!(set.has(63));
        assert!(set.add(64));
        assert!(set.has(64));
        assert!(set.add(127));
        assert!(set.has(127));
        assert!(set.add(128));
        assert!(set.has(128));
        assert!(set.remove(0));
        assert!(!set.has(0));
        assert!(set.remove(63));
        assert!(!set.has(63));
        assert!(set.remove(64));
        assert!(!set.has(64));
        assert!(set.remove(127));
        assert!(!set.has(127));
        assert!(set.remove(128));
        assert!(!set.has(128));
    }

    #[test]
    fn test_large_numbers() {
        let mut set = IntSet::default();
        let mut large_number = 1 << 30;
        // Test adding and removing a large number
        assert!(set.add(large_number));
        assert!(set.has(large_number));
        assert!(set.remove(large_number));
        assert!(!set.has(large_number));

        large_number = i64::MIN;
        assert!(set.add(large_number));
        assert!(set.has(large_number));
        assert!(set.remove(large_number));
        assert!(!set.has(large_number));
    }
}
