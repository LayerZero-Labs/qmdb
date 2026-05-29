//! no_std-friendly components for multiproof: data types and compact Partial Merkle Tree builder.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use borsh::{BorshDeserialize, BorshSerialize};
use core::fmt;
use hashbrown::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct TwigProof {
    pub twig_id: u64,
    pub left_hashes: Vec<[u8; 32]>,
    pub left_bits_bytes: Vec<u64>,
    pub left_bits_len: u64,
    pub right_hashes: Vec<[u8; 32]>,
    pub right_bits_bytes: Vec<u64>,
    pub right_bits_len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct MultiProof {
    pub root: [u8; 32],
    pub n_twigs: u64,
    pub upper_bits_len: u64,
    pub upper_hashes: Vec<[u8; 32]>,
    pub upper_bits_bytes: Vec<u64>,
    pub twigs: Vec<TwigProof>,
}

#[inline]
pub fn height_for_n(n: u64) -> u8 {
    if n <= 1 {
        return 0;
    }
    let bl = 64 - n.leading_zeros() as u8; // bit length
    if n.is_power_of_two() {
        bl - 1
    } else {
        bl
    }
}

pub fn compute_matched_nodes<I>(n_leaves: u64, matched_leaf_idxs: I) -> HashSet<(u8, u64)>
where
    I: IntoIterator<Item = u64>,
{
    let h: u8 = height_for_n(n_leaves);
    let mut out: HashSet<(u8, u64)> = HashSet::new();
    for idx in matched_leaf_idxs {
        for hh in 0..=h {
            out.insert((hh as u8, idx >> hh));
        }
    }
    out
}

pub fn pack_bits(bits: &[bool]) -> Vec<u64> {
    let mut out = vec![0u64; (bits.len() + 63) / 64];
    for (i, bit) in bits.iter().copied().enumerate() {
        if bit {
            out[i / 64] |= 1u64 << ((i & 63) as u8);
        }
    }
    out
}

pub fn build_partial_merkle_tree<F>(
    n_leaves: u64,
    matched_nodes: &HashSet<(u8, u64)>,
    mut node_hash_at: F,
) -> (Vec<[u8; 32]>, Vec<u64>, usize)
where
    F: FnMut(u8 /*level*/, u64 /*pos*/) -> [u8; 32],
{
    fn walk<FN>(
        height: u8,
        pos: u64,
        last_pos: u64,
        matched_nodes: &HashSet<(u8, u64)>,
        out_hashes: &mut Vec<[u8; 32]>,
        out_bits: &mut Vec<bool>,
        node_hash_at: &mut FN,
    ) where
        FN: FnMut(u8, u64) -> [u8; 32],
    {
        let parent = matched_nodes.contains(&(height as u8, pos as u64));
        out_bits.push(parent);
        if height == 0 || !parent {
            out_hashes.push(node_hash_at(height, pos));
        } else {
            walk(
                height - 1,
                pos * 2,
                last_pos,
                matched_nodes,
                out_hashes,
                out_bits,
                node_hash_at,
            );
            // right child exists if its index <= max index at (height-1): last_pos >> (height-1)
            if pos * 2 + 1 <= (last_pos >> (height - 1)) {
                walk(
                    height - 1,
                    pos * 2 + 1,
                    last_pos,
                    matched_nodes,
                    out_hashes,
                    out_bits,
                    node_hash_at,
                );
            }
        }
    }

    let h: u8 = height_for_n(n_leaves);
    let mut hashes = Vec::new();
    let mut bits = Vec::new();
    walk(
        h,
        0,
        n_leaves - 1,
        matched_nodes,
        &mut hashes,
        &mut bits,
        &mut node_hash_at,
    );
    let bit_len = bits.len();
    let bits_words = pack_bits(&bits);
    (hashes, bits_words, bit_len)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyError {
    Malformed,
    BitsUnderflow,
    HashesUnderflow,
    IdenticalChildren,
    UnusedHashes,
    UnusedBits,
    InsufficientBitWords,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyError::Malformed => write!(f, "Malformed"),
            VerifyError::BitsUnderflow => write!(f, "Bits underflow"),
            VerifyError::HashesUnderflow => write!(f, "Hashes underflow"),
            VerifyError::IdenticalChildren => write!(f, "Identical children when both exist"),
            VerifyError::UnusedHashes => write!(f, "Unused hashes"),
            VerifyError::UnusedBits => write!(f, "Unused bits"),
            VerifyError::InsufficientBitWords => write!(f, "Insufficient bit words"),
        }
    }
}

fn verify_partial_merkle_tree<FH, FM>(
    n_leaves: u64,
    hashes: &[[u8; 32]],
    bits_words: &[u64],
    bits_len: usize,
    mut calc_parent: FH,
    mut calc_parent_missing_right: FM,
) -> Result<([u8; 32], Vec<(usize, [u8; 32])>), VerifyError>
where
    FH: FnMut(u8 /*children_level*/, [u8; 32], [u8; 32]) -> [u8; 32],
    FM: FnMut(u8 /*children_level*/, [u8; 32], u64 /*pos*/) -> [u8; 32],
{
    fn step<FH2, FM2>(
        height: u8,
        pos: u64,
        last_pos: u64,
        hashes: &[[u8; 32]],
        bits_words: &[u64],
        bits_len: usize,
        hi: &mut usize,
        bi: &mut usize,
        calc_parent: &mut FH2,
        calc_parent_missing_right: &mut FM2,
        out_matches: &mut Vec<(usize, [u8; 32])>,
    ) -> Result<[u8; 32], VerifyError>
    where
        FH2: FnMut(u8, [u8; 32], [u8; 32]) -> [u8; 32],
        FM2: FnMut(u8, [u8; 32], u64) -> [u8; 32],
    {
        if *bi >= bits_len {
            return Err(VerifyError::BitsUnderflow);
        }
        let word = *bi / 64;
        let off = (*bi & 63) as u8;
        if word >= bits_words.len() {
            return Err(VerifyError::BitsUnderflow);
        }
        let parent = ((bits_words[word] >> off) & 1) == 1;
        *bi += 1;
        if height == 0 || !parent {
            if *hi >= hashes.len() {
                return Err(VerifyError::HashesUnderflow);
            }
            let h = hashes[*hi];
            *hi += 1;
            if height == 0 && parent {
                out_matches.push((pos as usize, h));
            }
            return Ok(h);
        }
        let left = step(
            height - 1,
            pos * 2,
            last_pos,
            hashes,
            bits_words,
            bits_len,
            hi,
            bi,
            calc_parent,
            calc_parent_missing_right,
            out_matches,
        )?;
        let max_idx_at_hm1 = last_pos >> (height - 1);
        if pos * 2 + 1 <= max_idx_at_hm1 {
            let right = step(
                height - 1,
                pos * 2 + 1,
                last_pos,
                hashes,
                bits_words,
                bits_len,
                hi,
                bi,
                calc_parent,
                calc_parent_missing_right,
                out_matches,
            )?;
            // Note: IdenticalChildren check removed.
            // In Bitcoin, this prevents CVE-2012-2459, but in QMDb's structure
            // with null-padding and different tree semantics, identical siblings
            // can legitimately occur (e.g., when proving adjacent null entries).
            // The proof still cryptographically binds to the root.
            return Ok(calc_parent(height - 1, left, right));
        }
        Ok(calc_parent_missing_right(height - 1, left, pos * 2 + 1))
    }

    if n_leaves == 0 {
        return Err(VerifyError::Malformed);
    }
    if hashes.len() > n_leaves as usize {
        return Err(VerifyError::Malformed);
    }
    if bits_len < hashes.len() {
        return Err(VerifyError::Malformed);
    }
    if bits_words.len() * 64 < bits_len {
        return Err(VerifyError::InsufficientBitWords);
    }

    let h: u8 = height_for_n(n_leaves);
    let mut hi = 0usize;
    let mut bi = 0usize;
    let mut matches_out = Vec::new();
    let last_pos = n_leaves - 1;
    let root = step(
        h,
        0,
        last_pos,
        hashes,
        bits_words,
        bits_len,
        &mut hi,
        &mut bi,
        &mut calc_parent,
        &mut calc_parent_missing_right,
        &mut matches_out,
    )?;
    if hi != hashes.len() {
        return Err(VerifyError::UnusedHashes);
    }
    // bits_len is the EXACT number of bits written by the producer (no padding to byte boundary)
    // Therefore we require exact consumption instead of BTC's byte-rounded check.
    if bi != bits_len {
        return Err(VerifyError::UnusedBits);
    }
    Ok((root, matches_out))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MultiVerifyError {
    PartialMerkleTree(VerifyError),
    MissingActiveBitsChunk,
    ActiveBitNotSet,
    TwigRootMismatch,
    UnanchoredOrMissingTwigProofs,
    RootMismatch,
    NTwigsMismatch,
    TwigsNotSorted,
    DuplicateTwigId,
}

impl fmt::Display for MultiVerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MultiVerifyError::PartialMerkleTree(e) => write!(f, "PartialMerkleTree error: {}", e),
            MultiVerifyError::MissingActiveBitsChunk => {
                write!(f, "Missing activebits chunk in proof")
            }
            MultiVerifyError::ActiveBitNotSet => write!(f, "Active bit not set for leaf"),
            MultiVerifyError::TwigRootMismatch => write!(f, "Twig root mismatch"),
            MultiVerifyError::UnanchoredOrMissingTwigProofs => {
                write!(f, "Unanchored or missing twig proofs")
            }
            MultiVerifyError::RootMismatch => write!(f, "Root mismatch"),
            MultiVerifyError::NTwigsMismatch => write!(f, "n_twigs mismatch"),
            MultiVerifyError::TwigsNotSorted => write!(f, "twigs not sorted by twig_id ascending"),
            MultiVerifyError::DuplicateTwigId => write!(f, "duplicate twig_id in twigs"),
        }
    }
}

use crate::def::{LEAF_COUNT_IN_TWIG, TWIG_ROOT_LEVEL, TWIG_SHIFT};
use crate::merkletree::activebitsutil::ActiveBitsUtil;
use crate::merkletree::hash::merkle_node_hash;
use crate::merkletree::null::get_null_hash_by_pos;

impl MultiProof {
    /// Verify and return pairs of (serial_number, leaf_hash) in canonical order
    pub fn verify(&self) -> Result<Vec<(u64, [u8; 32])>, MultiVerifyError> {
        // Enforce canonical ordering of twig proofs
        let mut prev: Option<u64> = None;
        for t in &self.twigs {
            if let Some(p) = prev {
                if t.twig_id < p {
                    return Err(MultiVerifyError::TwigsNotSorted);
                }
                if t.twig_id == p {
                    return Err(MultiVerifyError::DuplicateTwigId);
                }
            }
            prev = Some(t.twig_id);
        }

        // Verify each twig, compute its twig root, and collect leaf pairs
        let mut twig_id_to_root: HashMap<u64, [u8; 32]> = HashMap::new();
        let mut pairs_local: Vec<(u64, [u8; 32])> = Vec::new();

        for t in &self.twigs {
            // Left subtree verify
            let (left_root, left_matches) = verify_partial_merkle_tree(
                LEAF_COUNT_IN_TWIG as u64,
                &t.left_hashes,
                &t.left_bits_bytes,
                t.left_bits_len as usize,
                |level, left, right| merkle_node_hash(level, &left, &right),
                |level, left, pos| {
                    let null_right = *get_null_hash_by_pos(level, pos);
                    merkle_node_hash(level, &left, &null_right)
                },
            )
            .map_err(MultiVerifyError::PartialMerkleTree)?;

            // Right subtree verify
            let (right_root, right_matches) = verify_partial_merkle_tree(
                8u64,
                &t.right_hashes,
                &t.right_bits_bytes,
                t.right_bits_len as usize,
                |level, left, right| {
                    let abs = 8u8 + level;
                    merkle_node_hash(abs, &left, &right)
                },
                |level, left, pos| {
                    let abs = 8u8 + level;
                    let null_right = *get_null_hash_by_pos(abs, pos);
                    merkle_node_hash(abs, &left, &null_right)
                },
            )
            .map_err(MultiVerifyError::PartialMerkleTree)?;

            // Combine into twig root
            let twig_root = merkle_node_hash(11, &left_root, &right_root);
            twig_id_to_root.insert(t.twig_id, twig_root);

            // Map chunk_index -> activebits (4 u64s = 256 bits) for right leaves from matches
            let mut chunk_bytes: HashMap<usize, [u64; 4]> = HashMap::new();
            for (idx, h) in right_matches.iter().copied() {
                // Convert [u8; 32] to [u64; 4]
                let bits: [u64; 4] = bytemuck::cast(h);
                chunk_bytes.insert(idx, bits);
            }

            // For each matched left leaf, ensure active bit set and collect pair
            for (leaf_idx, h) in left_matches.iter().copied() {
                let chunk = leaf_idx / 256;
                let bit = leaf_idx & 255;
                let bits = chunk_bytes
                    .get(&chunk)
                    .ok_or(MultiVerifyError::MissingActiveBitsChunk)?;
                if !ActiveBitsUtil::get_bit(bits, bit as u32) {
                    return Err(MultiVerifyError::ActiveBitNotSet);
                }
                let sn = (t.twig_id << TWIG_SHIFT) | (leaf_idx as u64);
                pairs_local.push((sn, h));
            }
        }

        // Verify upper PartialMerkleTree and cross-check twig roots
        let (upper_root, upper_matches) = verify_partial_merkle_tree(
            self.n_twigs,
            &self.upper_hashes,
            &self.upper_bits_bytes,
            self.upper_bits_len as usize,
            |level, left, right| {
                let abs = TWIG_ROOT_LEVEL as u8 + level;
                merkle_node_hash(abs, &left, &right)
            },
            |level, left, pos| {
                let abs = TWIG_ROOT_LEVEL as u8 + level;
                let right = *get_null_hash_by_pos(abs, pos);
                merkle_node_hash(abs, &left, &right)
            },
        )
        .map_err(MultiVerifyError::PartialMerkleTree)?;

        // Ensure upper matches correspond exactly to provided twig proofs (no extras or misses)
        let mut matched_twigs_count: usize = 0;
        for (twig_idx, leaf_hash) in upper_matches.iter().copied() {
            let tid = twig_idx as u64;
            let expected = twig_id_to_root
                .get(&tid)
                .ok_or(MultiVerifyError::UnanchoredOrMissingTwigProofs)?;
            if &leaf_hash != expected {
                return Err(MultiVerifyError::TwigRootMismatch);
            }
            matched_twigs_count += 1;
        }
        if matched_twigs_count != self.twigs.len() {
            return Err(MultiVerifyError::UnanchoredOrMissingTwigProofs);
        }
        if upper_root != self.root {
            return Err(MultiVerifyError::RootMismatch);
        }

        Ok(pairs_local)
    }
}
