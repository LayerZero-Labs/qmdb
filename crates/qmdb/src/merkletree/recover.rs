//! Implementation of Merkle tree recovery and reconstruction functionality.
//!
//! This module provides mechanisms to:
//! - Recover a tree's state from persisted data
//! - Reconstruct tree nodes from edge nodes
//! - Handle tree recovery after crashes or pruning
//! - Manage entry scanning and reactivation
//!
//! The recovery process involves:
//! 1. Loading edge nodes (boundary nodes after pruning)
//! 2. Scanning and recovering entries
//! 3. Rebuilding twig structures
//! 4. Reconstructing upper tree levels
//! 5. Verifying tree consistency

#![allow(clippy::too_many_arguments)]
use crate::entryfile::EntryFileWithPreReader;

use super::tree::{get_shard_idx_and_key, EdgeNode, NodePos, Tree};
use aes_gcm::Aes256Gcm;
use byteorder::{ByteOrder, LittleEndian};
use log::debug;
use qmdb_common::def::{
    DEFAULT_ENTRY_SIZE, LEAF_COUNT_IN_TWIG, TWIG_MASK, TWIG_ROOT_LEVEL, TWIG_SHIFT,
};
use qmdb_common::merkletree::hash::merkle_node_hash;
use qmdb_common::merkletree::null::get_null_mt_for_twig;
use qmdb_common::merkletree::twig::get_null_twig_box;
use qmdb_common::{
    entryfile::entry::EntryBz,
    merkletree::{activebits::ActiveBits, null::NULL_TWIG},
};

/// Serializes a list of edge nodes to bytes
///
/// The format for each edge node is:
/// - 8 bytes: Position (level and index)
/// - 32 bytes: Hash value
///
/// # Arguments
/// * `edge_nodes` - List of edge nodes to serialize
///
/// # Returns
/// Byte vector containing the serialized edge nodes
pub fn edge_nodes_to_bytes(edge_nodes: &[EdgeNode]) -> Vec<u8> {
    let stride = 8 + 32;
    let mut res = vec![0u8; edge_nodes.len() * stride];
    for (i, node) in edge_nodes.iter().enumerate() {
        LittleEndian::write_u64(&mut res[i * stride..i * stride + 8], node.pos.as_u64());
        res[i * stride + 8..(i + 1) * stride].copy_from_slice(&node.value);
    }
    res
}

/// Deserializes a byte slice into a list of edge nodes
///
/// # Arguments
/// * `bz` - Byte slice containing serialized edge nodes
///
/// # Returns
/// Vector of deserialized edge nodes
///
/// # Panics
/// Panics if the byte slice length is not a multiple of 40 (8 + 32)
pub fn bytes_to_edge_nodes(bz: &[u8]) -> Vec<EdgeNode> {
    let stride = 8 + 32;
    if bz.len() % stride != 0 {
        panic!("Invalid byteslice length for EdgeNodes");
    }
    let len: usize = bz.len() / stride;
    let mut res = Vec::with_capacity(len);
    for i in 0..len {
        let pos = LittleEndian::read_u64(&bz[i * stride..i * stride + 8]);
        let mut edge_node = EdgeNode {
            pos: NodePos::new(pos),
            value: [0u8; 32],
        };
        edge_node
            .value
            .copy_from_slice(&bz[i * stride + 8..(i + 1) * stride]);
        res.push(edge_node);
    }
    res
}

impl Tree {
    /// Recovers a single entry's state in the tree
    ///
    /// This function:
    /// 1. Deactivates any dependent entries
    /// 2. Updates the youngest twig ID
    /// 3. Activates the entry if it's in an active twig
    /// 4. Updates the Merkle tree leaf
    /// 5. Handles twig transitions
    ///
    /// # Arguments
    /// * `entry` - The entry to recover
    /// * `oldest_active_twig_id` - ID of the oldest twig that hasn't been pruned
    /// * `youngest_twig_id` - ID of the most recent twig
    fn recover_entry(&mut self, entry: EntryBz, oldest_active_twig_id: u64, youngest_twig_id: u64) {
        let sn = entry.serial_number();
        let twig_id = sn >> TWIG_SHIFT;
        for (_, sn) in entry.dsn_iter() {
            let twig_id = sn >> TWIG_SHIFT;
            if twig_id >= oldest_active_twig_id {
                self.deactive_entry(sn);
            }
        }

        // update youngestTwigID
        self.youngest_twig_id = twig_id;
        self.youngest_twig_is_empty = false;
        // mark this entry as valid
        if twig_id >= oldest_active_twig_id {
            self.active_entry(sn);
        }
        // record ChangeStart/ChangeEnd for endblock sync
        let position = (sn & TWIG_MASK as u64) as i32;
        if self.mtree_for_yt_change_start == -1 {
            self.mtree_for_yt_change_start = position;
        }
        self.mtree_for_yt_change_end = position;

        // update the corresponding leaf of merkle tree
        let twigmt_idx = LEAF_COUNT_IN_TWIG as usize + position as usize;
        self.mtree_for_youngest_twig[twigmt_idx] = entry.hash();

        if position as u32 == TWIG_MASK {
            // Only the real youngest twig's left_root
            // need this sync_mt_for_youngest_twig call. All the other twigs' left_root
            // can be loaded from TwigFile
            let empty_twig_file = self.twig_file_wr.twig_file.is_empty();
            if twig_id == youngest_twig_id || empty_twig_file {
                self.sync_mt_for_youngest_twig(true);
            } else {
                self.load_mt_for_non_youngest_twig(twig_id);
            }
            self.youngest_twig_id += 1;
            self.youngest_twig_is_empty = true;
            let (s, i) = get_shard_idx_and_key(self.youngest_twig_id);
            self.upper_tree.active_twig_shards[s].insert(i, get_null_twig_box().clone());
            self.active_bit_shards[s].insert(i, ActiveBits::default());
            self.mtree_for_youngest_twig = get_null_mt_for_twig().to_vec();
        };
    }

    /// Scans and recovers entries from the entry file
    ///
    /// This function reads all entries from the specified position and:
    /// 1. Recovers each entry's state
    /// 2. Updates twig structures
    /// 3. Maintains progress tracking
    ///
    /// # Arguments
    /// * `oldest_active_twig_id` - ID of the oldest twig that hasn't been pruned
    /// * `youngest_twig_id` - ID of the most recent twig
    /// * `ef_prune_to` - Position in the entry file to start scanning from
    fn scan_entries(
        &mut self,
        oldest_active_twig_id: u64,
        youngest_twig_id: u64,
        ef_prune_to: i64,
    ) {
        let mut pos = ef_prune_to;
        let mut buf = Vec::with_capacity(DEFAULT_ENTRY_SIZE);
        let mut entry_file = EntryFileWithPreReader::new(&self.entry_file_wr.entry_file);
        let size = self.entry_file_wr.entry_file.size();
        let tf = &self.twig_file_wr.twig_file;
        if !tf.is_empty() {
            pos = tf.get_first_entry_pos(oldest_active_twig_id);
            let read_len = entry_file.read_entry(pos, size, &mut buf).unwrap();
            let _entry = EntryBz {
                bz: &buf[..read_len],
            };
        }
        let total = size - pos;
        let _step = total / 200;
        let _start_pos = pos;
        let mut _last_pos = pos;

        let mut _count = 0;
        let mut got_first_entry = false;
        while pos < size {
            // if (pos - start_pos) / step != (last_pos - start_pos) / step {
            //     println!(
            //         "ScanEntries{} {:.3} {}/{}",
            //         self.my_shard_id,
            //         (pos - start_pos) as f64 / total as f64,
            //         pos - start_pos,
            //         total
            //     );
            // }
            _last_pos = pos;
            let read_len = entry_file.read_entry(pos, size, &mut buf).unwrap();
            let entry = EntryBz {
                bz: &buf[..read_len],
            };
            if !got_first_entry {
                let sn = entry.serial_number();
                let twig_id = sn >> TWIG_SHIFT;
                let (s, i) = get_shard_idx_and_key(twig_id);
                self.upper_tree.active_twig_shards[s].insert(i, get_null_twig_box().clone());
                self.active_bit_shards[s].insert(i, ActiveBits::default());
                got_first_entry = true;
            }
            self.recover_entry(entry, oldest_active_twig_id, youngest_twig_id);
            pos += buf.len() as i64;
            buf.clear();
            _count += 1;
        }
    }

    /// Performs a lightweight scan of entries without full recovery
    ///
    /// This function is useful for operations that only need to read entry data
    /// without modifying the tree state.
    ///
    /// # Arguments
    /// * `oldest_active_twig_id` - ID of the oldest twig that hasn't been pruned
    /// * `ef_prune_to` - Position in the entry file to start scanning from
    /// * `access` - Callback function to process each entry
    pub fn scan_entries_lite<F>(&self, oldest_active_twig_id: u64, ef_prune_to: i64, access: F)
    where
        F: FnMut(&[u8], &[u8], i64, u64),
    {
        let mut pos = ef_prune_to;
        let tf = &self.twig_file_wr.twig_file;
        if !tf.is_empty() {
            pos = tf.get_first_entry_pos(oldest_active_twig_id);
        }
        let mut entry_file_rd = EntryFileWithPreReader::new(&self.entry_file_wr.entry_file);
        entry_file_rd.scan_entries_lite(pos, access);
    }

    /// Recovers all twigs in the tree
    ///
    /// This function:
    /// 1. Scans and recovers entries
    /// 2. Synchronizes the youngest twig's Merkle tree
    /// 3. Updates active bits trees
    /// 4. Returns a list of affected node IDs
    ///
    /// # Arguments
    /// * `oldest_active_twig_id` - ID of the oldest twig that hasn't been pruned
    /// * `youngest_twig_id` - ID of the most recent twig
    /// * `ef_prune_to` - Position in the entry file to start scanning from
    ///
    /// # Returns
    /// List of node IDs that need upper tree synchronization
    fn recover_twigs(
        &mut self,
        oldest_active_twig_id: u64,
        youngest_twig_id: u64,
        ef_prune_to: i64,
    ) -> Vec<u64> {
        self.scan_entries(oldest_active_twig_id, youngest_twig_id, ef_prune_to);
        self.sync_mt_for_youngest_twig(true);
        let res = self.sync_mt_for_active_bits_phase1(oldest_active_twig_id);
        let n_list = self.upper_tree.sync_mt_for_active_bits_phase2(res);
        self.clear_touched_pos();
        n_list
    }

    /// Recovers upper tree nodes from edge nodes
    ///
    /// This function:
    /// 1. Restores edge nodes to their positions
    /// 2. Synchronizes the upper tree
    /// 3. Returns the new root hash
    ///
    /// # Arguments
    /// * `edge_nodes` - List of edge nodes to restore
    /// * `n_list` - List of node IDs that need synchronization
    ///
    /// # Returns
    /// The new root hash after recovery
    fn recover_upper_nodes(&mut self, edge_nodes: &Vec<EdgeNode>, n_list: Vec<u64>) -> [u8; 32] {
        for node in edge_nodes {
            self.upper_tree.set_node_copy(node.pos, &node.value);
        }
        let (_, root_hash) = self
            .upper_tree
            .sync_upper_nodes(n_list, self.youngest_twig_id);
        root_hash
    }

    /// Recovers root hashes for inactive twigs
    ///
    /// This function rebuilds twig root hashes for twigs that are no longer active
    /// but still need to be referenced in the tree structure.
    ///
    /// # Arguments
    /// * `last_pruned_twig_id` - ID of the last twig that was pruned
    /// * `oldest_active_twig_id` - ID of the oldest twig that hasn't been pruned
    fn recover_inactive_twig_roots(
        &mut self,
        last_pruned_twig_id: u64,
        oldest_active_twig_id: u64,
    ) {
        for twig_id in last_pruned_twig_id..oldest_active_twig_id {
            let mut left_root = [0; 32];
            self.twig_file_wr
                .twig_file
                .get_hash_root(twig_id, &mut left_root);
            let twig_root = merkle_node_hash(11, &left_root[..], &NULL_TWIG.active_bits_mtl3[..]);
            let pos = NodePos::pos(TWIG_ROOT_LEVEL as u64, twig_id);
            self.upper_tree.set_node(pos, twig_root);
        }
    }
}

/// Recovers a complete tree from persisted state
///
/// This function rebuilds a tree by:
/// 1. Creating a new blank tree
/// 2. Restoring file sizes
/// 3. Recovering inactive twig roots
/// 4. Rebuilding active twigs
/// 5. Reconstructing upper tree levels
///
/// # Arguments
/// * `shard_id` - ID of the shard this tree belongs to
/// * `buffer_size` - Size of I/O buffers
/// * `segment_size` - Size of file segments
/// * `with_twig_file` - Whether to use twig files
/// * `dir_name` - Directory for persistence files
/// * `suffix` - Suffix for file names
/// * `edge_nodes` - List of edge nodes from pruned subtrees
/// * `last_pruned_twig_id` - ID of the last twig that was pruned
/// * `ef_prune_to` - Position in entry file to start recovery from
/// * `oldest_active_twig_id` - ID of oldest non-pruned twig
/// * `youngest_twig_id` - ID of most recent twig
/// * `file_sizes` - Sizes to restore files to
/// * `cipher` - Optional encryption cipher
///
/// # Returns
/// Tuple of (recovered tree, root hash)
pub fn recover_tree(
    shard_id: usize,
    buffer_size: usize,
    segment_size: usize,
    with_twig_file: bool,
    dir_name: String,
    suffix: String,
    edge_nodes: &Vec<EdgeNode>,
    last_pruned_twig_id: u64,
    ef_prune_to: i64,
    oldest_active_twig_id: u64,
    youngest_twig_id: u64,
    file_sizes: &[i64],
    cipher: Option<Aes256Gcm>,
) -> (Tree, [u8; 32]) {
    let mut tree = Tree::new_blank(
        shard_id,
        buffer_size,
        segment_size as i64,
        dir_name,
        suffix,
        with_twig_file,
        cipher,
    );
    tree.youngest_twig_id = youngest_twig_id;

    if file_sizes.len() == 2 {
        debug!(
            "OldSize entryFile {} twigFile {}",
            tree.entry_file_wr.entry_file.size(),
            tree.twig_file_wr.twig_file.hp_file.size(),
        );
        debug!(
            "NewSize entryFile {} twigFile {}",
            file_sizes[0], file_sizes[1]
        );
        tree.truncate_files(file_sizes[0], file_sizes[1]);
    }

    let (s, i) = get_shard_idx_and_key(oldest_active_twig_id);
    tree.upper_tree.active_twig_shards[s].insert(i, get_null_twig_box().clone());
    tree.active_bit_shards[s].insert(i, ActiveBits::default());
    tree.mtree_for_youngest_twig = get_null_mt_for_twig().to_vec();

    let mut starting_inactive_twig_id = last_pruned_twig_id;
    if last_pruned_twig_id == u64::MAX {
        starting_inactive_twig_id = 0;
    };
    if starting_inactive_twig_id % 2 == 1 {
        starting_inactive_twig_id -= 1;
    }
    // when twig_file is empty, we scan entries from 'ef_prune_to' instead of the oldest active
    // twig, so the inactive twig roots will be recovered in 'recover_twigs' instead of here
    if !tree.twig_file_wr.twig_file.is_empty() {
        tree.recover_inactive_twig_roots(starting_inactive_twig_id, oldest_active_twig_id);
    }

    let mut incomplete_twig = None;
    for edge_node in edge_nodes {
        if edge_node.pos.level() == TWIG_ROOT_LEVEL as u64 {
            incomplete_twig = Some(edge_node.pos.nth());
            if starting_inactive_twig_id < edge_node.pos.nth() {
                starting_inactive_twig_id = edge_node.pos.nth();
            }
            break;
        }
    }

    let mut n_list0 = calc_n_list(starting_inactive_twig_id, oldest_active_twig_id);
    let n_list = tree.recover_twigs(oldest_active_twig_id, youngest_twig_id, ef_prune_to);
    let new_list = if !n_list0.is_empty() && !n_list.is_empty() && n_list0.last() == n_list.first()
    {
        n_list0.extend_from_slice(&n_list[1..]);
        n_list0
    } else {
        n_list0.extend_from_slice(&n_list);
        n_list0
    };

    //it may be partially build, so it's useless
    if let Some(twig_id) = incomplete_twig {
        let (s, i) = get_shard_idx_and_key(twig_id);
        tree.upper_tree.active_twig_shards[s].remove(&i);
    }

    let youngest_twig = tree.upper_tree.get_twig(tree.youngest_twig_id).unwrap();
    tree.new_twig_map
        .insert(tree.youngest_twig_id, youngest_twig.clone());

    let root_hash = tree.recover_upper_nodes(edge_nodes, new_list);
    (tree, root_hash)
}

fn calc_n_list(starting_inactive_twig_id: u64, oldest_active_twig_id: u64) -> Vec<u64> {
    let mut n_list0 =
        Vec::with_capacity(1 + (oldest_active_twig_id - starting_inactive_twig_id) as usize / 2);
    for twig_id in (starting_inactive_twig_id + 1)..oldest_active_twig_id {
        if n_list0.is_empty() || *n_list0.last().unwrap() != twig_id / 2 {
            n_list0.push(twig_id / 2);
        }
    }
    n_list0
}

#[cfg(test)]
mod tests {
    use super::calc_n_list;

    #[test]
    fn test_calc_n_list() {
        let n_list = calc_n_list(123, 234);
        assert_eq!(
            n_list,
            vec![
                62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82,
                83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100, 101, 102,
                103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116
            ]
        );
    }
}
