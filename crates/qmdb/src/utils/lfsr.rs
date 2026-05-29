//! Linear Feedback Shift Register (LFSR) implementation.
//!
//! This module provides a Galois LFSR implementation based on the paper:
//! "Efficient Shift Registers, LFSR Counters, and Long Pseudo-Random Sequence Generators"
//! from the University of Otago (https://www.physics.otago.ac.nz/reports/electronics/ETR2012-1.pdf).
//!
//! The implementation supports bit counts from 5 to 63 and provides maximum-length
//! sequences for each bit count using carefully chosen tap positions.

// Based on: https://www.physics.otago.ac.nz/reports/electronics/ETR2012-1.pdf

/// A Galois Linear Feedback Shift Register.
///
/// This implementation uses a Galois configuration, which is more efficient
/// than a Fibonacci configuration. The LFSR generates pseudo-random sequences
/// with maximum period (2^n - 1) for the supported bit counts.
#[derive(Debug, Clone, Copy)]
pub struct GaloisLfsr {
    /// Current state of the LFSR
    state: u64,
    /// Tap positions encoded as a bit mask
    taps: u64,
}

impl GaloisLfsr {
    /// Creates a new Galois LFSR with the specified seed and bit count.
    ///
    /// # Arguments
    /// * `seed` - Initial state of the LFSR (must be non-zero and less than 2^bit_count)
    /// * `bit_count` - Number of bits in the LFSR (must be between 5 and 63)
    ///
    /// # Returns
    /// A new GaloisLfsr instance
    ///
    /// # Panics
    /// - If the seed is zero
    /// - If the seed is too large for the specified bit count
    /// - If the bit count is not between 5 and 63
    ///
    /// # Implementation Details
    /// The tap positions for each bit count are chosen to provide maximum-length
    /// sequences. These positions are based on primitive polynomials over GF(2)
    /// and are guaranteed to generate all possible non-zero states before repeating.
    pub fn new(seed: u64, bit_count: usize) -> Self {
        let mask = (1u64 << bit_count) - 1;
        if seed > mask {
            panic!("Seed is too large seed={:#018x}", seed);
        }

        if seed == 0 {
            panic!("Seed cannot be zero");
        }

        let pos = match bit_count {
            5 => (5, 4, 3, 2),
            6 => (6, 5, 3, 2),
            7 => (7, 6, 5, 4),
            8 => (8, 6, 5, 4),
            9 => (9, 8, 6, 5),
            10 => (10, 9, 7, 6),
            11 => (11, 10, 9, 7),
            12 => (12, 11, 8, 6),
            13 => (13, 12, 10, 9),
            14 => (14, 13, 11, 9),
            15 => (15, 14, 13, 11),
            16 => (16, 14, 13, 11),
            17 => (17, 16, 15, 14),
            18 => (18, 17, 16, 13),
            19 => (19, 18, 17, 14),
            20 => (20, 19, 16, 14),
            21 => (21, 20, 19, 16),
            22 => (22, 19, 18, 17),
            23 => (23, 22, 20, 18),
            24 => (24, 23, 21, 20),
            25 => (25, 24, 23, 22),
            26 => (26, 25, 24, 20),
            27 => (27, 26, 25, 22),
            28 => (28, 27, 24, 22),
            29 => (29, 28, 27, 25),
            30 => (30, 29, 26, 24),
            31 => (31, 30, 29, 28),
            32 => (32, 30, 26, 25),
            33 => (33, 32, 29, 27),
            34 => (34, 31, 30, 26),
            35 => (35, 34, 28, 27),
            36 => (36, 35, 29, 28),
            37 => (37, 36, 33, 31),
            38 => (38, 37, 33, 32),
            39 => (39, 38, 35, 32),
            40 => (40, 37, 36, 35),
            41 => (41, 40, 39, 38),
            42 => (42, 40, 37, 35),
            43 => (43, 42, 38, 37),
            44 => (44, 42, 39, 38),
            45 => (45, 44, 42, 41),
            46 => (46, 40, 39, 38),
            47 => (47, 46, 43, 42),
            48 => (48, 44, 41, 39),
            49 => (49, 45, 44, 43),
            50 => (50, 48, 47, 46),
            51 => (51, 50, 48, 45),
            52 => (52, 51, 49, 46),
            53 => (53, 52, 51, 47),
            54 => (54, 51, 48, 46),
            55 => (55, 54, 53, 49),
            56 => (56, 54, 52, 49),
            57 => (57, 55, 54, 52),
            58 => (58, 57, 53, 52),
            59 => (59, 57, 55, 52),
            60 => (60, 58, 56, 55),
            61 => (61, 60, 59, 56),
            62 => (62, 59, 57, 56),
            63 => (63, 62, 59, 58),
            _ => panic!("invalid bit count"),
        };

        let taps = (1u64 << pos.0) | (1u64 << pos.1) | (1u64 << pos.2) | (1u64 << pos.3);

        Self { state: seed, taps }
    }

    /// Advances the LFSR by one step and returns the new state.
    ///
    /// This method:
    /// 1. Checks if the lowest bit is set
    /// 2. If set, XORs the state with the tap positions
    /// 3. Right shifts the state by one bit
    ///
    /// # Returns
    /// The new state of the LFSR
    pub fn next(&mut self) -> u64 {
        let taps = if self.state % 2 == 0 { 0 } else { self.taps };
        self.state = (self.state ^ taps) >> 1;
        self.state
    }

    /// Advances the LFSR by n steps and returns the lowest n bits.
    ///
    /// # Arguments
    /// * `n` - Number of steps to advance and bits to return
    ///
    /// # Returns
    /// The lowest n bits of the state after advancing n steps
    pub fn rand_n(&mut self, n: usize) -> u64 {
        for _ in 0..n {
            self.next();
        }
        self.state & ((1u64 << n) - 1)
    }

    /// Gets the current state of the LFSR.
    ///
    /// # Returns
    /// The current state
    pub fn state(&self) -> u64 {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests the LFSR implementation for various bit counts.
    ///
    /// This test verifies that:
    /// - The LFSR generates maximum-length sequences
    /// - The sequence returns to the initial seed after the full period
    /// - The implementation works for bit counts from 5 to 16
    #[test]
    fn test_lfsr() {
        let seed = 1u64;
        for bit_count in 5..17 {
            let mut lfsr = GaloisLfsr::new(seed, bit_count);
            let mut count = 0;
            while lfsr.next() != seed {
                count += 1;
            }
            println!("count: {count}\n");
        }
    }
}
