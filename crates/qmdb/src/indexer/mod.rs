//! High-performance indexing system for QMDB.
//!
//! This module provides a scalable and efficient indexing system that supports:
//! - Fast key-value lookups
//! - Concurrent access and modifications
//! - Sharded storage for improved performance
//! - Version tracking and history
//! - Compaction and cleanup
//!
//! # Architecture
//!
//! The indexing system consists of two main implementations:
//!
//! ## Hybrid Indexer
//! Enabled with the `use_hybridindexer` feature flag.
//! - Uses a combination of in-memory and disk storage
//! - Optimized for large datasets
//! - Supports background compaction
//! - Efficient range queries
//!
//! ## Memory Indexer
//! Default implementation when `use_hybridindexer` is disabled.
//! - Pure in-memory storage
//! - Lower latency for small to medium datasets
//! - Simpler implementation
//! - Faster recovery
//!
//! # Core Components
//!
//! - `KVList`: Sorted key-value storage with efficient lookups
//! - `PosList`: Position tracking for entry locations
//! - `IndexCache`: Caching layer for frequently accessed entries
//! - `Unit`: Basic storage unit with atomic operations
//!
//! # Performance
//!
//! The indexer is optimized for:
//! - High throughput key-value operations
//! - Minimal memory overhead
//! - Efficient range queries
//! - Fast recovery after crashes
//!
//! # Thread Safety
//!
//! All operations are thread-safe through:
//! - Atomic operations
//! - Fine-grained locking
//! - Lock-free data structures
//! - Safe concurrent access
//!
//! # Example Usage
//!
//! ```no_run
//! use qmdb::indexer::Indexer;
//! use qmdb::indexer::memidx::UnitTrait;
//! use qmdb_common::indexer::indexer_trait::IndexerTrait;
//!
//! // Create a new indexer
//! let indexer = Indexer::new(65536);
//!
//! // Add key-value pairs
//! let key_bytes = b"example_key";
//! let position = 12345i64;
//! let serial_number = 67890u64;
//! let height = 1i64;
//! indexer.add_kv(key_bytes, position, serial_number);
//!
//! // Query values
//! let positions = indexer.for_each_value(height, key_bytes);
//!
//! // Remove entries
//! indexer.erase_kv(key_bytes, position, serial_number);
//! ```

pub mod hybrid;
pub mod kvlist;
pub mod memidx;

/// High-performance indexer implementation.
///
/// This type alias points to either the hybrid or in-memory implementation
/// based on the `use_hybridindexer` feature flag.
///
/// # Features
///
/// - Concurrent access and modifications
/// - Efficient key-value storage
/// - Version tracking
/// - Background compaction
/// - Range query support
///
/// # Thread Safety
///
/// All operations are thread-safe and can be safely shared across threads.
/// The implementation uses atomic operations and fine-grained locking
/// to ensure consistency.
///
/// # Performance
///
/// - O(log n) lookups
/// - Efficient range queries
/// - Optimized memory usage
/// - Background compaction
#[cfg(feature = "use_hybridindexer")]
pub type Indexer = hybrid::HybridIndexer;

/// Memory-optimized indexer implementation.
///
/// This type alias points to the in-memory implementation when the
/// `use_hybridindexer` feature is disabled.
///
/// # Features
///
/// - Pure in-memory storage
/// - Lower latency operations
/// - Simple recovery
/// - Efficient for small to medium datasets
///
/// # Thread Safety
///
/// All operations are thread-safe through atomic operations
/// and fine-grained locking.
///
/// # Performance
///
/// - O(log n) lookups
/// - Minimal overhead
/// - Fast recovery
#[cfg(not(feature = "use_hybridindexer"))]
pub type Indexer = memidx::InMemIndexer;

#[cfg(test)]
mod tests {
    use qmdb_common::indexer::indexer_trait::IndexerTrait;
    use serial_test::serial;

    use crate::test_helper::to_k80;

    use super::*;

    /// Gets all values associated with a key.
    ///
    /// # Arguments
    /// * `bt` - Indexer instance
    /// * `k80` - 80-bit key to look up
    ///
    /// # Returns
    /// Vector of file positions for the key
    fn get_all_values(bt: &Indexer, k80: &[u8]) -> Vec<i64> {
        let pos_list = bt.for_each_value(-1, k80);
        pos_list.enumerate().collect()
    }

    /// Gets all adjacent values for a key.
    ///
    /// # Arguments
    /// * `bt` - Indexer instance
    /// * `k80` - 80-bit key to look up
    ///
    /// # Returns
    /// Vector of file positions for adjacent keys
    fn get_all_adjacent_values(bt: &Indexer, k80: &[u8]) -> Vec<i64> {
        let post_list = bt.for_each_adjacent_value(-1, k80);
        post_list.enumerate().collect()
    }

    /// Tests that adding duplicate key-value pairs panics.
    #[test]
    #[serial]
    #[should_panic(expected = "Add Duplicated KV")]
    fn test_panic_duplicate_add_kv() {
        let bt = Indexer::new(65536);
        bt.add_kv(&to_k80(0x0004000300020001), 0x10, 0);
        bt.add_kv(&to_k80(0x0004000300020001), 0x10, 0);
    }

    /// Tests that erasing non-existent key-value pairs panics.
    #[test]
    #[serial]
    #[should_panic(expected = "Cannot Erase Non-existent KV")]
    fn test_panic_erase_non_existent_kv() {
        let bt = Indexer::new(65536);
        bt.add_kv(&to_k80(0x0004000300020001), 0x10, 0);
        bt.erase_kv(&to_k80(0x0004000300020000), 0x10, 0);
    }

    /// Tests basic key-value addition and removal.
    #[test]
    #[serial]
    fn test_add_kv() {
        let bt = Indexer::new(65536);
        bt.add_kv(&to_k80(0x1111_2222_3333_4444), 888, 0);
        bt.erase_kv(&to_k80(0x1111_2222_3333_4444), 888, 0);
    }

    #[test]
    #[serial]
    fn test_btree() {
        let bt = Indexer::new(65536);
        assert_eq!(0, bt.len(0));
        bt.add_kv(&to_k80(0x0004000300020001), 0x10, 0);
        assert_eq!(1, bt.len(0));
        bt.add_kv(&to_k80(0x0005000300020001), 0x10, 0);
        assert_eq!(2, bt.len(0));
        bt.add_kv(&to_k80(0x0004000300020001), 0x00, 0);

        assert_eq!(
            [0x10, 0x0],
            get_all_values(&bt, &to_k80(0x0004000300020001)).as_slice()
        );

        bt.add_kv(&to_k80(0x0004000300020000), 0x20, 0);
        bt.add_kv(&to_k80(0x0004000300020000), 0x30, 0);
        assert_eq!(5, bt.len(0));

        assert_eq!(
            [0x10, 0x0],
            get_all_values(&bt, &to_k80(0x0004000300020001)).as_slice()
        );
        assert_eq!(
            [0x30, 0x20],
            get_all_values(&bt, &to_k80(0x0004000300020000)).as_slice()
        );

        assert_eq!(
            [0x10, 0, 0x30, 0x20,],
            get_all_adjacent_values(&bt, &to_k80(0x0004000300020001)).as_slice()
        );

        assert_eq!(
            [0x30, 0x20],
            get_all_adjacent_values(&bt, &to_k80(0x0004000300020000)).as_slice()
        );

        bt.add_kv(&to_k80(0x0004000300020001), 0x100, 0);
        bt.add_kv(&to_k80(0x0004000300020001), 0x110, 0);
        bt.add_kv(&to_k80(0x0004000300020001), 0x120, 0);
        bt.add_kv(&to_k80(0x0004000300020001), 0x130, 0);
        bt.add_kv(&to_k80(0x0004000300020001), 0x140, 0);
        bt.add_kv(&to_k80(0x0004000300020001), 0x150, 0);
        bt.add_kv(&to_k80(0x0004000300020001), 0x160, 0);
        bt.add_kv(&to_k80(0x0004000300020001), 0x170, 0);
        assert_eq!(
            [0x170, 0x160, 0x150, 0x140, 0x130, 0x120, 0x110, 0x100, 0x10, 0],
            get_all_values(&bt, &to_k80(0x0004000300020001)).as_slice()
        );
        bt.change_kv(&to_k80(0x0004000300020001), 0x170, 0x710, 0, 0);
        assert_eq!(
            [0x710, 0x160, 0x150, 0x140, 0x130, 0x120, 0x110, 0x100, 0x10, 0],
            get_all_values(&bt, &to_k80(0x0004000300020001)).as_slice()
        );
        bt.add_kv(&to_k80(0x0004000300020002), 0x180, 0);

        assert_eq!(
            [0x180, 0x710, 0x160, 0x150, 0x140, 0x130, 0x120, 0x110, 0x100, 0x10, 0,],
            get_all_adjacent_values(&bt, &to_k80(0x0004000300020002)).as_slice()
        );

        bt.erase_kv(&to_k80(0x0004000300020001), 0x150, 0);

        assert_eq!(
            [0x710, 0x160, 0x140, 0x130, 0x120, 0x110, 0x100, 0x10, 0, 0x30, 0x20,],
            get_all_adjacent_values(&bt, &to_k80(0x0004000300020001)).as_slice()
        );

        bt.add_kv(&to_k80(0x000400030001FFFF), 0x150, 0);

        assert_eq!(
            [0x710, 0x160, 0x140, 0x130, 0x120, 0x110, 0x100, 0x10, 0, 0x30, 0x20,],
            get_all_adjacent_values(&bt, &to_k80(0x0004000300020001)).as_slice()
        );
    }

    #[test]
    #[serial]
    fn test_key_exists() {
        let indexer = Indexer::new(65536);
        indexer.add_kv(&to_k80(111), 88, 0);
        indexer.add_kv(&to_k80(222), 888, 0);

        assert!(!indexer.key_exists(&to_k80(333), 128, 0));
        assert!(!indexer.key_exists(&to_k80(111), 128, 0));
        assert!(!indexer.key_exists(&to_k80(111), 888, 0));
        assert!(indexer.key_exists(&to_k80(111), 88, 0));
        assert!(indexer.key_exists(&to_k80(222), 888, 0));
    }
}
