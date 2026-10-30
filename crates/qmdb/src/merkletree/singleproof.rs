//! Compact single-entry Merkle proof that ignores the Right Path (activeness).
//!
//! This proof format yields a classic compact Merkle path consisting only of
//! sibling hashes from the leaf up to the global root. It is intended for
//! historical-existence proofs where activeness is not required.
//!
//! Construction details:
//! - Left-of-twig path: 11 sibling hashes for the entry inside its twig
//! - Join at level 11: one sibling (the right subtree's top hash = active_bits_mtl3)
//! - Upper path: siblings from `TWIG_ROOT_LEVEL` up to the global root
//!
//! With a global leaf index defined as `index = twig_id * 4096 + pos_in_twig`,
//! this sequence can be verified by a classic verifier that walks upward by
//! dividing `index` by 2 at each step and concatenating with the provided
//! sibling according to `index % 2`.

use super::tree::{get_shard_idx_and_key, NodePos, Tree, EMPTY_ACTIVE_BITS};
use super::twigfile;
use borsh::{BorshDeserialize, BorshSerialize};
use qmdb_common::def::{FIRST_LEVEL_ABOVE_TWIG, TWIG_MASK, TWIG_ROOT_LEVEL, TWIG_SHIFT};
use qmdb_common::merkletree::activebits::ActiveBits;
use qmdb_common::merkletree::hash::merkle_node_hash;
use qmdb_common::merkletree::null::NULL_TWIG;
use qmdb_common::merkletree::twig::{self, TwigMT};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SingleProof {
    /// The serial number being proven.
    pub serial_num: u64,
    /// Global leaf index for classic verification: `twig_id * 4096 + pos`.
    pub index: u64,
    /// Leaf hash (entry hash) at the start of the path.
    pub leaf_hash: [u8; 32],
    /// Sibling hashes from leaf up to the global root, in order.
    pub siblings: Vec<[u8; 32]>,
    /// Expected global root hash.
    pub root: [u8; 32],
}

impl SingleProof {
    pub fn len(&self) -> usize {
        self.siblings.len()
    }
    pub fn is_empty(&self) -> bool {
        self.siblings.is_empty()
    }
}

fn hash2x<T: AsRef<[u8]>>(children_level: u8, a: T, b: T, exchange_ab: bool) -> [u8; 32] {
    if exchange_ab {
        merkle_node_hash(children_level, b, a)
    } else {
        merkle_node_hash(children_level, a, b)
    }
}

/// Computes the level-11 right-subtree top hash (active_bits_mtl3) from a right path.
fn compute_active_bits_top(right: &[ProofNode; 3]) -> [u8; 32] {
    // L8 -> L9
    let l9 = hash2x(
        8,
        right[0].self_hash,
        right[0].peer_hash,
        right[0].peer_at_left,
    );
    // L9 -> L10
    let l10 = hash2x(
        9,
        right[1].self_hash,
        right[1].peer_hash,
        right[1].peer_at_left,
    );
    // Note: right[2] represents L10 nodes; combine to L11 top
    let l11 = hash2x(
        10,
        right[2].self_hash,
        right[2].peer_hash,
        right[2].peer_at_left,
    );
    l11
}

/// Verifies a `SingleProof` using QMDb's level-aware node hashing.
///
/// The verifier follows the classic sibling concatenation rule using the
/// provided `index`, hashing upward and comparing to `root`.
pub fn verify_single_proof(sp: &SingleProof) -> bool {
    let mut acc = sp.leaf_hash;
    let mut index = sp.index;

    for (step, sib) in sp.siblings.iter().enumerate() {
        let children_level: u8 = if step <= 10 {
            step as u8
        } else if step == 11 {
            11u8
        } else {
            (TWIG_ROOT_LEVEL as u8) + (step as u8 - 12)
        };

        let (left, right) = if index % 2 == 0 {
            (acc, *sib)
        } else {
            (*sib, acc)
        };
        acc = merkle_node_hash(children_level, &left, &right);
        index /= 2;
    }
    acc == sp.root
}

/// Represents a node in a Merkle proof path
///
/// Each proof node contains:
/// - The hash of the current node
/// - The hash of its sibling (peer) node
/// - Whether the peer is on the left side
#[derive(Clone, Copy, Debug)]
struct ProofNode {
    /// Hash of the current node
    pub self_hash: [u8; 32],
    /// Hash of the sibling (peer) node
    pub peer_hash: [u8; 32],
    /// Whether the peer node is on the left side
    pub peer_at_left: bool,
}

/// A complete Merkle proof path from a leaf to the root
///
/// The path consists of three parts:
/// 1. Left path: Proof nodes from the entry to the twig root's left child
/// 2. Right path: Proof nodes from active bits to the twig root's right child
/// 3. Upper path: Proof nodes from the twig root to the tree root
#[derive(Debug)]
struct ProofPath {
    /// Proof nodes for the left (entry) side of the twig
    pub left_of_twig: [ProofNode; 11],
    /// Proof nodes for the right (active bits) side of the twig
    pub right_of_twig: [ProofNode; 3],
    /// Proof nodes from twig root to tree root
    pub upper_path: Vec<ProofNode>,
    /// Serial number of the entry being proven
    pub serial_num: u64,
    /// Root hash of the entire tree
    pub root: [u8; 32],
}

impl Default for ProofPath {
    fn default() -> Self {
        Self::new()
    }
}

impl ProofPath {
    /// Creates a new empty proof path
    pub fn new() -> Self {
        let empty_node = ProofNode {
            self_hash: [0; 32],
            peer_hash: [0; 32],
            peer_at_left: false,
        };
        ProofPath {
            left_of_twig: [empty_node; 11],
            right_of_twig: [empty_node; 3],
            upper_path: Vec::new(),
            serial_num: 0,
            root: [0; 32],
        }
    }
}

impl Tree {
    /// Builds a compact historical-existence proof for `sn`.
    ///
    /// - Ignores Right Path activeness checks
    /// - Produces a classic sibling-hash path suitable for verification by
    ///   a standard compact Merkle-path verifier using the returned `index`.
    pub fn get_single_proof(&self, sn: u64) -> Result<SingleProof, String> {
        // Reuse the existing full proof to source all needed hashes reliably.
        let full: ProofPath = self.get_proof(sn)?;

        // Derive leaf position and twig id.
        let pos_in_twig: u64 = (sn & TWIG_MASK as u64) as u64;
        let twig_id: u64 = sn >> TWIG_SHIFT;

        // Global index from level 0 to the global root:
        // - 4096 leaves per twig at level 0 (2048 left leaves + right subtree space)
        // - After 11 steps, index collapses to (twig_id * 2 + child)
        // - One more step to level 12 gives index == twig_id for upper tree
        let index: u64 = twig_id * 4096 + pos_in_twig;

        // 1) Left-of-twig: 11 siblings (levels 0..10)
        let mut siblings: Vec<[u8; 32]> = Vec::with_capacity(11 + 1 + full.upper_path.len());
        let leaf_hash = full.left_of_twig[0].self_hash;
        for i in 0..full.left_of_twig.len() {
            // 11 levels
            siblings.push(full.left_of_twig[i].peer_hash);
        }

        // 2) Join at level 11: need the right subtree's top hash (active_bits_mtl3)
        //    Compute it from the provided right_of_twig segment.
        let right_top = compute_active_bits_top(&full.right_of_twig);
        siblings.push(right_top);

        // 3) Upper path: peers from twig root up to global root
        for up in &full.upper_path {
            siblings.push(up.peer_hash);
        }

        Ok(SingleProof {
            serial_num: sn,
            index,
            leaf_hash,
            siblings,
            root: full.root,
        })
    }
    /// Gets the path and root hash for a specific twig in the upper tree
    ///
    /// # Arguments
    /// * `twig_id` - ID of the twig to get path for
    ///
    /// # Returns
    /// A tuple of (proof path, root hash)
    fn get_upper_path_and_root(&self, twig_id: u64) -> (Vec<ProofNode>, [u8; 32]) {
        let max_level_i64 = i64::from(self.get_max_level());

        let mut peer_hash = [0u8; 32];
        // use '^ 1' to flip the lowest bit to get sibling
        if let Some(v) = self.upper_tree.get_twig_root(twig_id ^ 1) {
            peer_hash.copy_from_slice(v);
        } else {
            peer_hash.copy_from_slice(&NULL_TWIG.twig_root[..]);
        }

        let mut self_hash = [0u8; 32];
        if let Some(v) = self.upper_tree.get_twig_root(twig_id) {
            self_hash.copy_from_slice(v);
        } else {
            return (Vec::new(), [0; 32]);
        }

        let mut upper_path =
            Vec::with_capacity((max_level_i64 - FIRST_LEVEL_ABOVE_TWIG + 1) as usize);
        upper_path.push(ProofNode {
            self_hash,
            peer_hash,
            peer_at_left: (twig_id & 1) != 0, //twig_id's lowest bit == 1 so the peer is at left
        });

        let mut n = twig_id >> 1;
        for level in FIRST_LEVEL_ABOVE_TWIG..max_level_i64 {
            let peer_at_left = (n & 1) != 0;

            let snode = match self.upper_tree.get_node(NodePos::pos(level as u64, n)) {
                Some(v) => *v,
                None => panic!("Cannot find node"),
            };
            let pnode = match self.upper_tree.get_node(NodePos::pos(level as u64, n ^ 1)) {
                Some(v) => *v,
                None => panic!("Cannot find node"),
            };
            upper_path.push(ProofNode {
                self_hash: snode,
                peer_hash: pnode,
                peer_at_left,
            });
            n >>= 1;
        }

        let root_option = self
            .upper_tree
            .get_node(NodePos::pos(max_level_i64 as u64, 0));
        let root = match root_option {
            Some(v) => *v,
            None => panic!("cannot find node {}-{}", max_level_i64, 0),
        };

        (upper_path, root)
    }

    /// Generates a Merkle proof for a specific entry
    ///
    /// # Arguments
    /// * `sn` - Serial number of the entry to generate proof for
    ///
    /// # Returns
    /// The proof path, or an error if the proof couldn't be generated
    fn get_proof(&self, sn: u64) -> Result<ProofPath, String> {
        let twig_id = sn >> TWIG_SHIFT;
        let mut path = ProofPath::new();
        path.serial_num = sn;

        if twig_id > self.youngest_twig_id {
            return Err("twig_id > self.youngest_twig_id".to_string());
        }

        (path.upper_path, path.root) = self.get_upper_path_and_root(twig_id);
        if path.upper_path.is_empty() {
            return Err("Cannot find upper path".to_string());
        }

        if twig_id == self.youngest_twig_id {
            path.left_of_twig = get_left_path_in_mem(&self.mtree_for_youngest_twig, sn);
        } else {
            let twig_file = &self.twig_file_wr.twig_file;
            if twig_file.is_empty() {
                return Err("twig_file is empty".to_string());
            }
            path.left_of_twig = get_left_path_on_disk(twig_file, twig_id, sn);
        }
        let (s, k) = get_shard_idx_and_key(twig_id);
        let active_bits = self.active_bit_shards[s]
            .get(&k)
            .unwrap_or(&EMPTY_ACTIVE_BITS);
        let twig = self.upper_tree.active_twig_shards[s]
            .get(&k)
            .map(|t| &**t)
            .unwrap_or(&NULL_TWIG);
        path.right_of_twig = get_right_path(twig, active_bits, sn);

        Ok(path)
    }
}

/// Generates the right (active bits) side of a proof path
///
/// # Arguments
/// * `twig` - The twig containing the entry
/// * `active_bits` - Active bits status for the twig
/// * `sn` - Serial number of the entry
///
/// # Returns
/// Array of proof nodes for the right path
pub fn get_right_path(twig: &twig::Twig, active_bits: &ActiveBits, sn: u64) -> [ProofNode; 3] {
    let n = sn & TWIG_MASK as u64;
    let mut right = [ProofNode {
        self_hash: [0; 32],
        peer_hash: [0; 32],
        peer_at_left: false,
    }; 3];
    let self_id = n / 256;
    let peer = self_id ^ 1;
    right[0]
        .self_hash
        .copy_from_slice(active_bits.get_bits(self_id as usize, 32));
    right[0]
        .peer_hash
        .copy_from_slice(active_bits.get_bits(peer as usize, 32));
    right[0].peer_at_left = (peer & 1) == 0;

    let self_ = n / 512;
    let peer = self_ ^ 1;
    right[1]
        .self_hash
        .copy_from_slice(&twig.active_bits_mtl1[self_ as usize]);
    right[1]
        .peer_hash
        .copy_from_slice(&twig.active_bits_mtl1[peer as usize]);
    right[1].peer_at_left = (peer & 1) == 0;

    let self_ = n / 1024;
    let peer = self_ ^ 1;
    right[2]
        .self_hash
        .copy_from_slice(&twig.active_bits_mtl2[self_ as usize]);
    right[2]
        .peer_hash
        .copy_from_slice(&twig.active_bits_mtl2[peer as usize]);
    right[2].peer_at_left = (peer & 1) == 0;
    right
}

/// Generates the left (entry) side of a proof path using a hash function
///
/// # Arguments
/// * `sn` - Serial number of the entry
/// * `get_hash` - Function that returns the hash at a given index
///
/// # Returns
/// Array of proof nodes for the left path
pub fn get_left_path<F>(sn: u64, mut get_hash: F) -> [ProofNode; 11]
where
    F: FnMut(usize) -> [u8; 32],
{
    let n = sn & TWIG_MASK as u64;
    let mut left = [ProofNode {
        self_hash: [0; 32],
        peer_hash: [0; 32],
        peer_at_left: false,
    }; 11];
    for (level, node) in left.iter_mut().enumerate() {
        let stride = 2048 >> level;
        let self_id = (n >> level) as usize;
        let peer = self_id ^ 1;
        node.self_hash.copy_from_slice(&get_hash(stride + self_id));
        node.peer_hash.copy_from_slice(&get_hash(stride + peer));
        node.peer_at_left = peer & 1 == 0;
    }
    left
}

/// Generates the left side of a proof path from an in-memory Merkle tree
///
/// # Arguments
/// * `mt4twig` - The in-memory Merkle tree
/// * `sn` - Serial number of the entry
///
/// # Returns
/// Array of proof nodes for the left path
pub fn get_left_path_in_mem(mt4twig: &TwigMT, sn: u64) -> [ProofNode; 11] {
    get_left_path(sn, |i| mt4twig[i])
}

/// Generates the left side of a proof path from on-disk twig data
///
/// # Arguments
/// * `tf` - The twig file to read from
/// * `twig_id` - ID of the twig containing the entry
/// * `sn` - Serial number of the entry
///
/// # Returns
/// Array of proof nodes for the left path
pub fn get_left_path_on_disk(tf: &twigfile::TwigFile, twig_id: u64, sn: u64) -> [ProofNode; 11] {
    let mut cache: HashMap<i64, [u8; 32]> = HashMap::with_capacity(8);
    get_left_path(sn, |i| match cache.get(&(i as i64)) {
        Some(v) => {
            let mut res = [0; 32];
            res.copy_from_slice(v);
            res
        }
        None => {
            let mut res = [0; 32];
            tf.get_hash_node(twig_id, i as i64, &mut cache, &mut res);
            res
        }
    })
}
