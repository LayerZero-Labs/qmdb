use crate::def::LEAF_COUNT_IN_TWIG;

/// Manages the active/inactive status of entries within a twig
///
/// This structure maintains a 32-element u64 array (256 bytes / 2048 bits) where each bit represents
/// whether a corresponding entry is active (1) or inactive (0).
/// Using [u64; 32] for better alignment and faster bit operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveBits(pub [u64; 32]);

impl Default for ActiveBits {
    fn default() -> Self {
        Self([0; 32])
    }
}

impl ActiveBits {
    /// Sets a bit to active (1) at the specified offset
    ///
    /// # Arguments
    /// * `offset` - The bit position to set (0-2047)
    ///
    /// # Panics
    /// Panics if offset is greater than LEAF_COUNT_IN_TWIG
    pub fn set_bit(&mut self, offset: u32) {
        if offset > LEAF_COUNT_IN_TWIG {
            panic!("Invalid ID");
        }
        let word_idx = (offset / 64) as usize;
        let bit_idx = offset & 63;
        let mask = 1u64 << bit_idx;
        assert!(self.0[word_idx] & mask == 0);
        self.0[word_idx] |= mask;
    }

    /// Clears a bit to inactive (0) at the specified offset
    ///
    /// # Arguments
    /// * `offset` - The bit position to clear (0-2047)
    ///
    /// # Panics
    /// Panics if offset is greater than LEAF_COUNT_IN_TWIG
    pub fn clear_bit(&mut self, offset: u32) {
        if offset > LEAF_COUNT_IN_TWIG {
            panic!("Invalid ID");
        }
        let word_idx = (offset / 64) as usize;
        let bit_idx = offset & 63;
        let mask = 1u64 << bit_idx;
        assert!(self.0[word_idx] & mask != 0);
        self.0[word_idx] &= !mask;
    }

    /// Gets the active status of a bit at the specified offset
    ///
    /// # Arguments
    /// * `offset` - The bit position to check (0-2047)
    ///
    /// # Returns
    /// true if the bit is active, false if inactive
    ///
    /// # Panics
    /// Panics if offset is greater than LEAF_COUNT_IN_TWIG
    pub fn get_bit(&self, offset: u32) -> bool {
        if offset > LEAF_COUNT_IN_TWIG {
            panic!("Invalid ID");
        }
        let word_idx = (offset / 64) as usize;
        let bit_idx = offset & 63;
        (self.0[word_idx] >> bit_idx) & 1 != 0
    }

    /// Gets a slice of bytes for a specific page (for hashing)
    ///
    /// # Arguments
    /// * `page_num` - The page number to get
    /// * `page_size` - The size of each page in bytes (32 or 64)
    ///
    /// # Returns
    /// A byte slice for the specified page
    pub fn get_bits(&self, page_num: usize, page_size: usize) -> &[u8] {
        debug_assert!(page_size == 32 || page_size == 64);
        let u64_per_page = page_size / 8; // 4 u64s for 32 bytes, 8 u64s for 64 bytes
        let start = page_num * u64_per_page;
        bytemuck::cast_slice(&self.0[start..start + u64_per_page])
    }

    /// Returns the underlying data as a byte slice
    pub fn as_bytes(&self) -> &[u8] {
        bytemuck::cast_slice(&self.0)
    }
}

#[cfg(test)]
mod active_bits_tests {
    use super::*;

    #[test]
    fn test_new_bits() {
        let bits = ActiveBits::default();
        for i in 0..32 {
            assert_eq!(0, bits.0[i]);
        }
    }

    #[test]
    fn test_set_bit() {
        let mut bits = ActiveBits::default();

        // bit 25: word_idx = 0, bit_idx = 25
        bits.set_bit(25);
        assert_eq!(1u64 << 25, bits.0[0]);

        // bit 70: word_idx = 1 (70/64=1), bit_idx = 6 (70%64=6)
        bits.set_bit(70);
        assert_eq!(1u64 << 6, bits.0[1]);

        // bit 83: word_idx = 1, bit_idx = 19 (83%64=19)
        bits.set_bit(83);
        assert_eq!((1u64 << 6) | (1u64 << 19), bits.0[1]);

        // bit 801: word_idx = 12 (801/64=12), bit_idx = 33 (801%64=33)
        bits.set_bit(801);
        assert_eq!(1u64 << 33, bits.0[12]);
    }

    #[test]
    fn test_clear_bits() {
        let mut bits = ActiveBits::default();

        // bit 2047: word_idx = 31 (2047/64=31), bit_idx = 63 (2047%64=63)
        bits.set_bit(2047);
        assert_eq!(1u64 << 63, bits.0[31]);

        // bit 2044: word_idx = 31, bit_idx = 60 (2044%64=60)
        bits.set_bit(2044);
        assert_eq!((1u64 << 63) | (1u64 << 60), bits.0[31]);

        bits.clear_bit(2047);
        assert_eq!(1u64 << 60, bits.0[31]);
        bits.clear_bit(2044);
        assert_eq!(0, bits.0[31]);
    }

    #[test]
    fn test_get_bit() {
        let mut bits = ActiveBits::default();

        bits.set_bit(2047);
        bits.set_bit(2044);

        assert!(bits.get_bit(2047));
        assert!(!bits.get_bit(2046));
        assert!(!bits.get_bit(2045));
        assert!(bits.get_bit(2044));
        assert!(!bits.get_bit(2043));
        assert!(!bits.get_bit(2042));
        assert!(!bits.get_bit(2041));
        assert!(!bits.get_bit(2040));
    }

    #[test]
    #[should_panic(expected = "Invalid ID")]
    fn test_set_bit_idx_out_of_range() {
        let mut bits = ActiveBits::default();
        bits.set_bit(LEAF_COUNT_IN_TWIG + 1);
    }

    #[test]
    #[should_panic(expected = "Invalid ID")]
    fn test_clear_bit_idx_out_of_range() {
        let mut bits = ActiveBits::default();
        bits.clear_bit(LEAF_COUNT_IN_TWIG + 1);
    }

    #[test]
    #[should_panic(expected = "Invalid ID")]
    fn test_get_bit_idx_out_of_range() {
        let bits = ActiveBits::default();
        bits.get_bit(LEAF_COUNT_IN_TWIG + 1);
    }

    #[test]
    fn test_get_bits() {
        let mut bits = ActiveBits::default();
        // page 3 uses u64[12..16]
        bits.0[12] = 0x0706050403020100;
        bits.0[13] = 0x0f0e0d0c0b0a0908;
        bits.0[14] = 0x1716151413121110;
        bits.0[15] = 0x1f1e1d1c1b1a1918;

        let page3 = bits.get_bits(3, 32);
        // bytes in little-endian order
        assert_eq!(page3[0], 0x00);
        assert_eq!(page3[7], 0x07);
        assert_eq!(page3[8], 0x08);
        assert_eq!(page3[31], 0x1f);
    }
}
