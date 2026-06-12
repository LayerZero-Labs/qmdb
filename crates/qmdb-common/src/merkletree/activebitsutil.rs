/// Utility functions for manipulating active bits using u64 operations.
/// Works with 4-element u64 slices (256 bits total).
pub struct ActiveBitsUtil;

impl ActiveBitsUtil {
    /// Sets a bit to active (1) at the specified offset within a 4-element u64 slice
    #[inline]
    pub fn set_bit(bits: &mut [u64], offset: u32) {
        debug_assert!(bits.len() == 4);
        debug_assert!(offset < 256);
        let word_idx = (offset / 64) as usize;
        let bit_idx = offset & 63;
        let mask = 1u64 << bit_idx;
        let word = &mut bits[word_idx];
        assert!(*word & mask == 0);
        *word |= mask;
    }

    /// Clears a bit to inactive (0) at the specified offset within a 4-element u64 slice
    #[inline]
    pub fn clear_bit(bits: &mut [u64], offset: u32) {
        debug_assert!(bits.len() == 4);
        debug_assert!(offset < 256);
        let word_idx = (offset / 64) as usize;
        let bit_idx = offset & 63;
        let mask = 1u64 << bit_idx;
        let word = &mut bits[word_idx];
        assert!(*word & mask != 0);
        *word &= !mask;
    }

    /// Gets the active status of a bit at the specified offset within a 4-element u64 slice
    #[inline]
    pub fn get_bit(bits: &[u64], offset: u32) -> bool {
        debug_assert!(bits.len() == 4);
        debug_assert!(offset < 256);
        let word_idx = (offset / 64) as usize;
        let bit_idx = offset & 63;
        (bits[word_idx] >> bit_idx) & 1 != 0
    }

    /// Finds the first active bit in a 4-element u64 slice (256 bits)
    #[inline]
    pub fn find_first_active(bits: &[u64]) -> Option<usize> {
        debug_assert!(bits.len() == 4);
        for (i, &word) in bits.iter().enumerate() {
            if word != 0 {
                let idx = word.trailing_zeros() as usize;
                return Some(i * 64 + idx);
            }
        }
        None
    }
}
