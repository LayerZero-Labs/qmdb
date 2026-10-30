//! Configuration module for QMDB (Quick Merkle Database).
//!
//! This module provides configuration options for tuning database performance,
//! security settings, and storage behavior. It includes settings for:
//! - File and buffer sizes
//! - Compaction thresholds and ratios
//! - Encryption keys
//! - IO optimization parameters
//! - Threading and concurrency options
//!
//! # Performance Tuning
//! The configuration allows fine-tuning of several performance aspects:
//!
//! ## Memory Management
//! - `wrbuf_size`: Controls the write buffer size (default: 8MB)
//!   - Larger values improve write performance but increase memory usage
//!   - Should be tuned based on available RAM and write patterns
//!
//! ## Storage Management
//! - `file_segment_size`: Controls file segment size (default: 1GB)
//!   - Affects I/O patterns and disk space allocation
//!   - Larger values reduce fragmentation but may impact initial allocation time
//!
//! ## Compaction Settings
//! - `compact_thres`: Entry count threshold for compaction
//! - `utilization_ratio`/`utilization_div`: Target storage utilization
//!   - Example: 7/10 targets 70% utilization after compaction
//!
//! ## I/O Optimization
//! - `prefetcher_thread_count`: Number of prefetcher threads
//! - `uring_count` and `uring_size`: io_uring settings (Linux only)
//!   - Tune based on storage device capabilities
//!   - Higher values may improve I/O throughput
//!
//! # Security Features
//! ## Encryption
//! - Optional AES-GCM encryption for sensitive data
//! - Separate keys for shards, indexer, and metadata
//! - 96-byte key structure:
//!   - Bytes 0-31: Shard encryption
//!   - Bytes 32-63: Indexer encryption
//!   - Bytes 64-95: MetaDB encryption
//!
//! # Usage Examples
//! ```no_run
//! use qmdb::config::Config;
//!
//! // Default configuration with custom directory
//! let config = Config::from_dir("/path/to/db");
//!
//! // Custom configuration with encryption
//! let mut config = Config::new(
//!     "/path/to/db",
//!     8 * 1024 * 1024,      // 8MB write buffer
//!     1024 * 1024 * 1024,   // 1GB segments
//!     true,                 // Use twig files
//!     None,                 // No encryption
//!     20_000_000,           // Compact threshold
//!     7,                    // Utilization ratio
//!     10,                   // Utilization divisor
//!     8,                    // Prefetcher threads
//!     16,                   // io_uring count
//!     1024,                 // io_uring size
//!     false,                // Don't bypass prefetcher
//!     2,                    // MetaDB info file count
//! );
//!
//! // Enable encryption
//! let mut keys = [0u8; 96];
//! // ... initialize keys ...
//! config.set_aes_keys(keys);
//! ```
//!
//! # Best Practices
//! 1. Start with default values and benchmark
//! 2. Adjust write buffer size based on memory constraints
//! 3. Configure compaction settings based on write patterns
//! 4. Enable encryption in production environments
//! 5. Tune I/O parameters based on storage hardware

#![allow(clippy::too_many_arguments)]

use qmdb_common::def::{COMPACT_THRES, UTILIZATION_DIV, UTILIZATION_RATIO};

/// Default number of threads for the prefetcher component.
///
/// The prefetcher improves read performance by asynchronously loading
/// data that may be needed soon. More threads can improve throughput
/// but consume more CPU resources.
const PREFETCHER_THREAD_COUNT: usize = 8;

/// Default size of io_uring queues for Linux systems.
///
/// Larger queue sizes can improve I/O throughput but consume more memory.
/// Should be tuned based on storage device capabilities and system resources.
const URING_SIZE: u32 = 256;

/// Default number of io_uring instances for Linux systems.
///
/// Multiple instances allow for better I/O parallelism but consume more
/// system resources. Should be tuned based on storage architecture and
/// available CPU cores.
const URING_COUNT: usize = 16;

/// Configuration for the QMDB database.
///
/// This struct contains all configurable parameters that control the database's
/// behavior, performance characteristics, and security features.
///
/// # Performance Considerations
/// - Write buffer size affects memory usage and write performance
/// - File segment size impacts I/O patterns and storage allocation
/// - Compaction settings balance storage efficiency and write overhead
/// - Prefetcher settings affect read performance and CPU usage
///
/// # Security Features
/// - Optional AES-GCM encryption for sensitive data
/// - Separate encryption keys for different components
/// - Secure storage of metadata and indices
///
/// # Platform-Specific Features
/// Some settings are specific to certain platforms:
/// - `uring_count` and `uring_size`: Linux-only io_uring settings
/// - Other settings work across all supported platforms
#[derive(Debug, Clone)]
pub struct Config {
    /// Base directory for all database files
    pub dir: String,
    /// Size of the write buffer in bytes (default: 8MB)
    pub wrbuf_size: usize,
    /// Size of file segments in bytes (default: 1GB)
    pub file_segment_size: usize,
    /// Whether to use twig files for improved tree traversal
    pub with_twig_file: bool,
    /// Optional 96-byte encryption keys for AES-GCM
    pub aes_keys: Option<[u8; 96]>,
    /// Threshold for triggering compaction
    pub compact_thres: i64,
    /// Target utilization ratio numerator
    pub utilization_ratio: i64,
    /// Target utilization ratio denominator
    pub utilization_div: i64,
    /// Number of threads for the prefetcher component
    pub prefetcher_thread_count: usize,
    /// Number of io_uring instances (Linux only)
    pub uring_count: usize,
    /// Size of io_uring queues (Linux only)
    pub uring_size: u32,
    /// Whether to bypass the prefetcher for direct access
    pub bypass_prefetcher: bool,
    /// Number of metadata info files to rotate for MetaDB (info.0, info.1, ...).
    ///
    /// Higher values keep more historical snapshots of MetaDB state on disk,
    /// at the cost of slightly more disk usage. A value of 2 keeps the
    /// existing behavior of alternating between `info.0` and `info.1`.
    pub meta_info_file_count: usize,
}

impl Default for Config {
    /// Creates a new Config instance with default values.
    ///
    /// # Default Values
    /// - `wrbuf_size`: 8MB
    /// - `file_segment_size`: 1GB
    /// - `with_twig_file`: false
    /// - `aes_keys`: None
    /// - `compact_thres`: 20,000,000
    /// - `utilization_ratio`: 7
    /// - `utilization_div`: 10
    /// - `prefetcher_thread_count`: 8
    /// - `uring_count`: 16
    /// - `uring_size`: 1024
    /// - `bypass_prefetcher`: false
    /// - `meta_info_file_count`: 2
    fn default() -> Self {
        Self {
            dir: "default".to_string(),
            wrbuf_size: 8 * 1024 * 1024,           //8MB
            file_segment_size: 1024 * 1024 * 1024, // 1GB
            with_twig_file: false,
            aes_keys: None,
            compact_thres: COMPACT_THRES,
            utilization_ratio: UTILIZATION_RATIO,
            utilization_div: UTILIZATION_DIV,
            prefetcher_thread_count: PREFETCHER_THREAD_COUNT,
            uring_count: URING_COUNT,
            uring_size: URING_SIZE,
            bypass_prefetcher: false,
            meta_info_file_count: 2,
        }
    }
}

impl Config {
    /// Creates a new Config instance with custom values.
    ///
    /// # Arguments
    /// * `dir` - Base directory for database files
    /// * `wrbuf_size` - Size of the write buffer in bytes
    /// * `file_segment_size` - Size of file segments in bytes
    /// * `with_twig_file` - Whether to use twig files
    /// * `aes_keys` - Optional encryption keys
    /// * `compact_thres` - Compaction threshold
    /// * `utilization_ratio` - Target utilization ratio numerator
    /// * `utilization_div` - Target utilization ratio denominator
    /// * `prefetcher_thread_count` - Number of prefetcher threads
    /// * `uring_count` - Number of io_uring instances (Linux only)
    /// * `uring_size` - Size of io_uring queues (Linux only)
    /// * `bypass_prefetcher` - Whether to bypass the prefetcher
    /// * `meta_info_file_count` - Number of rotated MetaDB info files to keep
    pub fn new(
        dir: &str,
        wrbuf_size: usize,
        file_segment_size: usize,
        with_twig_file: bool,
        aes_keys: Option<[u8; 96]>,
        compact_thres: i64,
        utilization_ratio: i64,
        utilization_div: i64,
        prefetcher_thread_count: usize,
        uring_count: usize,
        uring_size: u32,
        bypass_prefetcher: bool,
        meta_info_file_count: usize,
    ) -> Self {
        Self {
            dir: dir.to_string(),
            wrbuf_size,
            file_segment_size,
            with_twig_file,
            aes_keys,
            compact_thres,
            utilization_ratio,
            utilization_div,
            prefetcher_thread_count,
            uring_count,
            uring_size,
            bypass_prefetcher,
            meta_info_file_count,
        }
    }

    /// Creates a new Config instance with default values except for the directory.
    ///
    /// # Arguments
    /// * `dir` - Base directory for database files
    ///
    /// # Returns
    /// A new Config instance with the specified directory and default values for other fields
    pub fn from_dir(dir: &str) -> Self {
        Config {
            dir: dir.to_string(),
            ..Config::default()
        }
    }

    /// Creates a new Config instance with custom compaction settings.
    ///
    /// # Arguments
    /// * `dir` - Base directory for database files
    /// * `compact_thres` - Compaction threshold
    /// * `utilization_ratio` - Target utilization ratio numerator
    /// * `utilization_div` - Target utilization ratio denominator
    ///
    /// # Returns
    /// A new Config instance with the specified directory and compaction settings,
    /// using default values for other fields
    pub fn from_dir_and_compact_opt(
        dir: &str,
        compact_thres: i64,
        utilization_ratio: i64,
        utilization_div: i64,
    ) -> Self {
        Config {
            dir: dir.to_string(),
            compact_thres,
            utilization_ratio,
            utilization_div,
            ..Config::default()
        }
    }

    /// Sets the encryption keys for the database.
    ///
    /// # Arguments
    /// * `keys` - 96-byte array containing the encryption keys:
    ///   - Bytes 0-31: Shard encryption keys
    ///   - Bytes 32-63: Indexer encryption key
    ///   - Bytes 64-95: MetaDB encryption key
    pub fn set_aes_keys(&mut self, keys: [u8; 96]) {
        self.aes_keys = Some(keys);
    }

    /// Enables or disables the use of twig files.
    ///
    /// Twig files can improve tree traversal performance at the cost of
    /// additional storage space.
    ///
    /// # Arguments
    /// * `with_twig_file` - Whether to use twig files
    pub fn set_with_twig_file(&mut self, with_twig_file: bool) {
        self.with_twig_file = with_twig_file;
    }
}

