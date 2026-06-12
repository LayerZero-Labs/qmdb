//! Aligned buffer types for O_DIRECT I/O operations.
//!
//! This module provides buffer types with guaranteed memory alignment,
//! which is required for O_DIRECT file operations on Linux.
//!
//! # Background
//!
//! O_DIRECT requires:
//! - Buffer address aligned to filesystem block size (typically 512 bytes)
//! - Read/write offset aligned to block size
//! - Read/write length aligned to block size
//!
//! # Design
//!
//! We use a 512-byte block as the fundamental unit: `Block512([u8; 512])`.
//! This ensures:
//! - `size_of::<Block512>() == 512` (no memory bloat)
//! - `align_of::<Block512>() == 512` (correct alignment)
//! - Vec<Block512> has contiguous, properly aligned memory
//! - Drop is safe (no layout mismatch)

use std::ops::{Deref, DerefMut, RangeBounds};

/// A 512-byte aligned block.
///
/// This is the fundamental unit for aligned I/O operations.
/// Each block is exactly 512 bytes with 512-byte alignment.
#[repr(C, align(512))]
#[derive(Clone, Copy)]
pub struct Block512([u8; 512]);

impl Block512 {
    /// Size of each block in bytes.
    pub const SIZE: usize = 512;

    /// Create a new zeroed block.
    #[inline]
    pub const fn zeroed() -> Self {
        Self([0u8; 512])
    }
}

impl Default for Block512 {
    #[inline]
    fn default() -> Self {
        Self::zeroed()
    }
}

/// Buffer with 512-byte alignment for O_DIRECT operations.
///
/// This buffer guarantees that:
/// - The starting address is aligned to 512 bytes
/// - Memory is contiguous (no padding between bytes)
/// - Resize operations maintain alignment
/// - Drop is safe (no UB from layout mismatch)
///
/// # Example
///
/// ```ignore
/// use hpfile::AlignedBuf512;
///
/// let mut buf = AlignedBuf512::new_zeroed(1024);
/// assert!(buf.is_aligned());
/// assert_eq!(buf.len(), 1024);
///
/// // Use as a regular byte slice
/// buf[0] = 42;
/// assert_eq!(buf[0], 42);
/// ```
pub struct AlignedBuf512 {
    /// Internal storage as blocks
    blocks: Vec<Block512>,
    /// Actual byte length (may be less than blocks.len() * 512)
    len: usize,
}

impl AlignedBuf512 {
    /// Create a new buffer with the given size (bytes will be zeroed).
    ///
    /// The internal storage is rounded up to the nearest multiple of 512 bytes,
    /// but `len()` returns the requested size.
    #[inline]
    pub fn new_zeroed(size: usize) -> Self {
        let num_blocks = (size + Block512::SIZE - 1) / Block512::SIZE;
        Self {
            blocks: vec![Block512::zeroed(); num_blocks],
            len: size,
        }
    }

    /// Create a new buffer with uninitialized memory.
    ///
    /// # Safety
    /// The contents are uninitialized. Caller must ensure all bytes are
    /// initialized before reading via `as_slice()`, `Deref`, or any other
    /// method that exposes the buffer contents.
    #[inline]
    pub unsafe fn new_uninit(size: usize) -> Self {
        let num_blocks = (size + Block512::SIZE - 1) / Block512::SIZE;
        let mut blocks = Vec::with_capacity(num_blocks);
        // SAFETY: Block512 is Copy and has no drop glue
        blocks.set_len(num_blocks);
        Self { blocks, len: size }
    }

    /// Returns the length of the buffer in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns true if the buffer is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the capacity of the buffer in bytes (rounded up to block size).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.blocks.capacity() * Block512::SIZE
    }

    /// Returns the actual allocated size in bytes (always a multiple of 512).
    #[inline]
    pub fn allocated_size(&self) -> usize {
        self.blocks.len() * Block512::SIZE
    }

    /// Returns the alignment of this buffer type.
    #[inline]
    pub const fn alignment() -> usize {
        Block512::SIZE
    }

    /// Returns a raw pointer to the buffer.
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.blocks.as_ptr() as *const u8
    }

    /// Returns a mutable raw pointer to the buffer.
    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.blocks.as_mut_ptr() as *mut u8
    }

    /// Returns the buffer as a byte slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: Block512 is contiguous [u8; 512] with 512-byte alignment.
        // Vec<Block512> guarantees contiguous storage.
        unsafe { std::slice::from_raw_parts(self.as_ptr(), self.len) }
    }

    /// Returns the buffer as a mutable byte slice.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.as_mut_ptr(), self.len) }
    }

    /// Returns the full allocated buffer as a byte slice (length rounded to block size).
    #[inline]
    pub fn as_full_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.as_ptr(), self.allocated_size()) }
    }

    /// Returns the full allocated buffer as a mutable byte slice.
    #[inline]
    pub fn as_full_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.as_mut_ptr(), self.allocated_size()) }
    }

    /// Returns true if the buffer is properly aligned.
    #[inline]
    pub fn is_aligned(&self) -> bool {
        (self.as_ptr() as usize) % Block512::SIZE == 0
    }

    /// Resizes the buffer to the new length.
    ///
    /// If `new_len` is greater than the current length, new bytes are zeroed.
    /// Alignment is maintained after resize.
    #[inline]
    pub fn resize(&mut self, new_len: usize) {
        let new_blocks = (new_len + Block512::SIZE - 1) / Block512::SIZE;
        self.blocks.resize(new_blocks, Block512::zeroed());
        self.len = new_len;
    }

    /// Copies elements from one part of the buffer to another part of itself.
    ///
    /// `src` is the range within the buffer to copy from.
    /// `dest` is the starting index of the destination.
    #[inline]
    pub fn copy_within<R>(&mut self, src: R, dest: usize)
    where
        R: RangeBounds<usize> + Clone,
    {
        self.as_mut_slice().copy_within(src, dest);
    }

    /// Sets the logical length of the buffer.
    ///
    /// # Safety
    /// Caller must ensure that:
    /// - `new_len <= allocated_size()`
    /// - All bytes in 0..new_len are properly initialized
    #[inline]
    pub unsafe fn set_len(&mut self, new_len: usize) {
        debug_assert!(new_len <= self.allocated_size());
        self.len = new_len;
    }

    /// Clears the buffer (sets length to 0, but keeps capacity).
    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
        // Keep blocks for reuse
    }

    /// Extends the buffer with zeros.
    #[inline]
    pub fn extend_zeroed(&mut self, additional: usize) {
        self.resize(self.len + additional);
    }
}

impl Deref for AlignedBuf512 {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl DerefMut for AlignedBuf512 {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut_slice()
    }
}

impl std::fmt::Debug for AlignedBuf512 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlignedBuf512")
            .field("ptr", &self.as_ptr())
            .field("len", &self.len)
            .field("allocated", &self.allocated_size())
            .field("aligned", &self.is_aligned())
            .finish()
    }
}

impl Clone for AlignedBuf512 {
    fn clone(&self) -> Self {
        Self {
            blocks: self.blocks.clone(),
            len: self.len,
        }
    }
}

impl Default for AlignedBuf512 {
    fn default() -> Self {
        Self::new_zeroed(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_size() {
        assert_eq!(std::mem::size_of::<Block512>(), 512);
        assert_eq!(std::mem::align_of::<Block512>(), 512);
    }

    #[test]
    fn test_alignment() {
        let buf = AlignedBuf512::new_zeroed(1024);
        assert!(buf.is_aligned(), "Buffer should be 512-byte aligned");
        assert_eq!(buf.len(), 1024);
        assert_eq!(buf.allocated_size(), 1024); // 2 blocks exactly
    }

    #[test]
    fn test_no_memory_bloat() {
        // Verify that we don't have the 512x bloat issue
        let buf = AlignedBuf512::new_zeroed(512 * 1024); // 512KB
        assert_eq!(buf.len(), 512 * 1024);
        assert_eq!(buf.allocated_size(), 512 * 1024); // Should be exactly 512KB, not 256MB
    }

    #[test]
    fn test_alignment_after_resize() {
        let mut buf = AlignedBuf512::new_zeroed(512);
        assert!(buf.is_aligned());

        buf.resize(2048);
        assert!(buf.is_aligned(), "Buffer should remain aligned after resize");
        assert_eq!(buf.len(), 2048);

        buf.resize(256);
        assert!(buf.is_aligned(), "Buffer should remain aligned after shrink");
        assert_eq!(buf.len(), 256);
    }

    #[test]
    fn test_read_write() {
        let mut buf = AlignedBuf512::new_zeroed(512);

        // Write via slice
        buf.as_mut_slice()[0] = 42;
        buf.as_mut_slice()[511] = 255;

        // Read via slice
        assert_eq!(buf.as_slice()[0], 42);
        assert_eq!(buf.as_slice()[511], 255);

        // Deref access
        assert_eq!(buf[0], 42);
        assert_eq!(buf[511], 255);
    }

    #[test]
    fn test_small_sizes() {
        // Even small sizes should be aligned
        for size in [1, 7, 64, 100, 511, 512, 513, 1000] {
            let buf = AlignedBuf512::new_zeroed(size);
            assert!(
                buf.is_aligned(),
                "Buffer of size {} should be aligned",
                size
            );
            assert_eq!(buf.len(), size);
            // Allocated size is rounded up
            let expected_alloc = ((size + 511) / 512) * 512;
            assert_eq!(buf.allocated_size(), expected_alloc);
        }
    }

    #[test]
    fn test_contiguous_memory() {
        // Verify memory is truly contiguous (no padding between bytes)
        let mut buf = AlignedBuf512::new_zeroed(1024);

        // Write sequential values
        for i in 0..1024 {
            buf[i] = (i % 256) as u8;
        }

        // Verify they're all correct and contiguous
        for i in 0..1024 {
            assert_eq!(buf[i], (i % 256) as u8);
        }

        // Verify via raw pointer arithmetic
        let ptr = buf.as_ptr();
        for i in 0..1024 {
            unsafe {
                assert_eq!(*ptr.add(i), (i % 256) as u8);
            }
        }
    }

    #[test]
    fn test_copy_within() {
        let mut buf = AlignedBuf512::new_zeroed(100);
        for i in 0..50 {
            buf[i] = i as u8;
        }

        buf.copy_within(0..50, 50);

        for i in 0..50 {
            assert_eq!(buf[i], i as u8);
            assert_eq!(buf[i + 50], i as u8);
        }
    }

    #[test]
    fn test_full_slice() {
        let buf = AlignedBuf512::new_zeroed(100);
        assert_eq!(buf.len(), 100);
        assert_eq!(buf.as_slice().len(), 100);
        assert_eq!(buf.as_full_slice().len(), 512); // Rounded up to block size
    }
}
