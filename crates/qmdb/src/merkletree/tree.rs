//! Implementation of a high-performance Merkle tree optimized for blockchain state management.
//!
//! This module provides the core Merkle tree implementation with the following key features:
//! - Sharded storage for improved concurrency and performance
//! - Incremental updates with twig-based structure
//! - Efficient proof generation and verification
//! - Support for state pruning and recovery
//!
//! The tree is structured in two main parts:
//! 1. Upper tree levels (managed by [`UpperTree`])
//! 2. Twig levels (leaf-level subtrees managed by [`Twig`])
//!
//! ```text
//!              ____TwigRoot___                   Level_12
//!             /               \
//!            /                 \
//! 1       leftRoot              activeBitsMTL3   Level_11
//! 2       Level_10        2     activeBitsMTL2
//! 4       Level_9         4     activeBitsMTL1
//! 8       Level_8    8*32bytes  activeBits
//! 16      Level_7
//! 32      Level_6
//! 64      Level_5
//! 128     Level_4
//! 256     Level_3
//! 512     Level_2
//! 1024    Level_1
//! 2048    Level_0
//! ```

use super::twigfile::{self, TwigFile, TwigFileWriter};
use crate::entryfile::{EntryFile, EntryFileWriter};
use crate::merkletree::recover;
use aes_gcm::Aes256Gcm;
use once_cell::sync::Lazy;
use qmdb_common::def::{
    calc_max_level, nth_at_level_from_twigid, ENTRIES_PATH, FIRST_LEVEL_ABOVE_TWIG,
    LEAF_COUNT_IN_TWIG, MAX_TREE_LEVEL, MIN_PRUNE_COUNT, NODE_SHARD_COUNT, TWIG_MASK, TWIG_PATH,
    TWIG_ROOT_LEVEL, TWIG_SHARD_COUNT, TWIG_SHIFT,
};
use qmdb_common::entryfile::entry::{Entry, EntryBz};
use qmdb_common::entryfile::readbuf::ReadBuf;
use qmdb_common::merkletree::activebits::ActiveBits;
use qmdb_common::merkletree::hash::merkle_node_hash;
use qmdb_common::merkletree::null::get_null_hash_by_pos;
use qmdb_common::merkletree::null::{get_null_mt_for_twig, NULL_NODE_IN_HIGHER_TREE, NULL_TWIG};
use qmdb_common::merkletree::twig::{self, get_null_twig_box, sync_mtree, TwigMT};
use qmdb_common::utils::hasher::Hash32;
use rayon;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::{fmt, fs, mem, thread};

pub static EMPTY_ACTIVE_BITS: Lazy<ActiveBits> = Lazy::new(ActiveBits::default);

/*
             ____TwigRoot___                   Level_12
            /               \
           /                 \
1       leftRoot              activeBitsMTL3   Level_11
2       Level_10        2     activeBitsMTL2
4       Level_9         4     activeBitsMTL1
8       Level_8    8*32bytes  activeBits
16      Level_7
32      Level_6
64      Level_5
128     Level_4
256     Level_3
512     Level_2
1024    Level_1
2048    Level_0
*/

/*         1
     2             3
  4     5       6     7
 8 9   a b     c d   e f
*/

/// Represents a node position in the Merkle tree.
///
/// The position is encoded as a 64-bit integer where:
/// - High 8 bits store the level
/// - Low 56 bits store the node index at that level
#[derive(Copy, Clone, Eq, Hash, PartialEq)]
pub struct NodePos(u64);

impl NodePos {
    /// Creates a new NodePos from a raw 64-bit position value
    pub fn new(pos: u64) -> NodePos {
        NodePos(pos)
    }

    /// Creates a NodePos from a level and node index
    pub fn pos(level: u64, n: u64) -> NodePos {
        NodePos((level << 56) | n)
    }

    /// Returns the level of this node (0-255)
    pub fn level(&self) -> u64 {
        self.0 >> 56 // extract the high 8 bits
    }

    /// Returns the node index at this level
    pub fn nth(&self) -> u64 {
        (self.0 << 8) >> 8 // extract the low 56 bits
    }

    /// Returns the raw 64-bit position value
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl fmt::Debug for NodePos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "NodePos: {:?} {{ level: {}, nth: {} }}",
            self.as_u64(),
            self.level(),
            self.nth()
        )
    }
}

/// Represents a node at the edge of a pruned subtree
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct EdgeNode {
    /// Position of the edge node
    pub pos: NodePos,
    /// 32-byte hash value of the node
    pub value: [u8; 32],
}

pub fn print_edge_nodes(shard_id: usize, edge_nodes: &[EdgeNode]) {
    for node in edge_nodes.iter() {
        println!(
            "E{} {}-{} {}; ",
            shard_id,
            node.pos.level(),
            node.pos.nth(),
            hex::encode(node.value)
        );
    }
}

/// Manages the upper levels of the Merkle tree (above the twig level)
///
/// The UpperTree maintains:
/// 1. A sharded map of nodes for each level above the twig level
/// 2. Active twig shards that haven't been persisted yet
/// 3. Support for concurrent updates through sharding
#[derive(Clone)]
pub struct UpperTree {
    /// Shard ID this tree instance is responsible for
    pub my_shard_id: usize,
    /// Nodes in the upper tree levels, sharded for concurrent access
    /// Structure: [level][shard_id] -> map of positions to hash values
    /// the nodes in high level tree (higher than twigs)
    /// this variable can be recovered from saved edge nodes and activeTwigs
    pub nodes: Vec<Vec<HashMap<NodePos, [u8; 32]>>>, //MaxUpperLevel*NodeShardCount maps
    /// Currently active twig shards that haven't been persisted
    /// Structure: [shard_id] -> map of twig IDs to twig instances
    /// this variable can be recovered from entry file
    pub active_twig_shards: Vec<HashMap<u64, Box<twig::Twig>>>,
}

impl UpperTree {
    pub fn empty() -> Self {
        Self {
            my_shard_id: 0,
            nodes: Vec::with_capacity(0),
            active_twig_shards: Vec::with_capacity(0),
        }
    }

    pub fn print_twig_roots(&self) {
        for s in 0..self.active_twig_shards.len() {
            let m = &self.active_twig_shards[s];
            for (&i, twig) in m.iter() {
                let twig_id = i as usize * TWIG_SHARD_COUNT + s;
                println!("XXX {} {}", twig_id, hex::encode(twig.twig_root));
            }
        }
    }

    pub fn print_nodes(&self) {
        for v in self.nodes.iter() {
            for m in v.iter() {
                for (node, value) in m.iter() {
                    println!(
                        "YYY {:#03}-{:#08} {}",
                        node.level(),
                        node.nth(),
                        hex::encode(value)
                    );
                }
            }
        }
    }

    pub fn new(my_shard_id: usize) -> Self {
        let node_shards = vec![HashMap::<NodePos, [u8; 32]>::new(); NODE_SHARD_COUNT];
        let nodes = vec![node_shards; MAX_TREE_LEVEL];
        let active_twig_shards = vec![HashMap::<u64, Box<twig::Twig>>::new(); TWIG_SHARD_COUNT];

        Self {
            my_shard_id,
            nodes,
            active_twig_shards,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.len() == 0
    }

    pub fn add_twigs(&mut self, twig_map: HashMap<u64, Box<twig::Twig>>) {
        for (twig_id, twig) in twig_map {
            // new_twig_map is designed for concurrency.
            // When UpperTree is discarded, if the flusher needs to append a new twig, it is first placed in new_twig_map.
            // In new_twig_map, only left_root has a meaningful value.
            let (shard_idx, key) = get_shard_idx_and_key(twig_id);
            if let Some(v) = self.active_twig_shards[shard_idx].get_mut(&key) {
                v.left_root.copy_from_slice(twig.left_root.as_slice());
            } else {
                self.active_twig_shards[shard_idx].insert(key, twig);
            }
        }
    }

    pub fn get_twig(&mut self, twig_id: u64) -> Option<&mut Box<twig::Twig>> {
        let (shard_idx, key) = get_shard_idx_and_key(twig_id);
        self.active_twig_shards[shard_idx].get_mut(&key)
    }

    pub fn get_twig_root(&self, n: u64) -> Option<&[u8; 32]> {
        let (shard_idx, key) = get_shard_idx_and_key(n);
        let twig_option = self.active_twig_shards[shard_idx].get(&key);
        match twig_option {
            Some(v) => Some(&v.twig_root),
            None => {
                // the twig has been evicted
                let pos = NodePos::pos(TWIG_ROOT_LEVEL as u64, n);
                self.get_node(pos)
            }
        }
    }

    pub fn set_node_copy(&mut self, pos: NodePos, node: &[u8; 32]) {
        let mut n = [0; 32];
        n.copy_from_slice(node);
        // self.nodes[pos.level() as usize][pos.nth() as usize % NODE_SHARD_COUNT].insert(pos, n);
        self.set_node(pos, n);
    }

    pub fn set_node(&mut self, pos: NodePos, node: [u8; 32]) {
        self.nodes[pos.level() as usize][pos.nth() as usize % NODE_SHARD_COUNT].insert(pos, node);
    }

    pub fn get_node(&self, pos: NodePos) -> Option<&[u8; 32]> {
        self.nodes[pos.level() as usize][pos.nth() as usize % NODE_SHARD_COUNT].get(&pos)
    }

    fn delete_node(&mut self, pos: NodePos) {
        self.nodes[pos.level() as usize][pos.nth() as usize % NODE_SHARD_COUNT].remove(&pos);
    }

    pub fn prune_nodes(&mut self, start: u64, end: u64, youngest_twig_id: u64) -> Vec<u8> {
        let max_level = calc_max_level(youngest_twig_id);
        self.remove_useless_nodes(start, end, max_level);
        recover::edge_nodes_to_bytes(&self.get_edge_nodes(end, max_level))
    }

    fn remove_useless_nodes(&mut self, start: u64, end: u64, max_level: i64) {
        let mut cur_start = start;
        let mut cur_end = end;
        for level in TWIG_ROOT_LEVEL..=max_level {
            let mut end_back = cur_end;
            if cur_end % 2 != 0 && level != TWIG_ROOT_LEVEL {
                end_back -= 1;
            }

            let mut start_back = cur_start;
            start_back = start_back.saturating_sub(1);
            for i in start_back..end_back {
                let pos = NodePos::pos(level as u64, i);
                self.delete_node(pos);
            }
            cur_start >>= 1;
            cur_end >>= 1;
        }
    }

    fn get_edge_nodes(&self, end: u64, max_level: i64) -> Vec<EdgeNode> {
        let mut cur_end = end;
        let mut new_edge_nodes = Vec::new();
        for level in TWIG_ROOT_LEVEL..=max_level {
            let mut end_back = cur_end;
            if cur_end % 2 != 0 && level != TWIG_ROOT_LEVEL {
                end_back -= 1;
            }
            let pos = NodePos::pos(level as u64, end_back);
            if let Some(v) = self.get_node(pos) {
                new_edge_nodes.push(EdgeNode { pos, value: *v });
            } else {
                panic!(
                    "What? can not find shard_id={} max_level={} level={} end={} cur_end={}",
                    self.my_shard_id, max_level, level, end, cur_end
                );
            }
            cur_end >>= 1;
        }
        new_edge_nodes
    }

    pub fn sync_nodes_by_level(
        &mut self,
        level: i64,
        n_list: Vec<u64>,
        youngest_twig_id: u64,
    ) -> Vec<u64> {
        let max_n = nth_at_level_from_twigid(youngest_twig_id, level);
        let pos = NodePos::pos(level as u64, max_n);
        self.set_node_copy(pos, &NULL_NODE_IN_HIGHER_TREE[level as usize]);
        if max_n % 2 == 0 {
            //we also need the right child
            let pos = NodePos::pos(level as u64, max_n + 1);
            self.set_node_copy(pos, &NULL_NODE_IN_HIGHER_TREE[level as usize]);
        }
        // take written_nodes out from self.nodes
        self.nodes.push(Vec::new()); // push a placeholder that will be removed
        let mut written_nodes = self.nodes.swap_remove(level as usize);

        let mut new_list = Vec::with_capacity(n_list.len());
        rayon::scope(|s| {
            // run flushing in a threads such that sync_* won't be blocked
            for (shard_id, nodes) in written_nodes.iter_mut().enumerate() {
                let n_list = &n_list[..];
                let upper_tree = &*self; // change a mutable borrow to an immutable borrow
                let id: usize = shard_id;
                do_sync_job(upper_tree, nodes, level, id, n_list, youngest_twig_id);
            }
            for &i in n_list.iter() {
                if new_list.is_empty() || *new_list.last().unwrap() != i / 2 {
                    new_list.push(i / 2);
                }
            }
        });

        // return written_nodes back to self.nodes
        self.nodes.push(written_nodes);
        self.nodes.swap_remove(level as usize); // the placeholder is removed
        new_list
    }

    pub fn sync_upper_nodes(
        &mut self,
        mut n_list: Vec<u64>,
        youngest_twig_id: u64,
    ) -> (Vec<u64>, [u8; 32]) {
        let max_level = calc_max_level(youngest_twig_id);
        if !n_list.is_empty() {
            for level in FIRST_LEVEL_ABOVE_TWIG..=max_level {
                n_list = self.sync_nodes_by_level(level, n_list, youngest_twig_id);
            }
        }
        let root = *self.get_node(NodePos::pos(max_level as u64, 0)).unwrap();
        (n_list, root)
    }

    pub fn evict_twigs(
        &mut self,
        n_list: Vec<u64>,
        twig_evict_start: u64,
        twig_evict_end: u64,
    ) -> Vec<u64> {
        let new_list = self.sync_mt_for_active_bits_phase2(n_list);
        // run the pending twig-eviction jobs
        // they were not evicted earlier because sync_mt_for_active_bits_phase2 needs their content
        for twig_id in twig_evict_start..twig_evict_end {
            // evict the twig and store its twigRoot in nodes
            let pos = NodePos::pos(TWIG_ROOT_LEVEL as u64, twig_id);
            let twig_root = self.get_twig(twig_id).unwrap().twig_root;
            self.set_node_copy(pos, &twig_root);
            let (shard_idx, key) = get_shard_idx_and_key(twig_id);
            self.active_twig_shards[shard_idx].remove(&key);
        }
        new_list
    }

    pub fn sync_mt_for_active_bits_phase2(&mut self, mut n_list: Vec<u64>) -> Vec<u64> {
        let mut new_list = Vec::with_capacity(n_list.len());
        rayon::scope(|s| {
            for (sid, twig_shard) in self.active_twig_shards.iter_mut().enumerate() {
                for i in n_list.iter() {
                    let twig_id = i >> 1;
                    let (s, k) = get_shard_idx_and_key(twig_id);
                    if s != sid {
                        continue;
                    };
                    twig_shard.get_mut(&k).unwrap().sync_l2((i & 1) as i32);
                }
            }

            for i in &n_list {
                if new_list.is_empty() || *new_list.last().unwrap() != i / 2 {
                    new_list.push(i / 2);
                }
            }
        });

        mem::swap(&mut new_list, &mut n_list);
        new_list.clear();
        rayon::scope(|s| {
            for (sid, twig_shard) in self.active_twig_shards.iter_mut().enumerate() {
                for twig_id in n_list.iter() {
                    let (s, k) = get_shard_idx_and_key(*twig_id);
                    if s != sid {
                        continue;
                    };
                    twig_shard.get_mut(&k).unwrap().sync_l3();
                    twig_shard.get_mut(&k).unwrap().sync_top();
                }
            }

            for i in &n_list {
                if new_list.is_empty() || *new_list.last().unwrap() != i / 2 {
                    new_list.push(i / 2);
                }
            }
        });

        new_list
    }
}

fn do_sync_job(
    upper_tree: &UpperTree,
    nodes: &mut HashMap<NodePos, [u8; 32]>,
    level: i64,
    shard_id: usize,
    n_list: &[u64],
    youngest_twig_id: u64,
) {
    let child_nodes = upper_tree.nodes.get((level - 1) as usize).unwrap();
    for &i in n_list {
        if i as usize % NODE_SHARD_COUNT != shard_id {
            continue;
        }
        let pos = NodePos::pos(level as u64, i);
        if level == FIRST_LEVEL_ABOVE_TWIG {
            let left_option = upper_tree.get_twig_root(2 * i);
            let left = match left_option {
                Some(v) => v,
                None => panic!(
                    "Cannot find left twig root {}@{}",
                    2 * i,
                    upper_tree.my_shard_id
                ),
            };
            let right_option = upper_tree.get_twig_root(2 * i + 1);
            let mut right = [0u8; 32];
            match right_option {
                Some(v) => {
                    right.copy_from_slice(v);
                }
                None => {
                    right.copy_from_slice(&NULL_TWIG.twig_root[..]);
                }
            };
            let hash = merkle_node_hash(level as u8 - 1, left, &right);
            nodes.insert(pos, hash);
        } else {
            let node_pos_l = NodePos::pos((level - 1) as u64, 2 * i);
            let node_pos_r = NodePos::pos((level - 1) as u64, 2 * i + 1);
            let sl = node_pos_l.nth() as usize % NODE_SHARD_COUNT;
            let sr = node_pos_r.nth() as usize % NODE_SHARD_COUNT;
            let node_l = match child_nodes[sl].get(&node_pos_l) {
                Some(v) => v,
                None => {
                    panic!(
                        "Cannot find left child {}-{} {}-{} {} {:?}",
                        level,
                        i,
                        level - 1,
                        2 * i,
                        2 * i + 1,
                        node_pos_l
                    );
                }
            };

            let node_r = match child_nodes[sr].get(&node_pos_r) {
                Some(v) => v,
                None => {
                    panic!(
                        "Cannot find right child {}-{} {}-{} {} {:?} youngest_twig_id={}",
                        level,
                        i,
                        level - 1,
                        2 * i,
                        2 * i + 1,
                        node_pos_r,
                        youngest_twig_id,
                    )
                }
            };

            let hash = merkle_node_hash(level as u8 - 1, node_l, node_r);
            nodes.insert(pos, hash);
        }
    }
}

/// The main Merkle tree implementation combining upper levels and twigs
///
/// This struct provides the complete interface for:
/// - Appending and updating entries
/// - Managing active bits for entry status
/// - Generating and verifying proofs
/// - Persisting state to disk
/// - Pruning old state
pub struct Tree {
    /// Shard ID this tree instance is responsible for
    pub my_shard_id: usize,

    /// Upper levels of the tree (above twig level)
    pub upper_tree: UpperTree,
    /// Temporary storage for new twigs before they're added to upper_tree
    pub new_twig_map: HashMap<u64, Box<twig::Twig>>,

    /// Writer for persisting entries to disk
    pub entry_file_wr: EntryFileWriter,
    /// Writer for persisting twig data to disk
    pub twig_file_wr: TwigFileWriter,
    /// Base directory for persistence files
    pub dir_name: String,

    // these variables can be recovered from entry file
    /// ID of the most recently created twig    
    pub youngest_twig_id: u64,
    pub youngest_twig_is_empty: bool,
    // pub active_bit_shards: [HashMap<u64, [u8; 256]>; TWIG_SHARD_COUNT],
    /// Active bits for each twig shard indicating entry status    
    pub active_bit_shards: Vec<HashMap<u64, ActiveBits>>,
    /// Merkle tree for the most recent twig being built
    pub mtree_for_youngest_twig: Box<TwigMT>,

    // The following variables are only used during the execution of one block
    /// Start position of changes in youngest twig (used during block execution)    
    pub mtree_for_yt_change_start: i32,
    /// End position of changes in youngest twig (used during block execution)
    pub mtree_for_yt_change_end: i32,
    /// Set of 512-byte positions that have been modified
    touched_pos_of_512b: HashSet<u64>,
}

impl Tree {
    /// Creates a new empty Merkle tree with the specified configuration
    ///
    /// # Arguments
    /// * `shard_id` - ID of the shard this tree instance will manage
    /// * `buffer_size` - Size of the buffer for file I/O operations
    /// * `segment_size` - Size of segments for file storage
    /// * `dir_name` - Base directory for persistence files
    /// * `suffix` - Suffix to append to file names
    /// * `with_twig_file` - Whether to create and maintain twig files
    /// * `cipher` - Optional encryption cipher for file I/O
    pub fn new_blank(
        shard_id: usize,
        buffer_size: usize,
        segment_size: i64,
        dir_name: String,
        suffix: String,
        with_twig_file: bool,
        cipher: Option<Aes256Gcm>,
    ) -> Self {
        let dir_entry = format!("{dir_name}/{ENTRIES_PATH}{suffix}");
        let _ = fs::create_dir_all(&dir_entry);
        let twig_file = if with_twig_file {
            let dir_twig = format!("{dir_name}/{TWIG_PATH}{suffix}");
            let _ = fs::create_dir_all(&dir_twig);
            TwigFile::new(buffer_size, segment_size, dir_twig)
        } else {
            TwigFile::empty()
        };
        let twig_arc = Arc::new(twig_file);
        let ef = EntryFile::new(buffer_size, segment_size, dir_entry, cipher);

        Self {
            my_shard_id: shard_id,
            upper_tree: UpperTree::new(shard_id),
            new_twig_map: HashMap::new(),
            entry_file_wr: EntryFileWriter::new(Arc::new(ef), buffer_size),
            twig_file_wr: TwigFileWriter::new(twig_arc, buffer_size),
            dir_name,
            youngest_twig_id: 0,
            youngest_twig_is_empty: true,
            active_bit_shards: vec![HashMap::new(); TWIG_SHARD_COUNT],
            mtree_for_youngest_twig: get_null_mt_for_twig().to_vec(),
            mtree_for_yt_change_start: -1,
            mtree_for_yt_change_end: -1,
            touched_pos_of_512b: HashSet::new(),
        }
    }

    /// Creates a new Merkle tree and loads existing state if available
    ///
    /// Similar to new_blank(), but attempts to load existing state from disk.
    pub fn new(
        shard_id: usize,
        buffer_size: usize,
        segment_size: i64,
        dir_name: String,
        suffix: String,
        with_twig_file: bool,
        cipher: Option<Aes256Gcm>,
    ) -> Self {
        let mut tree = Self::new_blank(
            shard_id,
            buffer_size,
            segment_size,
            dir_name,
            suffix,
            with_twig_file,
            cipher,
        );

        tree.new_twig_map.insert(0, get_null_twig_box().clone());
        tree.upper_tree
            .set_node(NodePos::pos(FIRST_LEVEL_ABOVE_TWIG as u64, 0), [0; 32]);
        tree.upper_tree.active_twig_shards[0].insert(0, get_null_twig_box().clone());
        tree.active_bit_shards[0].insert(0, ActiveBits::default());

        tree
    }

    /// Closes all file handles and flushes pending writes
    pub fn close(&mut self) {
        // Close files
        self.entry_file_wr.entry_file.close();
        self.twig_file_wr.twig_file.close();
    }

    /// Returns the current sizes of the entry and twig files
    ///
    /// # Returns
    /// A tuple of (entry_file_size, twig_file_size) in bytes
    pub fn get_file_sizes(&self) -> (i64, i64) {
        (
            self.entry_file_wr.entry_file.size(),
            self.twig_file_wr.twig_file.hp_file.size(),
        )
    }

    /// Truncates the entry and twig files to the specified sizes
    ///
    /// # Arguments
    /// * `entry_file_size` - New size for the entry file
    /// * `twig_file_size` - New size for the twig file
    pub fn truncate_files(&self, entry_file_size: i64, twig_file_size: i64) {
        self.entry_file_wr
            .entry_file
            .truncate(entry_file_size)
            .unwrap();
        self.twig_file_wr.twig_file.truncate(twig_file_size);
    }

    /// Gets the active bits for a specific twig
    ///
    /// # Arguments
    /// * `twig_id` - ID of the twig to get active bits for
    ///
    /// # Returns
    /// Reference to the active bits for the specified twig
    pub fn get_active_bits(&self, twig_id: u64) -> &ActiveBits {
        let (shard_idx, key) = get_shard_idx_and_key(twig_id);
        match self.active_bit_shards[shard_idx].get(&key) {
            Some(v) => v,
            None => panic!("cannot find twig {}@{}", twig_id, self.my_shard_id),
        }
    }

    /// Gets a mutable reference to the active bits for a specific twig
    ///
    /// # Arguments
    /// * `twig_id` - ID of the twig to get active bits for
    ///
    /// # Returns
    /// Mutable reference to the active bits for the specified twig
    fn get_active_bits_mut(&mut self, twig_id: u64) -> &mut ActiveBits {
        let (shard_idx, key) = get_shard_idx_and_key(twig_id);
        self.active_bit_shards[shard_idx].get_mut(&key).unwrap()
    }

    /// Checks if a specific entry is active
    ///
    /// # Arguments
    /// * `sn` - Serial number of the entry to check
    ///
    /// # Returns
    /// true if the entry is active, false otherwise
    pub fn get_active_bit(&self, sn: u64) -> bool {
        let twig_id = sn >> TWIG_SHIFT;
        let pos = sn as u32 & TWIG_MASK;
        self.get_active_bits(twig_id).get_bit(pos)
    }

    /// Sets the activation status of an entry
    ///
    /// # Arguments
    /// * `sn` - Serial number of the entry to update
    /// * `active` - New activation status
    pub fn set_entry_activiation(&mut self, sn: u64, active: bool) {
        let twig_id = sn >> TWIG_SHIFT;
        let pos = sn as u32 & TWIG_MASK;
        let active_bits = self.get_active_bits_mut(twig_id);
        if active {
            active_bits.set_bit(pos);
        } else {
            active_bits.clear_bit(pos);
        }
        self.touch_pos(sn);
    }

    /// Records that a position has been touched during the current block execution
    ///
    /// # Arguments
    /// * `sn` - Serial number of the touched position
    pub fn touch_pos(&mut self, sn: u64) {
        self.touched_pos_of_512b.insert(sn / 512);
    }

    /// Clears the set of touched positions
    pub fn clear_touched_pos(&mut self) {
        self.touched_pos_of_512b.clear();
    }

    /// Marks an entry as active
    ///
    /// # Arguments
    /// * `sn` - Serial number of the entry to activate
    pub fn active_entry(&mut self, sn: u64) {
        self.set_entry_activiation(sn, true);
    }

    /// Marks an entry as inactive
    ///
    /// # Arguments
    /// * `sn` - Serial number of the entry to deactivate
    pub fn deactive_entry(&mut self, sn: u64) {
        self.set_entry_activiation(sn, false);
    }

    /// Appends a new entry to the tree
    ///
    /// # Arguments
    /// * `entry_bz` - The entry to append
    ///
    /// # Returns
    /// The offset of the appended entry in the entry file, or an error if the operation failed
    pub fn append_entry(&mut self, entry_bz: &EntryBz) -> Result<i64, std::io::Error> {
        let sn = entry_bz.serial_number();
        self.active_entry(sn);

        let twig_id = sn >> TWIG_SHIFT;
        self.youngest_twig_id = twig_id;
        self.youngest_twig_is_empty = false;
        // record change_start/change_end for endblock sync
        let position = sn as u32 & TWIG_MASK;
        if self.mtree_for_yt_change_start == -1 {
            self.mtree_for_yt_change_start = position as i32;
        } else if self.mtree_for_yt_change_end + 1 != position as i32 {
            panic!("non-increasing position!");
        }
        self.mtree_for_yt_change_end = position as i32;

        let pos = self.entry_file_wr.append(entry_bz)?;
        self.mtree_for_youngest_twig[(LEAF_COUNT_IN_TWIG + position) as usize]
            .copy_from_slice(entry_bz.hash().as_slice());

        if position == TWIG_MASK {
            // when this is the last entry of current twig
            // write the merkle tree of youngest twig to twig_file
            self.sync_mt_for_youngest_twig(false);
            //println!("GO shard={} twig_file_wr.append_twig", self.my_shard_id);
            self.twig_file_wr.append_twig(
                &self.mtree_for_youngest_twig[..],
                pos + entry_bz.len() as i64,
            );
            // allocate new twig as youngest twig
            self.youngest_twig_id += 1;
            self.youngest_twig_is_empty = true;
            let (s, i) = get_shard_idx_and_key(self.youngest_twig_id);
            self.new_twig_map
                .insert(self.youngest_twig_id, get_null_twig_box().clone());
            self.active_bit_shards[s].insert(i, ActiveBits::default());

            self.mtree_for_youngest_twig = get_null_mt_for_twig().to_vec();
        }
        Ok(pos)
    }

    // [Added] Helper to get segment size for pruning calculations
    pub fn get_entry_file_segment_size(&self) -> i64 {
        self.entry_file_wr.entry_file.segment_size()
    }

    /// Prunes old twigs from the tree to reclaim space
    ///
    /// # Arguments
    /// * `start_id` - ID of the first twig to prune
    /// * `end_id` - ID of the last twig to prune
    /// * `entry_file_size` - Current size of the entry file
    pub fn prune_twigs(&mut self, start_id: u64, end_id: u64, entry_file_size: i64) {
        if end_id - start_id < MIN_PRUNE_COUNT {
            panic!(
                "The count of pruned twigs is too small: {}",
                end_id - start_id
            );
        }

        self.entry_file_wr
            .entry_file
            .prune_head(entry_file_size)
            .unwrap();
        self.twig_file_wr
            .twig_file
            .prune_head((end_id * twigfile::TWIG_SIZE) as i64);
    }

    /// Flushes pending changes to disk and optionally deletes old twigs
    ///
    /// # Arguments
    /// * `twig_delete_start` - ID of the first twig to delete
    /// * `twig_delete_end` - ID of the last twig to delete
    ///
    /// # Returns
    /// A list of twig IDs that were affected by the flush operation
    pub fn flush_files(&mut self, twig_delete_start: u64, twig_delete_end: u64) -> Vec<u64> {
        let mut entry_file_tmp = self.entry_file_wr.temp_clone();
        let mut twig_file_tmp = self.twig_file_wr.temp_clone();
        mem::swap(&mut entry_file_tmp, &mut self.entry_file_wr);
        mem::swap(&mut twig_file_tmp, &mut self.twig_file_wr);
        let n_list = thread::scope(|s| {
            // run flushing in a threads such that sync_* won't be blocked
            s.spawn(|| {
                entry_file_tmp.flush().unwrap();
            });
            s.spawn(|| {
                twig_file_tmp.flush();
            });
            self.sync_mt_for_youngest_twig(false);
            let youngest_twig = self.new_twig_map.get(&self.youngest_twig_id).unwrap();
            let mut twig_map = HashMap::new();
            twig_map.insert(self.youngest_twig_id, youngest_twig.clone());
            mem::swap(&mut self.new_twig_map, &mut twig_map);
            //add new_twig_map's old content to upper_tree
            self.upper_tree.add_twigs(twig_map);
            //now, new_twig_map only contains one member: youngest_twig.clone()

            let n_list = self.sync_mt_for_active_bits_phase1(0);
            for twig_id in twig_delete_start..twig_delete_end {
                let (shard_idx, key) = get_shard_idx_and_key(twig_id);
                self.active_bit_shards[shard_idx].remove(&key);
            }
            self.touched_pos_of_512b.clear();
            n_list
        });
        mem::swap(&mut entry_file_tmp, &mut self.entry_file_wr);
        mem::swap(&mut twig_file_tmp, &mut self.twig_file_wr);
        //println!("GO shard={} flush_files twig_file_size={}", self.my_shard_id, self.twig_file_wr.twig_file.hp_file.size());
        n_list
    }

    /// First phase of syncing the Merkle tree for active bits
    ///
    /// # Returns
    /// A list of node IDs that need to be processed in phase 2
    pub fn sync_mt_for_active_bits_phase1(&mut self, start_twig_id: u64) -> Vec<u64> {
        let mut n_list = Vec::with_capacity(self.touched_pos_of_512b.len());
        for &pos in self.touched_pos_of_512b.iter() {
            let out_of_range = (pos * 512) < (start_twig_id << TWIG_SHIFT);
            if out_of_range {
                continue;
            }
            n_list.push(pos);
        }
        n_list.sort();

        let mut new_list = Vec::with_capacity(n_list.len());
        rayon::scope(|s| {
            for (sid, twig_shard) in self.upper_tree.active_twig_shards.iter_mut().enumerate() {
                for i in n_list.iter() {
                    let twig_id = i >> 2;
                    let (s, k) = get_shard_idx_and_key(twig_id);
                    if s != sid {
                        continue;
                    }
                    let active_bits = self.active_bit_shards[s].get(&k).unwrap();
                    twig_shard
                        .get_mut(&k)
                        .unwrap()
                        .sync_l1((i & 3) as i32, active_bits);
                }
            }

            for i in &n_list {
                if new_list.is_empty() || *new_list.last().unwrap() != i / 2 {
                    new_list.push(i / 2);
                }
            }
            new_list
        })
    }

    /// Synchronizes the Merkle tree for the youngest twig
    ///
    /// # Arguments
    /// * `recover_mode` - Whether to run in recovery mode
    pub fn sync_mt_for_youngest_twig(&mut self, recover_mode: bool) {
        if self.mtree_for_yt_change_start == -1 {
            return;
        }
        sync_mtree(
            &mut self.mtree_for_youngest_twig,
            self.mtree_for_yt_change_start,
            self.mtree_for_yt_change_end,
        );
        self.mtree_for_yt_change_start = -1;
        self.mtree_for_yt_change_end = 0;
        let youngest_twig = if recover_mode {
            let res = self.upper_tree.get_twig(self.youngest_twig_id);
            if res.is_none() {
                panic!("In sync_mt_for_youngest_twig {}", self.youngest_twig_id);
            }
            res.unwrap()
        } else {
            self.new_twig_map.get_mut(&self.youngest_twig_id).unwrap()
        };
        youngest_twig
            .left_root
            .copy_from_slice(&self.mtree_for_youngest_twig[1]);
        if recover_mode {
            youngest_twig.twig_root = merkle_node_hash(
                11,
                &youngest_twig.left_root,
                &youngest_twig.active_bits_mtl3,
            );
        }
    }

    /// Loads the Merkle tree for a non-youngest twig
    ///
    /// # Arguments
    /// * `twig_id` - ID of the twig to load
    pub fn load_mt_for_non_youngest_twig(&mut self, twig_id: u64) {
        if self.mtree_for_yt_change_start == -1 {
            return;
        }
        self.mtree_for_yt_change_start = -1;
        self.mtree_for_yt_change_end = 0;
        let active_twig = self.upper_tree.get_twig(self.youngest_twig_id).unwrap();
        self.twig_file_wr
            .twig_file
            .get_hash_root(twig_id, &mut active_twig.left_root);
    }

    // get_multi_proof is implemented as an inherent method in multiproof.rs

    /// Gets hash values for a list of positions in the tree
    ///
    /// # Arguments
    /// * `pos_list` - List of (level, position) pairs to get hashes for
    ///
    /// # Returns
    /// List of hash values corresponding to the requested positions
    pub fn get_hashes_by_pos_list(&self, pos_list: &Vec<(u8, u64)>) -> Vec<[u8; 32]> {
        let mut hashes = Vec::with_capacity(pos_list.len());
        for (_, hash) in self.hash_iter(pos_list) {
            hashes.push(hash);
        }
        hashes
    }

    pub fn get_max_level(&self) -> u8 {
        let mut youngest_twig_id = self.youngest_twig_id;
        if self.youngest_twig_is_empty {
            youngest_twig_id -= 1;
        }
        calc_max_level(youngest_twig_id) as u8
    }

    pub fn get_root_hash(&self) -> [u8; 32] {
        self.get_hashes_by_pos_list(&vec![(self.get_max_level(), 0)])
            .pop()
            .unwrap()
    }

    /// Creates an iterator over hash values for a list of positions
    ///
    /// # Arguments
    /// * `pos_list` - List of (level, position) pairs to iterate over
    ///
    /// # Returns
    /// An iterator that yields (index, hash) pairs
    pub fn hash_iter<'a>(&'a self, pos_list: &'a Vec<(u8, u64)>) -> HashIterForPosList<'a> {
        HashIterForPosList {
            cache: HashMap::with_capacity(20),
            tree: self,
            pos_list,
            idx: 0,
        }
    }

    /// Gets the hash value for a specific node
    ///
    /// # Arguments
    /// * `level` - Level of the node
    /// * `nth` - Position of the node at that level
    /// * `cache` - Cache of previously computed hash values
    ///
    /// # Returns
    /// The 32-byte hash value of the node
    pub(crate) fn get_hash_by_node(
        &self,
        level: u8,
        nth: u64,
        cache: &mut HashMap<i64, [u8; 32]>,
    ) -> [u8; 32] {
        let mut twig_id: u64 = 0;
        let mut level_stride: u64 = 0;
        if level <= 12 {
            level_stride = 4096 >> level;
            twig_id = nth / level_stride;

            // node is padding
            // return null hash
            if twig_id > self.youngest_twig_id {
                //println!("XX6 {} {} null", level, nth);
                return *get_null_hash_by_pos(level, nth);
            }
        }

        // left tree of twig
        if level <= 11 && (nth % level_stride) < level_stride / 2 {
            let is_youngest_twig_id = twig_id == self.youngest_twig_id;
            let self_id: u64 = nth % level_stride;
            let idx = level_stride / 2 + self_id;
            if is_youngest_twig_id {
                //println!("XX0");
                return self.mtree_for_youngest_twig[idx as usize];
            } else {
                let mut hash = [0u8; 32];
                self.twig_file_wr
                    .twig_file
                    .get_hash_node(twig_id, idx as i64, cache, &mut hash);
                //println!("XX1 twig_id={} yti={} idx={} nth={} level_stride={} self_id={}", twig_id, self.youngest_twig_id, idx, nth, level_stride, self_id);
                return hash;
            }
        }

        // right tree of twig
        if (8..=11).contains(&level) {
            let (s, k) = get_shard_idx_and_key(twig_id);
            let active_bits = self.active_bit_shards[s]
                .get(&k)
                .unwrap_or(&EMPTY_ACTIVE_BITS);
            let self_id: u64 = (nth % level_stride) - level_stride / 2;
            if level == 8 {
                let hash = active_bits.get_bits(self_id as usize, 32);
                //println!("XX3");
                return hash.try_into().unwrap();
            }
            let twig = self.upper_tree.active_twig_shards[s]
                .get(&k)
                .map(|t| &**t)
                .unwrap_or(&NULL_TWIG);
            //if level==9 && nth < 20 {
            //    println!("HH NULL shard={} nth={} level=9 {} {} {}", self.my_shard_id, nth, self.upper_tree.active_twig_shards[s].contains_key(&k),
            //    hex::encode(twig.active_bits_mtl1[self_id as usize]),
            //    hex::encode(NULL_TWIG.active_bits_mtl1[self_id as usize]),
            //        );
            //}
            //println!("XX4 {}", level);
            match level {
                9 => return twig.active_bits_mtl1[self_id as usize],
                10 => return twig.active_bits_mtl2[self_id as usize],
                11 => return twig.active_bits_mtl3,
                _ => {}
            }
        }

        // upper tree
        if level == 12 {
            //println!("XX12");
            return *self
                .upper_tree
                .get_twig_root(twig_id)
                .unwrap_or(&NULL_TWIG.twig_root);
        }
        *self
            .upper_tree
            .get_node(NodePos::pos(level as u64, nth))
            .unwrap_or(&NULL_NODE_IN_HIGHER_TREE[level as usize])
    }
}

/// Iterator for retrieving hash values for a list of positions in the tree
pub struct HashIterForPosList<'a> {
    /// Cache of computed hash values to avoid redundant calculations
    cache: HashMap<i64, [u8; 32]>,
    /// Reference to the tree being iterated over
    tree: &'a Tree,
    /// List of positions to get hashes for
    pos_list: &'a Vec<(u8, u64)>,
    /// Current position in the list
    idx: usize,
}

impl Iterator for HashIterForPosList<'_> {
    type Item = (usize, [u8; 32]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx >= self.pos_list.len() {
            return None;
        }
        let (level, nth) = self.pos_list[self.idx];
        let hash = self.tree.get_hash_by_node(level, nth, &mut self.cache);
        let idx = self.idx;
        self.idx += 1;
        Some((idx, hash))
    }
}

/// Calculates the shard index and key for a given twig ID
///
/// This function is used for sharding twig data across multiple storage locations
/// to improve concurrent access performance.
///
/// # Arguments
/// * `twig_id` - ID of the twig to calculate shard information for
///
/// # Returns
/// A tuple of (shard_index, key_within_shard)
pub fn get_shard_idx_and_key(twig_id: u64) -> (usize, u64) {
    let idx = twig_id as usize % TWIG_SHARD_COUNT;
    let key = twig_id / TWIG_SHARD_COUNT as u64;
    (idx, key)
}

// debug

impl Tree {
    /// Prints a debug representation of the tree structure
    ///
    /// This method is primarily used for debugging and development purposes
    /// to visualize the current state of the tree.
    pub fn print(&self) {
        let mut offset: i64 = 0;
        let mut buf = ReadBuf::new();
        for twig_id in 0..self.youngest_twig_id {
            for _sn in twig_id * 2048..(twig_id + 1) * 2048 {
                buf.clear();
                self.entry_file_wr.entry_file.read_entry(offset, &mut buf);
                offset += buf.len() as i64;

                let entry_bz = buf.as_entry_bz();
                let entry = Entry::from_bz(&entry_bz);

                println!(
                    "[entry] twig: {}, sn: {}, k: {}, v: {}",
                    twig_id,
                    entry.serial_number,
                    hex::encode(entry.key),
                    hex::encode(entry.value)
                );
            }
        }

        let mut cache: HashMap<i64, Hash32> = HashMap::new();
        for twig_id in 0..self.youngest_twig_id {
            for _sn in twig_id * 2048..(twig_id + 1) * 2048 {
                let _h = self.get_hash_by_node(0, _sn, &mut cache);
                println!(
                    "[hash] level: {}, nth: {}, hash: {}",
                    0,
                    _sn,
                    hex::encode(_h)
                );
            }
        }
    }
}

