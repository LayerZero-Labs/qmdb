//! Persistent storage implementation for Merkle tree twigs.
//!
//! This module provides functionality to store and retrieve Merkle tree twigs from disk.
//! To optimize storage space, only certain levels of the tree are persisted:
//! - Top levels (1-255): Used frequently for proof verification
//! - Leaf nodes (2048-4095): Required for data integrity
//! - Middle levels (256-2047): Computed on-the-fly when needed
//!
//! The storage format for each twig includes:
//! 1. A 12-byte header containing:
//!    - 8 bytes: Last entry end position
//!    - 4 bytes: XXH32 checksum of the position
//! 2. Node hashes for top levels (1-255)
//! 3. Node hashes for leaf nodes (2048-4095)
//!
//! Tree Level Structure:
//! ```text
//! Level_11  =  1         (Root)
//! Level_10  =  2-3      (Top levels - stored)
//! Level_9   =  4-7      (Top levels - stored)
//! Level_8   =  8-15     (Top levels - stored)
//! Level_7   =  16-31    (Top levels - stored)
//! Level_6   =  32-63    (Top levels - stored)
//! Level_5   =  64-127   (Top levels - stored)
//! Level_4   =  128-255  (Top levels - stored)
//! Level_3   X  256-511  (Middle levels - computed)
//! Level_2   X  512-1023 (Middle levels - computed)
//! Level_1   X  1024-2047 (Middle levels - computed)
//! Level_0   =  2048-4095 (Leaf nodes - stored)
//! ```

use byteorder::{ByteOrder, LittleEndian};
use hpfile::HPFile;
use qmdb_common::def::HPFILE_RANGE;
use qmdb_common::merkletree::hash::merkle_node_hash;
use std::collections::HashMap;
use std::sync::Arc;
use xxhash_rust::xxh32;

/*
We store top levels(1~255) and the leaves(2048~4095), while the middle levels are ignored to save disk
When we need to read the nodes in ignored levels, we must compute their value on-the-fly from leaves
Level_11  =  1
Level_10  =  2~3
Level_9   =  4~7
Level_8   =  8~15
Level_7   =  16~31
Level_6   =  32~63
Level_5   =  64~127
Level_4   =  128~255
Level_3   X  256~511
Level_2   X  512~1023
Level_1   X  1024~2047
Level_0   =  2048~4095
*/

/// Number of nodes in middle levels that are not persisted
const IGNORED_COUNT: u64 = 2048 - 256;

/// Total number of nodes in a complete twig (internal nodes + leaves)
const TWIG_FULL_LENGTH: u64 = 4095;

/// Number of nodes that are actually stored (excluding ignored middle levels)
const TWIG_ENTRY_COUNT: u64 = TWIG_FULL_LENGTH - IGNORED_COUNT;

/// Size of a persisted twig in bytes (12-byte header + 32 bytes per stored node)
pub const TWIG_SIZE: u64 = 12 + TWIG_ENTRY_COUNT * 32;

/// Manages persistent storage of Merkle tree twigs
///
/// Uses HPFile for the underlying storage, which provides:
/// - Efficient buffered I/O
/// - Concurrent read/write access
#[derive(Debug)]
pub struct TwigFile {
    /// The underlying Head-prunable file handle
    pub hp_file: HPFile,
}

impl TwigFile {
    /// Creates a new twig file with the specified parameters
    ///
    /// # Arguments
    /// * `buffer_size` - Size of I/O buffer in bytes
    /// * `segment_size` - Maximum size of each file segment
    /// * `dir_name` - Directory to store the file segments
    ///
    /// # Returns
    /// A new TwigFile instance
    pub fn new(buffer_size: usize, segment_size: i64, dir_name: String) -> TwigFile {
        TwigFile {
            hp_file: HPFile::new(buffer_size as i64, segment_size, dir_name).unwrap(),
        }
    }

    /// Creates an empty twig file for testing
    ///
    /// # Returns
    /// A TwigFile instance with an empty HPFile
    pub fn empty() -> TwigFile {
        TwigFile {
            hp_file: HPFile::empty(),
        }
    }

    /// Checks if the twig file is empty
    ///
    /// # Returns
    /// true if the file is empty, false otherwise
    pub fn is_empty(&self) -> bool {
        self.hp_file.is_empty()
    }

    /// Appends a twig's Merkle tree nodes to the file
    ///
    /// # Arguments
    /// * `m_tree` - Array of node hashes for the entire twig
    /// * `last_entry_end_pos` - End position of the last entry in this twig
    /// * `buffer` - Buffer for writing data
    ///
    /// # Panics
    /// - If last_entry_end_pos is negative
    /// - If m_tree length doesn't match TWIG_FULL_LENGTH + 1
    fn append_twig(&self, m_tree: &[[u8; 32]], last_entry_end_pos: i64, buffer: &mut Vec<u8>) {
        if self.hp_file.is_empty() {
            return;
        }
        if last_entry_end_pos < 0 {
            panic!("Invalid last entry end position: {last_entry_end_pos}");
        }
        if m_tree.len() != TWIG_FULL_LENGTH as usize + 1 {
            panic!("len(m_tree): {} != {}", m_tree.len(), TWIG_FULL_LENGTH);
        }
        // last_entry_end_pos and its 32b hash need 12 bytes
        let mut buf: [u8; 12] = [0; 12];
        LittleEndian::write_i64(&mut buf[0..8], last_entry_end_pos);
        let digest = xxh32::xxh32(&buf[0..8], 0);
        LittleEndian::write_u32(&mut buf[8..], digest);
        _ = self.hp_file.append(&buf[..], buffer);
        // only write the higher levels and leaf nodes, middle levels are ignored
        for i in 1..256 {
            _ = self.hp_file.append(&m_tree[i as usize][..], buffer);
        }
        for i in 2048..TWIG_FULL_LENGTH + 1 {
            _ = self.hp_file.append(&m_tree[i as usize][..], buffer);
        }
    }

    /// Gets the position of the first entry in a twig
    ///
    /// For twig_id > 0, this is the end position of the last entry
    /// in the previous twig.
    ///
    /// # Arguments
    /// * `twig_id` - ID of the twig to query
    ///
    /// # Returns
    /// Position of the first entry in the twig
    ///
    /// # Panics
    /// If the checksum verification fails
    pub fn get_first_entry_pos(&self, mut twig_id: u64) -> i64 {
        if twig_id == 0 {
            return 0;
        }
        // the end pos of previous twig's last entry is the
        // pos of current twig's first entry
        twig_id -= 1;
        let mut buf: [u8; 12] = [0; 12];
        self.hp_file
            .read_at(&mut buf[..], (twig_id * TWIG_SIZE) as i64 % HPFILE_RANGE)
            .unwrap();
        let mut digest = [0; 4];
        LittleEndian::write_u32(&mut digest, xxh32::xxh32(&buf[0..8], 0));
        assert_eq!(buf[8..], digest[..], "Checksum Error!");
        LittleEndian::read_i64(&buf[0..8])
    }

    // for the ignored middle layer, we must calculate the node's value from leaves in a range
    /// Calculates the range of leaf nodes needed to compute a middle-level node
    ///
    /// # Arguments
    /// * `hash_id` - ID of the node to compute
    ///
    /// # Returns
    /// Tuple of (start_leaf_id, end_leaf_id)
    ///
    /// # Panics
    /// If hash_id is not in the middle levels (256-2047)
    pub fn get_leaf_range(hash_id: i64) -> (i64, i64) {
        if (256..512).contains(&hash_id) {
            // level_3 : 256~511
            (hash_id * 8, hash_id * 8 + 8)
        } else if hash_id < 1024 {
            //level_2 : 512~1023
            return ((hash_id / 2) * 8, (hash_id / 2) * 8 + 8);
        } else if hash_id < 2048 {
            //level_1 : 1024~2047
            return ((hash_id / 4) * 8, (hash_id / 4) * 8 + 8);
        } else {
            panic!("Invalid hashID")
        }
    }

    /// Computes a node hash in the middle levels from its leaf nodes
    ///
    /// # Arguments
    /// * `twig_id` - ID of the twig containing the node
    /// * `hash_id` - ID of the node to compute
    /// * `cache` - Cache for storing intermediate node hashes
    /// * `out` - Buffer to store the computed hash
    pub fn get_hash_node_in_ignore_range(
        &self,
        twig_id: u64,
        hash_id: i64,
        cache: &mut HashMap<i64, [u8; 32]>,
        out: &mut [u8; 32],
    ) {
        let (start, end) = Self::get_leaf_range(hash_id);
        let mut buf = [0; 32 * 8];
        let offset =
            twig_id * TWIG_SIZE + 12 + (start as u64 - 1/*because hash_id starts from 1*/) * 32
                - IGNORED_COUNT * 32;
        let offset = (offset as i64) % HPFILE_RANGE;
        let num_bytes_read = self.hp_file.read_at(&mut buf[..], offset).unwrap_or(0);
        if num_bytes_read != buf.len() {
            // Cannot read them in one call because of file-crossing
            for i in 0..8 {
                //read them in 8 steps in case of file-crossing
                self.hp_file
                    .read_at(&mut buf[i * 32..i * 32 + 32], offset + (i * 32) as i64)
                    .unwrap();
            }
        }
        // recover a little cone with 8 leaves into cache
        // this little cone will be queried by 'get_proof'
        let mut level = 0;
        for i in start / 2..end / 2 {
            let off = ((i - start / 2) * 64) as usize;
            let v = merkle_node_hash(level, &buf[off..off + 32], &buf[off + 32..off + 64]);
            cache.insert(i, v);
        }
        level = 1;
        for i in start / 4..end / 4 {
            let v = merkle_node_hash(
                level,
                cache.get(&(i * 2)).unwrap(),
                cache.get(&(i * 2 + 1)).unwrap(),
            );
            cache.insert(i, v);
        }
        level = 2;
        let id = start / 8;
        let v = merkle_node_hash(
            level,
            cache.get(&(id * 2)).unwrap(),
            cache.get(&(id * 2 + 1)).unwrap(),
        );
        cache.insert(id, v);
        out.copy_from_slice(cache.get(&hash_id).unwrap().as_slice());
    }

    /// Gets the root hash of a twig
    ///
    /// # Arguments
    /// * `twig_id` - ID of the twig
    /// * `buf` - Buffer to store the root hash
    pub fn get_hash_root(&self, twig_id: u64, buf: &mut [u8; 32]) {
        self.get_hash_node(twig_id, 1, &mut HashMap::new(), buf)
    }

    /// Gets a node's hash from the twig file
    ///
    /// For nodes in middle levels, computes the hash from leaf nodes.
    /// For other nodes, reads the hash directly from storage.
    ///
    /// # Arguments
    /// * `twig_id` - ID of the twig containing the node
    /// * `hash_id` - ID of the node to retrieve
    /// * `cache` - Cache for storing intermediate node hashes
    /// * `buf` - Buffer to store the node hash
    ///
    /// # Panics
    /// If hash_id is invalid (≤ 0 or ≥ 4096)
    pub fn get_hash_node<'a>(
        &'a self,
        twig_id: u64,
        hash_id: i64,
        cache: &'a mut HashMap<i64, [u8; 32]>,
        buf: &mut [u8; 32],
    ) {
        if hash_id <= 0 || hash_id >= 4096 {
            panic!("Invalid hashID: {hash_id}");
        }
        if (256..2048).contains(&hash_id) {
            return self.get_hash_node_in_ignore_range(twig_id, hash_id, cache, buf);
        }
        let mut offset =
            twig_id * TWIG_SIZE + 12 + (hash_id as u64 - 1/*because hash_id starts from 1*/) * 32;
        if hash_id >= 2048 {
            offset -= IGNORED_COUNT * 32;
        }
        let offset = (offset as i64) % HPFILE_RANGE;
        self.hp_file
            .read_at(&mut buf[..], offset)
            .expect("TODO: panic message");
    }

    /// Truncates the twig file to the specified size
    ///
    /// # Arguments
    /// * `size` - New size in bytes
    ///
    /// # Panics
    /// If the truncate operation fails
    pub fn truncate(&self, size: i64) {
        if let Err(err) = self.hp_file.truncate(size) {
            panic!("{}", err)
        }
    }

    /// Closes the twig file
    pub fn close(&self) {
        self.hp_file.close();
    }

    /// Prunes the head of the twig file
    ///
    /// # Arguments
    /// * `off` - Offset to prune from
    ///
    /// # Panics
    /// If the prune operation fails
    pub fn prune_head(&self, off: i64) {
        if let Err(err) = self.hp_file.prune_head(off) {
            panic!("{}", err)
        }
    }
}

/// Writer interface for TwigFile that provides buffered writes
pub struct TwigFileWriter {
    /// Reference to the underlying twig file
    pub twig_file: Arc<TwigFile>,
    /// Write buffer to batch operations
    wrbuf: Vec<u8>,
}

impl TwigFileWriter {
    /// Creates a new writer for the given twig file
    ///
    /// # Arguments
    /// * `twig_file` - Reference to the twig file to write to
    /// * `buffer_size` - Size of the write buffer in bytes
    ///
    /// # Returns
    /// A new TwigFileWriter instance
    pub fn new(twig_file: Arc<TwigFile>, buffer_size: usize) -> TwigFileWriter {
        TwigFileWriter {
            twig_file,
            wrbuf: Vec::with_capacity(buffer_size),
        }
    }

    /// Creates a temporary clone of the writer
    ///
    /// The clone shares the same twig file but has an empty write buffer.
    ///
    /// # Returns
    /// A new TwigFileWriter instance
    pub fn temp_clone(&self) -> TwigFileWriter {
        TwigFileWriter {
            twig_file: self.twig_file.clone(),
            wrbuf: Vec::with_capacity(0),
        }
    }

    /// Appends a twig's Merkle tree nodes to the file
    ///
    /// # Arguments
    /// * `m_tree` - Array of node hashes for the entire twig
    /// * `last_entry_end_pos` - End position of the last entry in this twig
    pub fn append_twig(&mut self, m_tree: &[[u8; 32]], last_entry_end_pos: i64) {
        self.twig_file
            .append_twig(m_tree, last_entry_end_pos, &mut self.wrbuf);
    }

    /// Flushes the write buffer to disk
    pub fn flush(&mut self) {
        self.twig_file
            .hp_file
            .flush(&mut self.wrbuf)
            .unwrap();
    }
}

#[cfg(test)]
mod twig_file_test {
    use qmdb_common::merkletree::twig::{sync_mtree, TwigMT};

    use super::*;

    fn generate_twig(mut rand_num: u32, twig: &mut TwigMT) {
        // let mut rand_num = rand_num;
        for item in twig.iter_mut().take(4096).skip(2048) {
            let mut j = 0;
            while j + 4 < 32 {
                LittleEndian::write_u32(&mut item[j..j + 4], rand_num);
                rand_num += 257;
                j += 4;
            }
        }
        sync_mtree(twig, 0, 2048);
    }

    #[test]
    fn twig_file_all() {
        let temp_dir = ::tempfile::Builder::new().prefix("twig").tempdir().unwrap();
        let dir_path = temp_dir.path().to_str().unwrap().to_string();

        let tf = TwigFile::new(64 * 1024, 1024 * 1024, dir_path.clone());

        let mut twigs = [[[0; 32]; 4096]; 3];
        generate_twig(1000, &mut twigs[0]);
        generate_twig(1111111, &mut twigs[1]);
        generate_twig(2222222, &mut twigs[2]);
        let mut buffer = vec![];
        tf.append_twig(&twigs[0][..], 789, &mut buffer);
        tf.append_twig(&twigs[1][..], 1000789, &mut buffer);
        tf.append_twig(&twigs[2][..], 2000789, &mut buffer);

        let _ = tf.hp_file.flush(&mut buffer);
        tf.close();

        let tf = TwigFile::new(64 * 1024, 1024 * 1024, dir_path);
        assert_eq!(0, tf.get_first_entry_pos(0));
        assert_eq!(789, tf.get_first_entry_pos(1));
        assert_eq!(1000789, tf.get_first_entry_pos(2));
        assert_eq!(2000789, tf.get_first_entry_pos(3));

        for twig_id in 0..3 {
            for i in 1..4096 {
                let mut cache = HashMap::<i64, [u8; 32]>::new();
                let mut buf = [0; 32];
                tf.get_hash_node(twig_id, i, &mut cache, &mut buf);
                assert_eq!(buf[..], twigs[twig_id as usize][i as usize][..]);
            }
        }
        for twig_id in 0..3 {
            let mut cache = HashMap::<i64, [u8; 32]>::new();
            for i in 1..4096 {
                if cache.contains_key(&i) {
                    let bz = cache.get(&i).unwrap();
                    assert_eq!(&twigs[twig_id as usize][i as usize][..], bz.as_slice());
                } else {
                    let mut buf = [0; 32];
                    tf.get_hash_node(twig_id, i, &mut cache, &mut buf);
                    assert_eq!(buf[..], twigs[twig_id as usize][i as usize][..]);
                }
            }
        }
        tf.close();
    }
}
