//! Core definitions and constants for QMDB.
//!
//! This module contains fundamental constants and helper functions used throughout
//! the QMDB codebase, including:
//! - Cache and buffer sizes
//! - Cryptographic parameters
//! - Entry and shard configurations
//! - File system paths
//! - Merkle tree parameters
//! - Operation codes
//! - Compaction settings
//!
//! # Architecture
//! The constants are organized into several categories:
//!
//! ## Storage Configuration
//! - Shard counts and sizes
//! - Buffer dimensions
//! - File paths and segments
//!
//! ## Security Parameters
//! - Encryption settings (nonce, tag sizes)
//! - Entry validation constants
//! - Version tracking
//!
//! ## Performance Tuning
//! - Cache configurations
//! - Batch processing limits
//! - Compaction thresholds
//!
//! ## Merkle Tree Structure
//! - Tree levels and dimensions
//! - Twig parameters
//! - Node organization
//!
//! # Performance Considerations
//! - Buffer sizes affect memory usage and I/O patterns
//! - Shard counts impact parallelism and contention
//! - Compaction parameters influence storage efficiency
//!
//! # Security Notes
//! - Cryptographic parameters follow industry standards
//! - Version tracking prevents replay attacks
//! - Entry validation ensures data integrity

/// Number of shards in the bytes cache.
/// Higher values improve concurrency but increase memory usage.
pub const BYTES_CACHE_SHARD_COUNT: usize = 512;

/// Size of the large buffer for bulk operations (16KB).
/// Used for efficient batch processing of entries.
pub const BIG_BUF_SIZE: usize = 16 * 1024;

/// Size of the nonce used in AES-GCM encryption (12 bytes).
/// Standard nonce size for AES-GCM as per NIST recommendations.
pub const NONCE_SIZE: usize = 12;

/// Size of the authentication tag in AES-GCM encryption (16 bytes).
/// Standard tag size for AES-GCM providing 128-bit security.
pub const TAG_SIZE: usize = 16;

/// Fixed length of an entry in bytes (excluding encryption overhead).
/// Includes: key length(1) + value length(3) + deactivated serial number count(1) + key + value + next key hash(32) + version(8) + serial number(8)
/// Layout:
/// +---------------+------------------+--------------------------------+
/// | key len (1B)  | value len (3B)   | deactivated SN count (1B)      |
/// +---------------+------------------+--------------------------------+
/// | key (var)     | value (var)      | next key hash (32B)            |
/// +---------------+------------------+--------------------------------+
/// | version (8B)  | serial num (8B)  | deactivated SN list (var*8B)   |
/// +---------------+------------------+--------------------------------+
pub const ENTRY_FIXED_LENGTH: usize = 1 + 3 + 1 + 32 + 8 + 8;

#[cfg(feature = "tee_cipher")]
/// Base entry length with encryption overhead for TEE mode.
/// Includes fixed length plus authentication tag.
pub const ENTRY_BASE_LENGTH: usize = ENTRY_FIXED_LENGTH + TAG_SIZE;

#[cfg(not(feature = "tee_cipher"))]
/// Base entry length without encryption overhead for non-TEE mode.
/// Same as fixed length when encryption is disabled.
pub const ENTRY_BASE_LENGTH: usize = ENTRY_FIXED_LENGTH;

/// Version number indicating a null entry.
/// Used to mark deleted or invalid entries.
pub const NULL_ENTRY_VERSION: i64 = -2;

/// Number of shards in the database.
/// Affects parallelism and data distribution.
pub const SHARD_COUNT: usize = 16;

/// Default size of an entry in bytes.
/// Optimized for common key-value sizes.
pub const DEFAULT_ENTRY_SIZE: usize = 192;

/// Number of sentinel entries per shard.
/// Used for boundary marking and validation.
pub const SENTRY_COUNT: usize = (1 << 16) / SHARD_COUNT;

/// Number of blocks between pruning operations.
/// Controls storage cleanup frequency.
pub const PRUNE_EVERY_NBLOCKS: i64 = 500;

/// Maximum number of proof requests that can be processed at once.
/// Prevents resource exhaustion from proof generation.
pub const MAX_PROOF_REQ: usize = 1000;

/// Size of the pre-read buffer (256KB).
/// Optimizes sequential read performance.
pub const PRE_READ_BUF_SIZE: usize = 256 * 1024;

/// Maximum file size range (2048TB).
/// Supports large-scale blockchain state storage.
pub const HPFILE_RANGE: i64 = 1i64 << 51; // 2048TB

/// Number of entries per shard division.
/// Controls intra-shard data distribution.
pub const SHARD_DIV: usize = (1 << 16) / SHARD_COUNT;

/// Operation code for read operations.
/// Used in transaction processing.
pub const OP_READ: u8 = 1;

/// Operation code for create operations.
/// Used in transaction processing.
pub const OP_CREATE: u8 = 2;

/// Operation code for write operations.
/// Used in transaction processing.
pub const OP_WRITE: u8 = 3;

/// Operation code for delete operations.
/// Used in transaction processing.
pub const OP_DELETE: u8 = 4;

/// Default size for new files (1MB).
/// Initial allocation for storage files.
pub const DEFAULT_FILE_SIZE: i64 = 1024 * 1024;

/// Size of small buffers (32KB).
/// Used for routine operations.
pub const SMALL_BUFFER_SIZE: i64 = 32 * 1024;

/// First level above twig in the Merkle tree.
/// Defines tree structure boundary.
pub const FIRST_LEVEL_ABOVE_TWIG: i64 = 13;

/// Root level of a twig in the Merkle tree (12).
/// Defines twig subtree height.
pub const TWIG_ROOT_LEVEL: i64 = FIRST_LEVEL_ABOVE_TWIG - 1; // 12

/// Minimum number of entries required for pruning.
/// Prevents excessive storage fragmentation.
pub const MIN_PRUNE_COUNT: u64 = 2;

/// Path for code storage.
/// Directory for smart contract code.
pub const CODE_PATH: &str = "code";

/// Path for entry storage.
/// Directory for key-value entries.
pub const ENTRIES_PATH: &str = "entries";

/// Path for twig storage.
/// Directory for Merkle tree twigs.
pub const TWIG_PATH: &str = "twig";

/// Number of shards for twig storage.
/// Parallelizes twig operations.
pub const TWIG_SHARD_COUNT: usize = 4;

/// Number of shards for node storage.
/// Parallelizes node operations.
pub const NODE_SHARD_COUNT: usize = 4;

/// Maximum level in the Merkle tree.
/// Supports large state spaces.
pub const MAX_TREE_LEVEL: usize = 64;

/// Maximum level above twig in the Merkle tree (51).
/// Defines upper tree structure.
pub const MAX_UPPER_LEVEL: usize = MAX_TREE_LEVEL - FIRST_LEVEL_ABOVE_TWIG as usize; // 51

/// Bit shift for twig operations (twig has 2^11 leaves).
/// Optimizes twig calculations.
pub const TWIG_SHIFT: u32 = 11; // a twig has 2**11 leaves

/// Number of leaves in a twig (2^11 = 2048).
/// Defines twig capacity.
pub const LEAF_COUNT_IN_TWIG: u32 = 1 << TWIG_SHIFT; // 2**11==2048

/// Mask for extracting leaf index within a twig.
/// Optimizes leaf lookup.
pub const TWIG_MASK: u32 = LEAF_COUNT_IN_TWIG - 1;

/// Size of the compaction ring buffer.
/// Controls compaction job queuing.
pub const COMPACT_RING_SIZE: usize = 1024;

// UTILIZATION
pub const COMPACT_THRES: i64 = 20000000;
pub const UTILIZATION_DIV: i64 = 10;
pub const UTILIZATION_RATIO: i64 = 7;

/// Determines if a range of entries should be compacted based on utilization metrics.
///
/// This function evaluates whether compaction would improve storage efficiency by
/// considering the ratio of active entries to total entries and comparing against
/// configured thresholds.
///
/// # Arguments
/// * `utilization_div` - Divisor for utilization ratio (e.g., 10 for 10%)
/// * `utilization_ratio` - Target utilization ratio (e.g., 7 for 70%)
/// * `compact_thres` - Minimum threshold for compaction
/// * `active_count` - Number of active entries in the range
/// * `sn_start` - Starting serial number of the range
/// * `sn_end` - Ending serial number of the range
///
/// # Returns
/// `true` if compaction is needed, `false` otherwise
///
/// # Performance
/// - O(1) constant time operation
/// - No allocations or I/O
///
/// # Examples
/// ```
/// use qmdb_common::def::is_compactible;
///
/// let should_compact = is_compactible(
///     10,     // 10% divisor
///     7,      // 70% target ratio
///     1000,   // minimum 1000 entries
///     800,    // 800 active entries
///     0,      // start at serial 0
///     1000,   // end at serial 1000
/// );
/// ```
pub fn is_compactible(
    utilization_div: i64,
    utilization_ratio: i64,
    compact_thres: i64,
    active_count: usize,
    sn_start: u64,
    sn_end: u64,
) -> bool {
    /*
    Utilization check:
    active_count   utilization_ratio
    ------------ < ----------------    ==> true (need compaction)
    total_count     utilization_div

    Where:
    - active_count: Number of active entries
    - total_count: Total number of entries (sn_end - sn_start)
    - utilization_ratio/utilization_div: Target utilization ratio
    */
    let has_few_active_entries = active_count < compact_thres as usize;
    if has_few_active_entries {
        return false;
    }
    let total_count = sn_end - sn_start;
    let is_good_utilization = (total_count as usize * utilization_ratio as usize)
        < (active_count * utilization_div as usize);
    !is_good_utilization
}

/// Calculates the maximum level in the Merkle tree based on the youngest twig ID.
///
/// This function determines how many levels are needed in the Merkle tree to
/// accommodate all entries up to the most recent twig. It uses the twig ID to
/// calculate the required tree height.
///
/// # Arguments
/// * `youngest_twig_id` - ID of the youngest (most recent) twig
///
/// # Returns
/// The maximum level needed in the Merkle tree
///
/// # Performance
/// - O(1) constant time operation
/// - Uses CPU intrinsics for efficiency
///
/// # Examples
/// ```
/// use qmdb_common::def::calc_max_level;
///
/// let max_level = calc_max_level(1000); // Calculate tree height for twig 1000
/// ```
pub fn calc_max_level(youngest_twig_id: u64) -> i64 {
    // The bit-length of an unsigned integer x is the number of bits required to represent x in binary (excluding any leading zeros).
    // it directly corresponds to the minimum height of a binary tree needed to index all elements from 0 up to x.
    // A tree consisting of only a root node has a height of 0.
    // The root of each twig is at level 12, therefore:
    // bit_length(x) = 64 - leading_zeros(x)
    // max_level = 12 + bit_length(youngest_twig_id)
    //           = 12 + 64 - leading_zeros(youngest_twig_id)
    //           = 13 + 63 - leading_zeros(youngest_twig_id)

    FIRST_LEVEL_ABOVE_TWIG + 63 - youngest_twig_id.leading_zeros() as i64
}

/// Calculates the maximum level in the Merkle tree from a serial number.
///
/// This function converts a serial number into a twig ID and then computes the
/// tree height required to include that twig.
///
/// # Arguments
/// * `sn` - The serial number of an entry.
///
/// # Returns
/// The maximum required level in the Merkle tree.
pub fn calc_max_level_from_sn(sn: u64) -> u8 {
    calc_max_level(sn >> TWIG_SHIFT) as u8
}

/// Calculates the maximum node index at a given level based on the twig ID.
///
/// # Arguments
/// * `twig_id` - ID of the most recently created twig
/// * `level` - Level to calculate maximum node index for
///
/// # Returns
/// The maximum valid node index at the specified level
pub fn nth_at_level_from_twigid(twig_id: u64, level: i64) -> u64 {
    assert!(level >= FIRST_LEVEL_ABOVE_TWIG, "{} is too small", level);
    let diff_level = level - FIRST_LEVEL_ABOVE_TWIG;
    #[rustfmt::skip]
    assert!(diff_level <= u8::MAX as i64,"{} is too large, exceeds u64 capacity",level);
    #[rustfmt::skip]
    assert_ne!(twig_id >> diff_level, 0, "twig_id {} is too small for level {}", twig_id, level);
    twig_id >> (diff_level + 1)
}

pub fn nth_at_level_from_level0nth(level0nth: u64, level: u8) -> u64 {
    if level >= 1 {
        assert_ne!(level0nth >> (level - 1), 0);
    }
    level0nth >> level
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calc_max_level() {
        // Test cases in format: (twig_id, expected_leading_zeros, expected_bit_length, expected_max_level)
        let cases = [
            (0u64, 64, 0, 12),       // 0b0
            (1u64, 63, 1, 12 + 1),   // 0b1
            (2u64, 62, 2, 12 + 2),   // 0b10
            (3u64, 62, 2, 12 + 2),   // 0b11
            (4u64, 61, 3, 12 + 3),   // 0b100
            (7u64, 61, 3, 12 + 3),   // 0b111
            (8u64, 60, 4, 12 + 4),   // 0b1000
            (10u64, 60, 4, 12 + 4),  // 0b1010
            (15u64, 60, 4, 12 + 4),  // 0b1111
            (16u64, 59, 5, 12 + 5),  // 0b10000
            (20u64, 59, 5, 12 + 5),  // 0b10100
            (32u64, 58, 6, 12 + 6),  // 0b100000
            (100u64, 57, 7, 12 + 7), // 0b1100100
            // Byte boundaries
            (0xFFu64, 56, 8, 12 + 8),         // eight 1s
            (0xFFFFu64, 48, 16, 12 + 16),     // sixteen 1s
            (0xFFFFFFFFu64, 32, 32, 12 + 32), // thirty-two 1s
            (u64::MAX, 0, 64, 12 + 64),       // sixty-four 1s
        ];

        for (twig_id, leading_zeros, bit_length, expected_level) in cases {
            assert_eq!(
                twig_id.leading_zeros(),
                leading_zeros,
                "Leading zeros mismatch for twig_id: {twig_id}"
            );

            assert_eq!(
                leading_zeros,
                64 - bit_length,
                "Bit length mismatch for twig_id: {twig_id}"
            );

            assert_eq!(
                calc_max_level(twig_id),
                expected_level,
                "Failed for twig_id: {twig_id} (leading_zeros: {leading_zeros}), expected level: {expected_level}"
            );
        }
    }
}
