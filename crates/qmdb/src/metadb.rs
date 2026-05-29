//! Metadata database module for QMDB (Quick Merkle Database).
//!
//! This module manages persistent metadata storage for the database, handling critical state
//! information and blockchain-specific data. It provides:
//!
//! # Core Features
//! - Persistent state management
//! - Secure metadata encryption (AES-GCM)
//! - Block height tracking
//! - Merkle tree state persistence
//! - Pruning history management
//! - File size and position tracking
//! - Serial number management
//! - Extra data storage per block
//!
//! # Architecture
//! The metadata system consists of several components:
//!
//! ## State Management
//! - Current block height and state
//! - Root hash history
//! - Edge node tracking
//! - File size monitoring
//!
//! ## Storage Layout
//! Two primary files:
//! 1. Info File (`info.{0,1}`)
//!    - Current state information
//!    - Alternates between two files for safety
//!    - Optionally encrypted
//!
//! 2. History File (`prune_helper`)
//!    - Pruning history
//!    - Block height mappings
//!    - Twig ID tracking
//!
//! ## Security
//! - Optional AES-GCM encryption
//! - Nonce generation per block
//! - Authenticated encryption
//! - Secure state transitions
//!
//! # Performance
//! - Efficient state updates
//! - Minimal disk I/O
//! - Optimized serialization
//! - Background pruning
//!
//! # Example Usage
//! ```no_run
//! use qmdb::metadb::MetaDB;
//! use aes_gcm::Aes256Gcm;
//!
//! // Create a new metadata database
//! let mut db = MetaDB::initialize_dir(
//!     "/path/to/data",
//!     None, // Optional encryption cipher
//!     2,    // MetaDB info file count
//! );
//!
//! // Update state
//! db.set_curr_height(1000);
//! db.set_root_hash(0, [0; 32]);
//!
//! // Commit changes
//! let state = db.commit();
//! ```
//!
//! # Error Handling
//! The module handles various error conditions:
//! - File I/O errors
//! - Encryption/decryption failures
//! - Serialization issues
//! - Invalid state transitions
//!
//! # Best Practices
//! 1. Regular commits
//! 2. Proper error handling
//! 3. Secure key management
//! 4. Backup strategy
//! 5. Monitoring state size

use aes_gcm::aead::AeadInPlace;
use aes_gcm::Aes256Gcm;
use byteorder::{ByteOrder, LittleEndian};
use dashmap::DashMap;
use log::warn;
use qmdb_common::def::{NONCE_SIZE, PRUNE_EVERY_NBLOCKS, SHARD_COUNT, TAG_SIZE, TWIG_SHIFT};
use std::{fs, path::Path, sync::Arc};

use std::io::{Read, Write};

use std::fs::File;

#[cfg(unix)]
use std::os::unix::fs::FileExt;

/// Metadata information for the database state.
///
/// This struct contains all metadata needed to track the database's state,
/// including block heights, serial numbers, root hashes, and file sizes.
/// It provides a complete snapshot of the database's state at a given point.
///
/// # State Components
/// - Block height tracking
/// - Serial number management
/// - Root hash history
/// - File size monitoring
/// - Pruning information
/// - Edge node storage
///
/// # Thread Safety
/// - Safe to clone
/// - Serializable
/// - Immutable after creation
///
/// # Performance
/// - Efficient serialization
/// - Minimal memory footprint
/// - Fast state transitions
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct MetaInfo {
    /// Current block height
    pub curr_height: i64,
    /// Last pruned twig ID and entry file position for each shard
    pub last_pruned_twig: [(u64, i64); SHARD_COUNT],
    /// Next serial number to assign for each shard
    pub next_serial_num: [u64; SHARD_COUNT],
    /// Oldest active serial number for each shard
    pub oldest_active_sn: [u64; SHARD_COUNT],
    /// Oldest active file position for each shard
    pub oldest_active_file_pos: [i64; SHARD_COUNT],
    /// Current root hash for each shard
    pub root_hash: [[u8; 32]; SHARD_COUNT],
    /// Historical root hashes by block height
    pub root_hash_by_height: Vec<[u8; 32]>,
    /// Edge nodes for each shard's Merkle tree
    pub edge_nodes: [Vec<u8>; SHARD_COUNT],
    /// Size of twig files for each shard
    pub twig_file_sizes: [i64; SHARD_COUNT],
    /// Size of entry files for each shard
    pub entry_file_sizes: [i64; SHARD_COUNT],
    /// First twig ID and entry file size at each pruning height
    pub first_twig_at_height: [(u64, i64); SHARD_COUNT],
    /// Extra data stored at the current height
    pub extra_data: String,
}

impl MetaInfo {
    /// Creates a new MetaInfo instance with default values.
    ///
    /// Initializes a fresh metadata state with:
    /// - Zero block height
    /// - Empty root hashes
    /// - Zero file sizes
    /// - Default serial numbers
    /// - Empty edge nodes
    /// - No pruning history
    fn new() -> Self {
        Self {
            curr_height: 0,
            last_pruned_twig: [(0, 0); SHARD_COUNT],
            next_serial_num: [0; SHARD_COUNT],
            oldest_active_sn: [0; SHARD_COUNT],
            oldest_active_file_pos: [0; SHARD_COUNT],
            root_hash: [[0; 32]; SHARD_COUNT],
            root_hash_by_height: vec![],
            edge_nodes: Default::default(),
            twig_file_sizes: [0; SHARD_COUNT],
            entry_file_sizes: [0; SHARD_COUNT],
            first_twig_at_height: [(0, 0); SHARD_COUNT],
            extra_data: "".to_owned(),
        }
    }

    /// Prints all fields in a human-friendly format.
    ///
    /// - Byte arrays are rendered in hexadecimal
    /// - Large byte vectors show length and a short hex preview
    pub fn print(&self) {
        let to_hex = |bytes: &[u8]| {
            let mut s = String::with_capacity(bytes.len() * 2);
            for b in bytes {
                s.push_str(&format!("{:02x}", b));
            }
            s
        };

        let fmt_u64_arr = |arr: &[u64; SHARD_COUNT]| format!("{:?}", arr);
        let fmt_i64_arr = |arr: &[i64; SHARD_COUNT]| format!("{:?}", arr);
        let fmt_tuple_arr = |arr: &[(u64, i64); SHARD_COUNT]| format!("{:?}", arr);
        let fmt_root_hash = |arr: &[[u8; 32]; SHARD_COUNT]| {
            let v: Vec<String> = arr.iter().map(|h| to_hex(h)).collect();
            format!("[{}]", v.join(", "))
        };
        let fmt_root_hash_hist = |v: &Vec<[u8; 32]>| {
            if v.is_empty() {
                "len=0".to_owned()
            } else {
                let last_hex = to_hex(&v[v.len() - 1]);
                format!("len={}, last={}", v.len(), last_hex)
            }
        };
        let fmt_edge_nodes = |arr: &[Vec<u8>; SHARD_COUNT]| {
            let parts: Vec<String> = arr
                .iter()
                .map(|v| {
                    if v.is_empty() {
                        "len=0".to_owned()
                    } else {
                        let preview_len = v.len().min(16);
                        let head_hex = to_hex(&v[..preview_len]);
                        if v.len() > preview_len {
                            format!("len={}, head={}...", v.len(), head_hex)
                        } else {
                            format!("len={}, head={}", v.len(), head_hex)
                        }
                    }
                })
                .collect();
            format!("[{}]", parts.join(", "))
        };

        let header_field = "Field";
        println!("{:<28} | {}", header_field, "Value");
        println!("{}", "-".repeat(28 + 3 + 48));

        println!("{:<28} | {}", "curr_height", self.curr_height);
        println!(
            "{:<28} | {}",
            "last_pruned_twig",
            fmt_tuple_arr(&self.last_pruned_twig)
        );
        println!(
            "{:<28} | {}",
            "next_serial_num",
            fmt_u64_arr(&self.next_serial_num)
        );
        println!(
            "{:<28} | {}",
            "oldest_active_sn",
            fmt_u64_arr(&self.oldest_active_sn)
        );
        println!(
            "{:<28} | {}",
            "oldest_active_file_pos",
            fmt_i64_arr(&self.oldest_active_file_pos)
        );
        println!(
            "{:<28} | {}",
            "twig_file_sizes",
            fmt_i64_arr(&self.twig_file_sizes)
        );
        println!(
            "{:<28} | {}",
            "entry_file_sizes",
            fmt_i64_arr(&self.entry_file_sizes)
        );
        println!(
            "{:<28} | {}",
            "first_twig_at_height",
            fmt_tuple_arr(&self.first_twig_at_height)
        );
        println!("{:<28} | {}", "root_hash", fmt_root_hash(&self.root_hash));
        println!(
            "{:<28} | {}",
            "root_hash_by_height",
            fmt_root_hash_hist(&self.root_hash_by_height)
        );
        println!(
            "{:<28} | {}",
            "edge_nodes",
            fmt_edge_nodes(&self.edge_nodes)
        );
        println!("{:<28} | {}", "extra_data", self.extra_data);
    }
}

/// Metadata database managing persistent state information.
///
/// This struct provides methods to read and write metadata to disk,
/// optionally encrypting sensitive information using AES-GCM.
///
/// # Core Features
/// - Persistent state storage
/// - Optional encryption
/// - Atomic updates
/// - History tracking
/// - Extra data storage
///
/// # Performance
/// - Efficient state updates
/// - Minimal disk I/O
/// - Background pruning
/// - Optimized serialization
///
/// # Thread Safety
/// - Safe concurrent access
/// - Protected state updates
/// - Atomic operations
pub struct MetaDB {
    /// Current metadata state
    info: MetaInfo,
    /// Path to the metadata file
    meta_file_name: String,
    /// Number of rotated metadata info files to keep (info.0..info.N-1)
    meta_file_count: usize,
    /// File handle for pruning history
    history_file: File,
    /// Map of extra data by block height
    extra_data_map: Arc<DashMap<i64, String>>,
    /// Optional encryption cipher
    cipher: Option<Aes256Gcm>,
}

/// Reads an entire file into a byte vector.
///
/// # Arguments
/// * `filename` - Path to the file to read
///
/// # Returns
/// The file contents as a byte vector
///
/// # Performance
/// - Single read operation
/// - Pre-allocated buffer
/// - Memory efficient
///
/// # Error Handling
/// - Reports file access errors
/// - Handles missing files
/// - Manages buffer overflow
fn get_file_as_byte_vec(filename: &String) -> Vec<u8> {
    let mut f = File::open(filename).expect("no file found");
    let metadata = fs::metadata(filename).expect("unable to read metadata");
    let mut buffer = vec![0; metadata.len() as usize];
    f.read_exact(&mut buffer).expect("buffer overflow");

    buffer
}

impl MetaDB {
    /// Loads the metadata database from the specified directory.
    pub fn load_from_dir(
        dir_name: &str,
        cipher: Option<Aes256Gcm>,
        meta_file_count: usize,
    ) -> Self {
        Self::open_internal(dir_name, cipher, meta_file_count, true)
    }

    /// Initializes a new metadata database in the specified directory.
    pub fn initialize_dir(
        dir_name: &str,
        cipher: Option<Aes256Gcm>,
        meta_file_count: usize,
    ) -> Self {
        Self::open_internal(dir_name, cipher, meta_file_count, false)
    }

    /// Internal method to open or create the metadata database.
    /// # Arguments
    /// * `dir_name` - Directory path for the metadata files
    /// * `cipher` - Optional AES-GCM cipher for encryption
    /// * `meta_file_count` - Number of rotated metadata info files to keep
    /// * `must_exist` - Whether the directory must already exist
    /// # Returns
    /// A MetaDB instance
    /// # Process Flow
    /// 1. Validate directory existence based on `must_exist`
    /// 2. Open or create metadata files
    /// 3. Initialize MetaDB struct
    /// 4. Load existing metadata if present
    /// # Error Handling
    /// - Handles missing or existing directories
    /// - Reports file I/O errors
    /// - Manages decryption failures
    fn open_internal(
        dir_name: &str,
        cipher: Option<Aes256Gcm>,
        meta_file_count: usize,
        must_exist: bool,
    ) -> Self {
        let path = Path::new(&dir_name);
        if must_exist {
            assert!(path.exists(), "Directory {} not initialized", dir_name);
        } else {
            assert!(!path.exists(), "Directory {} already initialized", dir_name);
            fs::create_dir_all(dir_name).expect("Failed to create directory");
        }

        let meta_file_name = format!("{dir_name}/info");
        let file_name = format!("{dir_name}/prune_helper");
        let history_file = File::options()
            .read(true)
            .write(true)
            .create(!must_exist)
            .truncate(false)
            .open(&file_name)
            .expect("Failed to open prune_helper file");

        let mut res = Self {
            info: MetaInfo::new(),
            meta_file_name,
            meta_file_count,
            history_file,
            extra_data_map: Arc::new(DashMap::new()),
            cipher,
        };

        res.reload_from_file();
        res
    }

    /// Reloads metadata from disk.
    ///
    /// This method reads both metadata files and uses the most recent valid state.
    /// If encryption is enabled, the files are decrypted before deserialization.
    ///
    /// # Process Flow
    /// 1. Read both info files
    /// 2. Decrypt if necessary
    /// 3. Deserialize content
    /// 4. Select latest valid state
    ///
    /// # Error Handling
    /// - Handles missing files
    /// - Reports decryption errors
    /// - Manages invalid states
    /// - Logs warnings
    pub fn reload_from_file(&mut self) {
        let mut best: Option<MetaInfo> = None;
        for idx in 0..self.meta_file_count {
            let name = format!("{}.{}", self.meta_file_name, idx);
            if !Path::new(&name).exists() {
                continue;
            }
            let mut meta_info_bz = get_file_as_byte_vec(&name);
            if self.cipher.is_some() {
                Self::decrypt(&self.cipher, &mut meta_info_bz);
                let size = meta_info_bz.len();
                meta_info_bz = meta_info_bz[8..size - TAG_SIZE].to_owned();
            }
            match bincode::deserialize::<MetaInfo>(&meta_info_bz[..]) {
                Ok(info) => match &best {
                    None => best = Some(info),
                    Some(curr) if info.curr_height > curr.curr_height => best = Some(info),
                    _ => {}
                },
                Err(_) => warn!("Failed to deserialize {name}, ignore it"),
            };
        }

        if let Some(info) = best {
            self.info = info;
        }
    }

    /// Decrypts a metadata buffer using AES-GCM.
    ///
    /// # Arguments
    /// * `cipher` - Optional AES-GCM cipher
    /// * `bz` - Buffer to decrypt in-place
    ///
    /// # Security
    /// - Uses authenticated encryption
    /// - Verifies data integrity
    /// - Handles nonce management
    ///
    /// # Error Handling
    /// - Reports decryption failures
    /// - Handles invalid buffers
    /// - Manages tag verification
    fn decrypt(cipher: &Option<Aes256Gcm>, bz: &mut [u8]) {
        if bz.len() < TAG_SIZE + 8 {
            panic!("meta db file size not correct")
        }
        let cipher = (*cipher).as_ref().unwrap();
        let mut nonce_arr = [0u8; NONCE_SIZE];
        nonce_arr[..8].copy_from_slice(&bz[0..8]);
        let tag_start = bz.len() - TAG_SIZE;
        let mut tag = [0u8; TAG_SIZE];
        tag[..].copy_from_slice(&bz[tag_start..]);
        let payload = &mut bz[8..tag_start];
        if let Err(e) =
            cipher.decrypt_in_place_detached(&nonce_arr.into(), b"", payload, &tag.into())
        {
            panic!("{e:?}")
        };
    }

    /// Gets the extra data stored at the current height.
    ///
    /// # Returns
    /// The extra data string stored at the current block height
    ///
    /// # Performance
    /// - Direct access
    /// - No disk I/O
    /// - No deserialization
    pub fn get_extra_data(&self) -> String {
        self.info.extra_data.clone()
    }

    /// Inserts extra data for a specific block height.
    ///
    /// # Arguments
    /// * `height` - Block height to store data for
    /// * `data` - Data to store
    ///
    /// # Thread Safety
    /// - Thread-safe storage
    /// - Atomic updates
    /// - Concurrent access
    pub fn insert_extra_data(&mut self, height: i64, data: String) {
        self.extra_data_map.insert(height, data);
    }

    /// Commits the current state to disk.
    ///
    /// This method:
    /// 1. Writes the current state to the metadata file
    /// 2. Encrypts the data if a cipher is configured
    /// 3. Updates pruning history at regular intervals
    ///
    /// # Returns
    /// An Arc reference to the committed state
    ///
    /// # Process Flow
    /// 1. Serialize current state
    /// 2. Apply encryption if enabled
    /// 3. Write to alternating files
    /// 4. Update pruning history
    ///
    /// # Performance
    /// - Efficient serialization
    /// - Batched writes
    /// - Minimal disk I/O
    ///
    /// # Error Handling
    /// - Reports write errors
    /// - Handles encryption failures
    /// - Manages file system issues
    pub fn commit(&mut self) -> Arc<MetaInfo> {
        let kv = self.extra_data_map.remove(&self.info.curr_height).unwrap();
        self.info.extra_data = kv.1;
        let meta_file_count = if self.meta_file_count == 0 {
            1
        } else {
            self.meta_file_count
        };
        let name = format!(
            "{}.{}",
            self.meta_file_name,
            (self.info.curr_height as usize) % meta_file_count
        );
        let mut bz = bincode::serialize(&self.info).unwrap();
        if self.cipher.is_some() {
            let cipher = self.cipher.as_ref().unwrap();
            let mut nonce_arr = [0u8; NONCE_SIZE];
            LittleEndian::write_i64(&mut nonce_arr[..8], self.info.curr_height);
            match cipher.encrypt_in_place_detached(&nonce_arr.into(), b"", &mut bz) {
                Err(err) => panic!("{}", err),
                Ok(tag) => {
                    let mut out = vec![];
                    out.extend_from_slice(&nonce_arr[0..8]);
                    out.extend_from_slice(&bz);
                    out.extend_from_slice(tag.as_slice());
                    fs::write(&name, out).unwrap();
                }
            };
        } else {
            fs::write(&name, bz).unwrap();
        }
        if self.info.curr_height % PRUNE_EVERY_NBLOCKS == 0 && self.info.curr_height > 0 {
            let mut data = [0u8; SHARD_COUNT * 16];
            for shard_id in 0..SHARD_COUNT {
                let start = shard_id * 16;
                let (twig_id, entry_file_size) = self.info.first_twig_at_height[shard_id];
                LittleEndian::write_u64(&mut data[start..start + 8], twig_id);
                LittleEndian::write_u64(&mut data[start + 8..start + 16], entry_file_size as u64);
                if self.cipher.is_some() {
                    let cipher = self.cipher.as_ref().unwrap();
                    let n = self.info.curr_height / PRUNE_EVERY_NBLOCKS;
                    let pos = (((n as usize - 1) * SHARD_COUNT) + shard_id) * (16 + TAG_SIZE);
                    let mut nonce_arr = [0u8; NONCE_SIZE];
                    LittleEndian::write_u64(&mut nonce_arr[..8], pos as u64);
                    match cipher.encrypt_in_place_detached(
                        &nonce_arr.into(),
                        b"",
                        &mut data[start..start + 16],
                    ) {
                        Err(err) => panic!("{}", err),
                        Ok(tag) => {
                            self.history_file
                                .write_all(&data[start..start + 16])
                                .unwrap();
                            self.history_file.write_all(tag.as_slice()).unwrap();
                        }
                    };
                }
            }
            if self.cipher.is_none() {
                self.history_file.write_all(&data[..]).unwrap();
            }
        }
        Arc::new(self.info.clone())
    }

    /// Sets the current block height.
    ///
    /// # Arguments
    /// * `h` - New block height
    ///
    /// # State Changes
    /// - Updates current height
    /// - Triggers pruning checks
    /// - Updates history
    pub fn set_curr_height(&mut self, h: i64) {
        self.info.curr_height = h;
    }

    /// Gets the current block height.
    ///
    /// # Returns
    /// The current block height
    ///
    /// # Performance
    /// - Direct access
    /// - No disk I/O
    /// - No synchronization
    pub fn get_curr_height(&self) -> i64 {
        self.info.curr_height
    }

    /// Sets the twig file size for a shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    /// * `size` - New file size
    ///
    /// # State Changes
    /// - Updates file size
    /// - Affects pruning decisions
    /// - Impacts storage metrics
    pub fn set_twig_file_size(&mut self, shard_id: usize, size: i64) {
        self.info.twig_file_sizes[shard_id] = size;
    }

    /// Gets the twig file size for a shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    ///
    /// # Returns
    /// The current twig file size
    ///
    /// # Performance
    /// - Direct access
    /// - No disk I/O
    /// - No synchronization
    pub fn get_twig_file_size(&self, shard_id: usize) -> i64 {
        self.info.twig_file_sizes[shard_id]
    }

    /// Sets the entry file size for a shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    /// * `size` - New file size
    ///
    /// # State Changes
    /// - Updates file size
    /// - Affects pruning decisions
    /// - Impacts storage metrics
    pub fn set_entry_file_size(&mut self, shard_id: usize, size: i64) {
        self.info.entry_file_sizes[shard_id] = size;
    }

    /// Gets the entry file size for a shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    ///
    /// # Returns
    /// The current entry file size
    ///
    /// # Performance
    /// - Direct access
    /// - No disk I/O
    /// - No synchronization
    pub fn get_entry_file_size(&self, shard_id: usize) -> i64 {
        self.info.entry_file_sizes[shard_id]
    }

    /// Sets the first twig ID and entry file size at a specific height.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    /// * `height` - Block height
    /// * `twig_id` - First twig ID at this height
    /// * `entry_file_size` - Entry file size at this height
    ///
    /// # State Changes
    /// - Updates twig mapping
    /// - Records file size
    /// - Updates pruning info
    ///
    /// # Performance
    /// - Direct update
    /// - No disk I/O
    /// - No validation
    pub fn set_first_twig_at_height(
        &mut self,
        shard_id: usize,
        height: i64,
        twig_id: u64,
        entry_file_size: i64,
    ) {
        if height % PRUNE_EVERY_NBLOCKS == 0 {
            self.info.first_twig_at_height[shard_id] = (twig_id, entry_file_size);
        }
    }

    /// Gets the first twig ID and entry file size at a specific height.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    /// * `height` - Block height
    ///
    /// # Returns
    /// A tuple containing:
    /// - First twig ID at this height
    /// - Entry file size at this height
    ///
    /// # Performance
    /// - Direct access
    /// - No validation
    pub fn get_first_twig_at_height(&self, shard_id: usize, height: i64) -> (u64, i64) {
        let n = height / PRUNE_EVERY_NBLOCKS;
        let mut pos = (((n as usize - 1) * SHARD_COUNT) + shard_id) * 16;
        if self.cipher.is_some() {
            pos = (((n as usize - 1) * SHARD_COUNT) + shard_id) * (16 + TAG_SIZE);
        }
        let mut buf = [0u8; 32];
        if self.cipher.is_some() {
            self.history_file.read_at(&mut buf, pos as u64).unwrap();
            let cipher = self.cipher.as_ref().unwrap();
            let mut nonce_arr = [0u8; NONCE_SIZE];
            LittleEndian::write_u64(&mut nonce_arr[..8], pos as u64);
            let mut tag = [0u8; TAG_SIZE];
            tag.copy_from_slice(&buf[16..]);
            if let Err(e) = cipher.decrypt_in_place_detached(
                &nonce_arr.into(),
                b"",
                &mut buf[0..16],
                &tag.into(),
            ) {
                panic!("{e:?}")
            };
        } else {
            self.history_file
                .read_at(&mut buf[..16], pos as u64)
                .unwrap();
        }
        let twig_id = LittleEndian::read_u64(&buf[..8]);
        let entry_file_size = LittleEndian::read_u64(&buf[8..16]);
        (twig_id, entry_file_size as i64)
    }

    /// Sets the last pruned twig information for a shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    /// * `twig_id` - ID of the last pruned twig
    /// * `ef_prune_to` - Entry file position up to which pruning is complete
    ///
    /// # State Changes
    /// - Updates pruning state
    /// - Records file position
    /// - Updates metrics
    pub fn set_last_pruned_twig(&mut self, shard_id: usize, twig_id: u64, ef_prune_to: i64) {
        self.info.last_pruned_twig[shard_id] = (twig_id, ef_prune_to);
    }

    /// Gets the last pruned twig information for a shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    ///
    /// # Returns
    /// A tuple containing:
    /// - ID of the last pruned twig
    /// - Entry file position up to which pruning is complete
    ///
    /// # Performance
    /// - Direct access
    /// - No disk I/O
    /// - No validation
    pub fn get_last_pruned_twig(&self, shard_id: usize) -> (u64, i64) {
        self.info.last_pruned_twig[shard_id]
    }

    /// Gets the edge nodes for a shard's Merkle tree.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    ///
    /// # Returns
    /// Vector of edge node bytes
    ///
    /// # Performance
    /// - Direct access
    /// - No disk I/O
    /// - No validation
    pub fn get_edge_nodes(&self, shard_id: usize) -> Vec<u8> {
        self.info.edge_nodes[shard_id].clone()
    }

    /// Sets the edge nodes for a shard's Merkle tree.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    /// * `bz` - Edge node bytes to store
    ///
    /// # State Changes
    /// - Updates edge nodes
    /// - Affects tree structure
    /// - Impacts proofs
    pub fn set_edge_nodes(&mut self, shard_id: usize, bz: &[u8]) {
        self.info.edge_nodes[shard_id] = bz.to_vec();
    }

    /// Gets the next serial number for a shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    ///
    /// # Returns
    /// The next available serial number
    ///
    /// # Performance
    /// - Direct access
    /// - No disk I/O
    /// - No synchronization
    pub fn get_next_serial_num(&self, shard_id: usize) -> u64 {
        self.info.next_serial_num[shard_id]
    }

    /// Gets the youngest twig ID for a shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    ///
    /// # Returns
    /// The ID of the most recently created twig
    ///
    /// # Performance
    /// - Direct access
    /// - No disk I/O
    /// - No validation
    pub fn get_youngest_twig_id(&self, shard_id: usize) -> u64 {
        self.info.next_serial_num[shard_id] >> TWIG_SHIFT
    }

    /// Sets the next serial number for a shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    /// * `sn` - Next serial number to use
    ///
    /// # State Changes
    /// - Updates serial counter
    /// - Affects entry ordering
    /// - Impacts compaction
    pub fn set_next_serial_num(&mut self, shard_id: usize, sn: u64) {
        // called when new entry is appended
        self.info.next_serial_num[shard_id] = sn
    }

    /// Gets the root hash for a shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    ///
    /// # Returns
    /// The current root hash
    ///
    /// # Performance
    /// - Direct access
    /// - No disk I/O
    /// - No validation
    pub fn get_root_hash(&self, shard_id: usize) -> [u8; 32] {
        self.info.root_hash[shard_id]
    }

    /// Sets the root hash for a shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    /// * `h` - New root hash
    ///
    /// # State Changes
    /// - Updates root hash
    /// - Affects tree state
    /// - Impacts verification
    pub fn set_root_hash(&mut self, shard_id: usize, h: [u8; 32]) {
        self.info.root_hash[shard_id] = h
    }

    /// Gets the hash of root hashes at a specific height.
    ///
    /// # Arguments
    /// * `height` - Block height
    ///
    /// # Returns
    /// The combined hash of all shard root hashes
    ///
    /// # Performance
    /// - Direct access
    /// - No disk I/O
    /// - No validation
    pub fn get_hash_of_root_hash(&self, height: i64) -> [u8; 32] {
        let mut is_prev_height = true;
        if height == self.info.curr_height {
            is_prev_height = false;
        }
        let l = self.info.root_hash_by_height.len();
        if l == 2 {
            if is_prev_height {
                self.info.root_hash_by_height[0]
            } else {
                self.info.root_hash_by_height[1]
            }
        } else if l == 1 {
            if is_prev_height {
                [0u8; 32]
            } else {
                self.info.root_hash_by_height[0]
            }
        } else {
            [0u8; 32]
        }
    }

    /// Gets the oldest active serial number for a shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    ///
    /// # Returns
    /// The oldest active serial number
    ///
    /// # Performance
    /// - Direct access
    /// - No disk I/O
    /// - No validation
    pub fn get_oldest_active_sn(&self, shard_id: usize) -> u64 {
        self.info.oldest_active_sn[shard_id]
    }

    /// Sets the oldest active serial number for a shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    /// * `id` - New oldest active serial number
    ///
    /// # State Changes
    /// - Updates active range
    /// - Affects pruning
    /// - Impacts compaction
    pub fn set_oldest_active_sn(&mut self, shard_id: usize, id: u64) {
        self.info.oldest_active_sn[shard_id] = id
    }

    /// Gets the oldest active file position for a shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    ///
    /// # Returns
    /// The oldest active file position
    ///
    /// # Performance
    /// - Direct access
    /// - No disk I/O
    /// - No validation
    pub fn get_oldest_active_file_pos(&self, shard_id: usize) -> i64 {
        self.info.oldest_active_file_pos[shard_id]
    }

    /// Sets the oldest active file position for a shard.
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard
    /// * `pos` - New oldest active file position
    ///
    /// # State Changes
    /// - Updates file position
    /// - Affects pruning
    /// - Impacts compaction
    pub fn set_oldest_active_file_pos(&mut self, shard_id: usize, pos: i64) {
        self.info.oldest_active_file_pos[shard_id] = pos
    }

    /// Initializes the metadata database.
    ///
    /// # State Changes
    /// - Resets block height
    /// - Clears root hashes
    /// - Initializes counters
    /// - Prepares storage
    ///
    /// # Performance
    /// - Fast initialization
    /// - Minimal disk I/O
    /// - No validation
    pub fn init(&mut self) {
        let curr_height = 0;
        self.info.curr_height = curr_height;
        for i in 0..SHARD_COUNT {
            self.info.last_pruned_twig[i] = (0, 0);
            self.info.next_serial_num[i] = 0;
            self.info.oldest_active_sn[i] = 0;
            self.info.oldest_active_file_pos[i] = 0;
            self.set_twig_file_size(i, 0);
            self.set_entry_file_size(i, 0);
        }
        self.extra_data_map.insert(curr_height, "".to_owned());
        self.commit();
    }

    pub fn get_info(&self) -> &MetaInfo {
        &self.info
    }
}

#[cfg(test)]
mod tests {
    use crate::test_helper::cipher::create_cipher;

    use super::*;
    use serial_test::serial;
    use tempfile::Builder;

    fn create_metadb(cipher: Option<Aes256Gcm>) -> (MetaDB, tempfile::TempDir) {
        let temp_dir = Builder::new().prefix("testdb.db").tempdir().unwrap();
        let mdb = MetaDB::initialize_dir(temp_dir.path().join("meta").to_str().unwrap(), cipher, 2);
        (mdb, temp_dir)
    }

    #[test]
    #[serial]
    fn test_metadb_init() {
        let cipher = create_cipher();
        let (mut mdb, _temp_dir) = create_metadb(cipher);
        mdb.init();
        mdb.reload_from_file();

        assert_eq!(0, mdb.get_curr_height());
        for i in 0..SHARD_COUNT {
            assert_eq!((0, 0), mdb.get_last_pruned_twig(i));
            assert_eq!(0, mdb.get_next_serial_num(i));
            assert_eq!(0, mdb.get_oldest_active_sn(i));
            assert_eq!(0, mdb.get_oldest_active_file_pos(i));
            assert_eq!(0, mdb.get_twig_file_size(i));
            assert_eq!(0, mdb.get_entry_file_size(i));
            assert_eq!([0u8; 32], mdb.get_root_hash(i));
            assert_eq!(vec![0u8; 0], mdb.get_edge_nodes(i));
        }
    }

    #[test]
    #[serial]
    fn test_metadb() {
        let (mut mdb, _temp_dir) = create_metadb(None);

        for i in 0..SHARD_COUNT {
            mdb.set_curr_height(12345);
            mdb.set_last_pruned_twig(i, 1000 + i as u64, 7000 + i as i64);
            mdb.set_next_serial_num(i, 2000 + i as u64);
            mdb.set_oldest_active_sn(i, 3000 + i as u64);
            mdb.set_oldest_active_file_pos(i, 4000 + i as i64);
            mdb.set_twig_file_size(i, 5000 + i as i64);
            mdb.set_entry_file_size(i, 6000 + i as i64);
            mdb.set_root_hash(i, [i as u8; 32]);
            mdb.set_edge_nodes(i, &[i as u8; 8]);
            mdb.set_first_twig_at_height(i, 100 + i as i64, 200 + i as u64, 0);
        }
        mdb.extra_data_map.insert(12345, "test".to_owned());
        mdb.commit();
        mdb.reload_from_file();

        assert_eq!(12345, mdb.get_curr_height());
        for i in 0..SHARD_COUNT {
            assert_eq!(
                (1000 + i as u64, 7000 + i as i64),
                mdb.get_last_pruned_twig(i)
            );
            assert_eq!(2000 + i as u64, mdb.get_next_serial_num(i));
            assert_eq!(3000 + i as u64, mdb.get_oldest_active_sn(i));
            assert_eq!(4000 + i as i64, mdb.get_oldest_active_file_pos(i));
            assert_eq!(5000 + i as i64, mdb.get_twig_file_size(i));
            assert_eq!(6000 + i as i64, mdb.get_entry_file_size(i));
            assert_eq!([i as u8; 32], mdb.get_root_hash(i));
            assert_eq!(vec![i as u8; 8], mdb.get_edge_nodes(i));
        }
        assert_eq!("test", mdb.get_extra_data());
    }
}
