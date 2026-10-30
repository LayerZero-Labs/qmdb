use std::ops::Range;

use bitvec::prelude::*;

use super::BRANCH_NODE_SIZE;
use crate::{
    beatree::{
        allocator::PageNumber,
        ops::bit_ops::{bitwise_memcpy, reconstruct_key},
        Key,
    },
    io::{page_pool::Page, FatPage, PagePool},
};

// Here is the layout of a branch node:
//
// ```rust,ignore
// bbn_pn: u32            // the page number this is stored under,
//                        // if this is a BBN, 0 otherwise.
//
// n: u16                 // item count
// prefix_compressed: u16 // number of consecutive separators sharing the prefix,
//                        // starting from the first one in the separators list,
//                        // followed by entirely encoded separators
// prefix_len: u16        // bits
// cells: u16[n]          // bit offsets of the end of separators within the separators bitvec
//
// # To avoid two padding bytes, prefix and separators are stored in a single bitvec.
//
// prefix: bitvec[prefix_len]
// separators: bitvec
//
// # Node pointers follow. The list is aligned to the end of the node, with the last item in the
// # list occupying the last 4 bytes of the node.
//
// node_pointers: LNPN or BNID[n]
// ```

const BRANCH_NODE_HEADER_SIZE: usize = 4 + 2 + 2 + 2;
pub const BRANCH_NODE_BODY_SIZE: usize = BRANCH_NODE_SIZE - BRANCH_NODE_HEADER_SIZE;

/// A branch node, regardless of its level.
pub struct BranchNode {
    pub(super) page: FatPage,
}

impl BranchNode {
    pub fn new_in(page_pool: &PagePool) -> Self {
        BranchNode {
            page: page_pool.alloc_fat_page(),
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &*self.page
    }

    pub fn page(&self) -> Page {
        self.page.page()
    }

    pub fn view(&self) -> BranchNodeView<'_> {
        BranchNodeView {
            inner: self.as_slice(),
        }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut *self.page
    }

    pub fn bbn_pn(&self) -> u32 {
        self.view().bbn_pn()
    }

    pub fn set_bbn_pn(&mut self, pn: u32) {
        let slice = self.as_mut_slice();
        slice[0..4].copy_from_slice(&pn.to_le_bytes());
    }

    pub fn n(&self) -> u16 {
        self.view().n()
    }

    pub fn set_n(&mut self, n: u16) {
        let slice = self.as_mut_slice();
        slice[4..6].copy_from_slice(&n.to_le_bytes());
    }

    pub fn prefix_compressed(&self) -> u16 {
        self.view().prefix_compressed()
    }

    pub fn set_prefix_compressed(&mut self, prefix_compressed: u16) {
        let slice = self.as_mut_slice();
        slice[6..8].copy_from_slice(&prefix_compressed.to_le_bytes());
    }

    pub fn prefix_len(&self) -> u16 {
        self.view().prefix_len()
    }

    pub fn set_prefix_len(&mut self, len: u16) {
        let slice = self.as_mut_slice();
        slice[8..10].copy_from_slice(&len.to_le_bytes());
    }

    pub fn prefix(&self) -> &BitSlice<u8, Msb0> {
        self.view().prefix()
    }

    pub fn raw_prefix(&self) -> RawPrefix<'_> {
        self.view().raw_prefix()
    }

    // Set the prefix extracting `self.prefix_len` bits from the provided key
    fn set_prefix(&mut self, key: &[u8; 32]) {
        let start = BRANCH_NODE_HEADER_SIZE + self.n() as usize * 2;
        let prefix_len = self.prefix_len() as usize;
        let end = start + ((prefix_len + 7) / 8).next_multiple_of(8);
        bitwise_memcpy(&mut self.as_mut_slice()[start..end], 0, key, 0, prefix_len);
    }

    fn cells_mut(&mut self) -> &mut [[u8; 2]] {
        let cells_end = BRANCH_NODE_HEADER_SIZE + self.n() as usize * 2;
        assert!(cells_end < BRANCH_NODE_BODY_SIZE);

        // SAFETY: This creates a slice of length 2 * N starting at index BRANCH_NODE_HEADER_SIZE.
        // This is ensured to be within the bounds of the page by the assertion above.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.page[BRANCH_NODE_HEADER_SIZE..BRANCH_NODE_HEADER_SIZE + 2].as_ptr()
                    as *mut [u8; 2],
                self.n().into(),
            )
        }
    }

    pub fn raw_separator(&self, i: usize) -> RawSeparator<'_> {
        self.view().raw_separator(i)
    }

    pub fn raw_separators_mut(&mut self, from: usize, to: usize) -> RawSeparatorsMut<'_> {
        let RawSeparatorsData {
            start,
            byte_len,
            bit_start,
            bit_len,
        } = self.view().raw_separators_data(from, to);

        (&mut self.page[start..start + byte_len], bit_start, bit_len)
    }

    /// Get the total bit-len of the provided half-open range of separators as represented in
    /// this branch.
    pub fn separator_range_len(&self, from: usize, to: usize) -> usize {
        self.view().separator_range_len(from, to)
    }

    fn set_separator(
        &mut self,
        i: usize,
        separator: &BitSlice<u8, Msb0>,
        bit_offset_start: usize,
        bit_offset_end: usize,
    ) {
        let n = self.n() as usize;
        let prefix_len = self.prefix_len() as usize;
        let slice = self.as_mut_slice();

        let cells_start = BRANCH_NODE_HEADER_SIZE + (i * 2);
        slice[cells_start..][..2].copy_from_slice(&(bit_offset_end as u16).to_le_bytes());

        let separators_start = BRANCH_NODE_HEADER_SIZE + (n * 2);
        slice[separators_start..].view_bits_mut()[prefix_len..][bit_offset_start..bit_offset_end]
            .copy_from_bitslice(&separator);
    }

    pub fn node_pointer(&self, i: usize) -> u32 {
        self.view().node_pointer(i)
    }

    pub fn node_pointers_mut(&mut self) -> &mut [[u8; 4]] {
        let node_pointers_byte_len = self.n() as usize * 4;
        assert!(node_pointers_byte_len < BRANCH_NODE_SIZE);

        let node_pointers_init = BRANCH_NODE_SIZE - node_pointers_byte_len;
        // SAFETY: This creates a slice of length 4 * N aligned with the end of the node.
        // This is ensured to be within the bounds of the page by the assertion above.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.page[node_pointers_init..].as_mut_ptr() as *mut [u8; 4],
                self.n() as usize,
            )
        }
    }

    fn set_node_pointer(&mut self, i: usize, node_pointer: u32) {
        let offset = BRANCH_NODE_SIZE - (self.n() as usize - i) * 4;
        self.as_mut_slice()[offset..offset + 4].copy_from_slice(&node_pointer.to_le_bytes());
    }
}

pub struct BranchNodeView<'a> {
    inner: &'a [u8],
}

impl<'a> BranchNodeView<'a> {
    pub fn from_slice(slice: &'a [u8]) -> Self {
        assert_eq!(slice.len(), BRANCH_NODE_SIZE);
        BranchNodeView { inner: slice }
    }

    pub fn bbn_pn(&self) -> u32 {
        u32::from_le_bytes(self.inner[0..4].try_into().unwrap())
    }

    pub fn n(&self) -> u16 {
        u16::from_le_bytes(self.inner[4..6].try_into().unwrap())
    }

    pub fn prefix_compressed(&self) -> u16 {
        u16::from_le_bytes(self.inner[6..8].try_into().unwrap())
    }

    pub fn prefix_len(&self) -> u16 {
        u16::from_le_bytes(self.inner[8..10].try_into().unwrap())
    }

    pub fn cell(&self, i: usize) -> usize {
        let cell_offset = BRANCH_NODE_HEADER_SIZE + (i * 2);
        u16::from_le_bytes(self.inner[cell_offset..][..2].try_into().unwrap()) as usize
    }

    fn cells(&self) -> &[[u8; 2]] {
        let cells_end = BRANCH_NODE_HEADER_SIZE + self.n() as usize * 2;
        assert!(cells_end < BRANCH_NODE_BODY_SIZE);

        // SAFETY: This creates a slice of length 2 * N starting at index BRANCH_NODE_HEADER_SIZE.
        // This is ensured to be within the bounds of the page by the assertion above.
        unsafe {
            std::slice::from_raw_parts(
                self.inner[BRANCH_NODE_HEADER_SIZE..BRANCH_NODE_HEADER_SIZE + 2].as_ptr()
                    as *const [u8; 2],
                self.n().into(),
            )
        }
    }

    pub fn separator(&self, i: usize) -> &'a BitSlice<u8, Msb0> {
        let start_separators = BRANCH_NODE_HEADER_SIZE + self.n() as usize * 2;
        let mut bit_offset_start = self.prefix_len() as usize;
        bit_offset_start += if i != 0 { self.cell(i - 1) } else { 0 };
        let bit_offset_end = self.prefix_len() as usize + self.cell(i);
        &self.inner[start_separators..].view_bits()[bit_offset_start..bit_offset_end]
    }

    fn raw_separators_data(&self, from: usize, to: usize) -> RawSeparatorsData {
        let mut bit_offset_start = self.prefix_len() as usize;
        bit_offset_start += if from != 0 { self.cell(from - 1) } else { 0 };
        let bit_offset_end = self.prefix_len() as usize + self.cell(to - 1);

        let bit_len = bit_offset_end - bit_offset_start;

        let bit_start = bit_offset_start % 8;
        let start_separators = BRANCH_NODE_HEADER_SIZE + self.n() as usize * 2;
        let start = start_separators + (bit_offset_start / 8);
        // load only slices into RawSeparator that have a length multiple of 8 bytes
        let byte_len = if bit_len == 0 {
            0
        } else {
            (((bit_start + bit_len) + 7) / 8).next_multiple_of(8)
        };

        RawSeparatorsData {
            start,
            byte_len,
            bit_start,
            bit_len,
        }
    }

    /// Get the total bit-len of the provided half-open range of separators as represented in
    /// this branch.
    pub fn separator_range_len(&self, from: usize, to: usize) -> usize {
        let bit_offset_start = if from != 0 { self.cell(from - 1) } else { 0 };
        let bit_offset_end = self.cell(to - 1);
        bit_offset_end - bit_offset_start
    }

    pub fn raw_separator(&self, i: usize) -> RawSeparator<'a> {
        self.raw_separators(i, i + 1)
    }

    pub fn raw_separators(&self, from: usize, to: usize) -> RawSeparators<'a> {
        let RawSeparatorsData {
            start,
            byte_len,
            bit_start,
            bit_len,
        } = self.raw_separators_data(from, to);

        (&self.inner[start..start + byte_len], bit_start, bit_len)
    }

    pub fn prefix(&self) -> &'a BitSlice<u8, Msb0> {
        let start = BRANCH_NODE_HEADER_SIZE + self.n() as usize * 2;
        &self.inner[start..].view_bits()[..self.prefix_len() as usize]
    }

    pub fn raw_prefix(&self) -> RawPrefix<'a> {
        let bit_len = self.prefix_len() as usize;

        let start = BRANCH_NODE_HEADER_SIZE + self.n() as usize * 2;
        let end = start + ((bit_len + 7) / 8);

        (&self.inner[start..end], bit_len)
    }

    pub fn node_pointer(&self, i: usize) -> u32 {
        let offset = BRANCH_NODE_SIZE - (self.n() as usize - i) * 4;
        u32::from_le_bytes(self.inner[offset..offset + 4].try_into().unwrap())
    }

    pub fn node_pointers(&self) -> &[[u8; 4]] {
        let node_pointers_byte_len = self.n() as usize * 4;
        assert!(node_pointers_byte_len < BRANCH_NODE_SIZE);

        let node_pointers_init = BRANCH_NODE_SIZE - node_pointers_byte_len;
        // SAFETY: This creates a slice of length 4 * N aligned with the end of the node.
        // This is ensured to be within the bounds of the page by the assertion above.
        unsafe {
            std::slice::from_raw_parts(
                self.inner[node_pointers_init..].as_ptr() as *const [u8; 4],
                self.n() as usize,
            )
        }
    }
}

unsafe impl Send for BranchNode {}

// A RawPrefix is made by a tuple of raw bytes and the relative bit length
pub type RawPrefix<'a> = (&'a [u8], usize);

// Data required to extract one or more separators from the branch node
#[derive(Debug)]
struct RawSeparatorsData {
    // start of the separators within the branch node page
    start: usize,
    // length in bytes of the separators, it must be a multiple of 8
    byte_len: usize,
    // bit offset of the init of the separators within the first byte
    bit_start: usize,
    // bit length of the separators
    bit_len: usize,
}

// RawSeparator and RawSeparators are made by a triple, the raw bytes, the bit-offset
// at which the separators or separator starts to be encoded in the first byte
// and the relative bit length
//
// The raw bytes are always a multiple of 8 bytes in length
pub type RawSeparator<'a> = (&'a [u8], usize, usize);
pub type RawSeparators<'a> = (&'a [u8], usize, usize);
pub type RawSeparatorsMut<'a> = (&'a mut [u8], usize, usize);

pub fn body_size(prefix_len: usize, total_separator_lengths: usize, n: usize) -> usize {
    // prefix plus separator lengths are measured in bits, which we round
    // up to the next byte boundary. They are preceded by cells and followed by node pointers
    (n * 2) + (prefix_len + total_separator_lengths + 7) / 8 + (n * 4)
}

/// Given inputs describing a range of compressed separators, output the sum of the separator
/// lengths when uncompressed.
/// Provide:
///   - The length of the compressed prefix
///   - The length of the compressed range
///   - The number of separators in the range
///   - The length of the first separator in the range.
pub fn uncompressed_separator_range_size(
    prefix_len: usize,
    compressed_lengths: usize,
    n: usize,
    first_len: usize,
) -> usize {
    let first_contraction = prefix_len.saturating_sub(first_len);
    let expansion = prefix_len * n;
    compressed_lengths + expansion - first_contraction
}

/// Given inputs describing a set of separators for a branch, output the compressed size if compressed
/// with the given prefix length.
///
/// `prefix_compressed_items` must be greater than zero.
/// `pre_compression_size_sum` is the sum of all separator lengths, not including the first.
pub fn compressed_separator_range_size(
    first_separator_length: usize,
    prefix_compressed_items: usize,
    pre_compression_size_sum: usize,
    prefix_len: usize,
) -> usize {
    // first length can be less than the shared prefix due to trailing zero compression.
    // then add the total size.
    // then subtract the size difference due to compression of the remaining items.
    first_separator_length.saturating_sub(prefix_len) + pre_compression_size_sum
        - (prefix_compressed_items - 1) * prefix_len
}

// Extract the key at a given index from a BranchNode, taking into account prefix compression.
pub fn get_key(node: &BranchNode, index: usize) -> Key {
    let prefix = if index < node.prefix_compressed() as usize {
        Some(node.raw_prefix())
    } else {
        None
    };
    reconstruct_key(prefix, node.raw_separator(index))
}

pub struct BranchNodeBuilder {
    branch: BranchNode,
    index: usize,
    prefix_len: usize,
    prefix_compressed: usize,
    separator_bit_offset: usize,
}

impl BranchNodeBuilder {
    pub fn new(
        mut branch: BranchNode,
        n: usize,
        prefix_compressed: usize,
        prefix_len: usize,
    ) -> Self {
        branch.set_n(n as u16);
        branch.set_prefix_compressed(prefix_compressed as u16);
        branch.set_prefix_len(prefix_len as u16);

        BranchNodeBuilder {
            branch,
            index: 0,
            prefix_len,
            prefix_compressed,
            separator_bit_offset: 0,
        }
    }

    // returns the number of separtors already pushed
    pub fn n_pushed(&self) -> usize {
        self.index
    }

    pub fn push(&mut self, key: Key, mut separator_len: usize, pn: u32) {
        assert!(self.index < self.branch.n() as usize);

        if self.index == 0 {
            self.branch.set_prefix(&key);
        }

        let separator = if self.index < self.prefix_compressed {
            // The first separator can have length less than prefix due to trailing zero
            // compression.
            separator_len = separator_len.saturating_sub(self.prefix_len);
            &key.view_bits::<Msb0>()[self.prefix_len..][..separator_len]
        } else {
            &key.view_bits::<Msb0>()[..separator_len]
        };

        let offset_start = self.separator_bit_offset;
        let offset_end = self.separator_bit_offset + separator_len;

        self.branch
            .set_separator(self.index, separator, offset_start, offset_end);

        self.separator_bit_offset = offset_end;

        self.branch.set_node_pointer(self.index, pn);

        self.index += 1;
    }

    // Copy the given chunk of separators from the provided base to the new node.
    // Only compressed separators are expected in the specified range `from..to`.
    //
    // `updated` represents a list of new page numbers alongside the index
    // of the separator to update within the specified range
    pub fn push_chunk(
        &mut self,
        base: &BranchNode,
        from: usize,
        to: usize,
        updated: impl Iterator<Item = (usize, PageNumber)>,
    ) {
        let n_items = to - from;
        assert!(self.index + n_items <= self.branch.prefix_compressed() as usize);

        if self.index == 0 {
            // set the prefix if this is the first inserted item
            let key = get_key(base, from);
            self.branch.set_prefix(&key);
        }

        let bit_prefix_len_difference =
            base.prefix_len() as isize - self.branch.prefix_len() as isize;
        let is_prefix_extension = bit_prefix_len_difference.is_positive();
        let bit_prefix_len_difference = bit_prefix_len_difference.abs() as usize;

        // 1. copy and update cells
        // self.separator_bit_offset is the end offset of the last inserted separator
        let mut cell_pointer = self.separator_bit_offset;

        let mut base_prev_cell_pointer = 0;
        if from != 0 {
            base_prev_cell_pointer = base.view().cell(from - 1)
        }

        for (base_cell, cell) in (&base.view().cells()[from..to])
            .iter()
            .zip(&mut self.branch.cells_mut()[self.index..self.index + n_items])
        {
            let base_cell_pointer = u16::from_le_bytes(*base_cell) as usize;
            let mut separator_len = base_cell_pointer - base_prev_cell_pointer;
            base_prev_cell_pointer = base_cell_pointer;

            if is_prefix_extension {
                separator_len += bit_prefix_len_difference;
            } else {
                separator_len = separator_len.saturating_sub(bit_prefix_len_difference);
            }

            cell_pointer += separator_len as usize;
            cell.copy_from_slice(&u16::try_from(cell_pointer).unwrap().to_le_bytes());
        }

        // 2. copy node pointers
        let base_view = base.view();
        let base_node_pointers = &base_view.node_pointers()[from..to];
        self.branch.node_pointers_mut()[self.index..self.index + n_items]
            .copy_from_slice(base_node_pointers);

        // update page numbers of modified separators
        for (i, new_pn) in updated {
            self.branch.set_node_pointer(self.index + i, new_pn.0)
        }

        // 3. copy and shift separators
        if bit_prefix_len_difference == 0 {
            // fast path, copy and shift all compressed separators at once
            let (separators_bytes, separators_bit_start, _separators_bit_len) = self
                .branch
                .raw_separators_mut(self.index, self.index + n_items);

            let (base_separators_bytes, base_separators_bit_start, base_separators_bit_len) =
                base_view.raw_separators(from, to);

            bitwise_memcpy(
                separators_bytes,
                separators_bit_start,
                base_separators_bytes,
                base_separators_bit_start,
                base_separators_bit_len,
            );
        } else {
            // slow path, copy and shift separators one by one
            self.copy_and_shift_separators(
                base,
                self.index..self.index + n_items,
                from..to,
                is_prefix_extension,
                bit_prefix_len_difference,
            );
        }

        self.index += n_items;
        self.separator_bit_offset = cell_pointer;
    }

    fn copy_and_shift_separators(
        &mut self,
        base: &BranchNode,
        range: Range<usize>,
        base_range: Range<usize>,
        is_prefix_extension: bool,
        bit_prefix_len_difference: usize,
    ) {
        if range.is_empty() {
            return;
        }

        let carry_prefix: Option<(usize, usize, usize)> = if is_prefix_extension {
            // bit_prefix_len_difference is the amount of bits that need to be taken from
            // the base prefix and added in front of every separator.
            // The interested bits are the last bits of the prefix

            let bits_to_skip = base.prefix_len() as usize - bit_prefix_len_difference as usize;
            let bytes_to_skip = bits_to_skip / 8;
            let prefix_bit_start = bits_to_skip % 8;

            let base_prefix_start = BRANCH_NODE_HEADER_SIZE + base.n() as usize * 2;
            let start = base_prefix_start + bytes_to_skip;
            let byte_len = (((bit_prefix_len_difference as usize) + 7) / 8).next_multiple_of(8);

            Some((start, start + byte_len, prefix_bit_start))
        } else {
            None
        };

        // iterate over all separators that need to be copied and shifted
        for (index, base_index) in range.clone().into_iter().zip(base_range) {
            let RawSeparatorsData {
                start: mut separator_bytes_start,
                byte_len: separator_bytes_len,
                bit_start: mut separator_bit_start,
                bit_len: separator_bit_len,
            } = self.branch.view().raw_separators_data(index, index + 1);

            let RawSeparatorsData {
                start: mut base_separator_bytes_start,
                byte_len: base_separator_bytes_len,
                bit_start: mut base_separator_bit_start,
                bit_len: base_separator_bit_len,
            } = base.view().raw_separators_data(base_index, base_index + 1);

            // copy the prefix in place if this is_prefix_extension
            if let Some((base_prefix_start, base_prefix_end, base_prefix_bit_start)) = carry_prefix
            {
                bitwise_memcpy(
                    &mut self.branch.page
                        [separator_bytes_start..separator_bytes_start + separator_bytes_len],
                    separator_bit_start,
                    &base.page[base_prefix_start..base_prefix_end],
                    base_prefix_bit_start,
                    bit_prefix_len_difference,
                );
            }

            // shift and copy the separator from the base to the new node.
            //
            // If there is a prefix extension, `handle_prefix_offset` helps to understand
            // how many bytes need to be skipped, given the amount of extended prefix. The same
            // function, in the case of a non-prefix extension, is used to understand how many
            // bits to skip from the base_separator.
            let handle_prefix_offset = |bit_start: &mut usize, byte_init: &mut usize| {
                if *bit_start > 7 {
                    // skip some bytes because they are part of the prefix
                    let bytes_to_skip = *bit_start / 8;
                    *bit_start = *bit_start % 8;
                    *byte_init += bytes_to_skip;
                }
            };

            let bit_len;
            if is_prefix_extension {
                // carry_prefix is already being applied, so skip bits in the new node's separator
                separator_bit_start += bit_prefix_len_difference;
                handle_prefix_offset(&mut separator_bit_start, &mut separator_bytes_start);
                bit_len = base_separator_bit_len;
            } else {
                // the prefix in the new node is bigger, so skip bits in the base's separator
                base_separator_bit_start += bit_prefix_len_difference;
                handle_prefix_offset(
                    &mut base_separator_bit_start,
                    &mut base_separator_bytes_start,
                );
                bit_len = separator_bit_len;
            }

            bitwise_memcpy(
                &mut self.branch.page
                    [separator_bytes_start..separator_bytes_start + separator_bytes_len],
                separator_bit_start,
                &base.page[base_separator_bytes_start
                    ..base_separator_bytes_start + base_separator_bytes_len],
                base_separator_bit_start,
                bit_len,
            );
        }
    }

    pub fn finish(self) -> BranchNode {
        self.branch
    }
}

#[cfg(test)]
mod test {
    use super::get_key;
    use crate::{
        beatree::{branch::BranchNodeBuilder, ops::bit_ops::separator_len},
        io::PagePool,
    };

    lazy_static::lazy_static! {
        static ref PAGE_POOL: PagePool = PagePool::new();
    }

    #[test]
    fn push_chunk() {
        let mut keys = vec![[0u8; 32]; 10];
        for (i, key) in keys.iter_mut().enumerate() {
            key[0] = 0x11;
            if i != 0 {
                key[1] = 128;
                key[2] = i as u8;
            }
        }

        // prefix: 00010001_
        // key 0:           _000...            // cell_poiter: 0
        // key 1:           _10000000_00000001 // cell_poiter: 16
        // key 2:           _10000000_0000001X // cell_poiter: 31
        // key 3:           _10000000_00000011 // cell_poiter: 47
        // ...
        // key 9:           _10000000_00001001
        // prefix_len: 8
        let mut builder = BranchNodeBuilder::new(BranchNode::new_in(&PAGE_POOL), 10, 10, 8);
        for (i, k) in keys.iter().enumerate() {
            builder.push(*k, separator_len(k), i as u32);
        }
        let base_branch = builder.finish();

        use crate::beatree::branch::BranchNode;

        let new_branch = BranchNode::new_in(&PAGE_POOL);
        let mut builder = BranchNodeBuilder::new(
            new_branch, 4, /*n*/
            4, /*prefix_compressed*/
            9, /*prefix_len*/
        );
        builder.push_chunk(&base_branch, 1, 4, [].into_iter());
        let mut key255 = [0; 32];
        key255[0] = 0x11;
        key255[1] = 255;
        key255[2] = 255;
        builder.push(key255, separator_len(&key255), 10);
        // prefix: 00010001_1
        // key p + 1:        0000000_00000001 // cell_poiter: 15
        // key p + 2:        0000000_00000010 // cell_poiter: 29
        // key p + 3:        0000000_00000011 // cell_poiter: 44
        // key 65535:        1111111_11111111
        let branch = builder.finish();
        assert_eq!(get_key(&branch, 0), keys[1]);
        assert_eq!(get_key(&branch, 1), keys[2]);
        assert_eq!(get_key(&branch, 2), keys[3]);
        assert_eq!(get_key(&branch, 3), key255);

        let new_branch = BranchNode::new_in(&PAGE_POOL);
        let mut builder = BranchNodeBuilder::new(
            new_branch, 4, /*n*/
            4, /*prefix_compressed*/
            7, /*prefix_len*/
        );
        builder.push_chunk(&base_branch, 1, 4, [].into_iter());
        builder.push(key255, separator_len(&key255), 10);
        // prefix: 0001000
        // key 1:         1_10000000_00000001 // cell_poiter: 17
        // key 2:         1_10000000_0000001X // cell_poiter: 33
        // key 3:         1_10000000_00000011 // cell_poiter: 50
        // key 255:       1_11111111_11111111
        let branch = builder.finish();
        assert_eq!(get_key(&branch, 0), keys[1]);
        assert_eq!(get_key(&branch, 1), keys[2]);
        assert_eq!(get_key(&branch, 2), keys[3]);
        assert_eq!(get_key(&branch, 3), key255);
    }
}

#[cfg(feature = "benchmarks")]
pub mod benches {
    use crate::{
        beatree::{
            ops::bit_ops::separator_len,
            Key,
            {
                benches::get_keys,
                branch::{BranchNode, BranchNodeBuilder},
            },
        },
        io::{PagePool, PAGE_SIZE},
    };
    use criterion::{BenchmarkId, Criterion};

    pub fn branch_builder_benchmark(c: &mut Criterion) {
        let mut group = c.benchmark_group("branch_builder");

        // benchmark the branch builder creating an almost full branch node
        // given different prefix sizes

        let page_pool = PagePool::new();

        for prefix_len_bytes in [1, 4, 8, 12, 16] {
            // body_size = (2 * n) + (prefix_len_bits + (separator_len_bits * n) + 7)/8 + (4 * n)
            // n = (8 * body_size - prefix_len_bits) / (separator_len_bits + 8*6)
            let body_size_target = PAGE_SIZE - 8;
            let prefix_len_bits = prefix_len_bytes * 8;
            let separator_len_bits = (32 - prefix_len_bytes) * 8;
            let n = (8 * body_size_target - prefix_len_bits) / (separator_len_bits + 8 * 6);

            let mut separators: Vec<(usize, Key)> = get_keys(prefix_len_bytes, n)
                .into_iter()
                .map(|s| (separator_len(&s), s))
                .collect();
            separators.sort_by(|a, b| a.1.cmp(&b.1));

            group.bench_function(
                BenchmarkId::new("prefix_len_bytes", prefix_len_bytes),
                |b| {
                    b.iter_batched(
                        || (BranchNode::new_in(&page_pool), separators.clone()),
                        |(branch_node, separators)| {
                            let mut branch_node_builder =
                                BranchNodeBuilder::new(branch_node, n, n, prefix_len_bits);

                            for (index, (separator_len, separator)) in
                                separators.into_iter().enumerate()
                            {
                                branch_node_builder.push(separator, separator_len, index as u32);
                            }

                            branch_node_builder.finish();
                        },
                        criterion::BatchSize::SmallInput,
                    )
                },
            );
        }

        group.finish();
    }
}
