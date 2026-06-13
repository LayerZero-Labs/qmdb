//! The write-path for the WAL.

use super::{WAL_ENTRY_TAG_CLEAR, WAL_ENTRY_TAG_END, WAL_ENTRY_TAG_START, WAL_ENTRY_TAG_UPDATE};
use crate::{io::PAGE_SIZE, merkle::ElidedChildren, page_diff::PageDiff};

const MAX_SIZE: usize = 1 << 37; // 128 GiB

struct Mmap {
    ptr: *mut u8,
    size: usize,
}

impl Mmap {
    fn new(size: usize) -> anyhow::Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                -1,
                0,
            )
        } as *mut u8;
        if ptr == libc::MAP_FAILED as *mut u8 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("mmap failed: {err:?}");
        }
        Ok(Self { ptr, size })
    }

    /// Try to resize this mapping.
    ///
    /// The `new_size` should be a multiple of the page size.
    ///
    /// Upon success, returns true and the existing mapping is shrunk or grown to `new_size`
    /// updating `self.size` to the new size and potentially `self.ptr` to point to the new
    /// mapping, if the kernel decided to move it.
    fn resize(&mut self, new_size: usize) -> bool {
        assert!(new_size % PAGE_SIZE == 0);

        #[cfg(target_os = "linux")]
        unsafe {
            let new_ptr = libc::mremap(
                self.ptr as *mut libc::c_void,
                self.size,
                new_size,
                libc::MREMAP_MAYMOVE,
            );
            if new_ptr == libc::MAP_FAILED {
                false
            } else {
                self.ptr = new_ptr as *mut u8;
                self.size = new_size;
                true
            }
        }

        // Non-linux platforms don't support mremap, so we don't support resizing WAL mmaping.
        #[cfg(not(target_os = "linux"))]
        false
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::munmap(self.ptr as *mut libc::c_void, self.size);
        }
    }
}

/// A builder for a WAL blob.
pub struct WalBlobBuilder {
    mmap: Mmap,
    /// The position at which the next byte will be written. Never reaches `mmap.size`.
    cur: usize,
}

impl WalBlobBuilder {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_initial_size(1 << 30)
    }

    fn with_initial_size(size: usize) -> anyhow::Result<Self> {
        let mmap = Mmap::new(size)?;
        Ok(Self { mmap, cur: 0 })
    }

    pub fn write_clear(&mut self, bucket_index: u64) {
        unsafe {
            self.write_byte(WAL_ENTRY_TAG_CLEAR);
            self.write(&bucket_index.to_le_bytes());
        }
    }

    pub fn write_update(
        &mut self,
        page_id: [u8; 32],
        page_diff: &PageDiff,
        changed: impl Iterator<Item = [u8; 32]>,
        elided_children: ElidedChildren,
        bucket_index: u64,
    ) {
        unsafe {
            // SAFETY: Those do not overlap with the mmap.
            self.write_byte(WAL_ENTRY_TAG_UPDATE);
            self.write(&page_id);
            self.write(&page_diff.as_bytes());
            for changed in changed {
                self.write(&changed);
            }
            self.write(&elided_children.to_bytes());
            self.write(&bucket_index.to_le_bytes());
        }
    }

    fn write_byte(&mut self, byte: u8) {
        unsafe {
            // SAFETY: This slice trivially does not overlap with the mmap.
            self.write(&[byte]);
        }
    }

    /// # Safety
    ///
    /// The `bytes` mut not overlap with the mmap.
    unsafe fn write(&mut self, bytes: &[u8]) {
        let pos = self.cur;
        let new_cur = self.cur.checked_add(bytes.len()).unwrap();
        if new_cur >= self.mmap.size {
            // Grow once to the required size
            if let Err(e) = self.grow(new_cur) {
                panic!("WAL blob too large: {e}");
            }
        }
        // NB: `self.mmap` could have changed after the grow.
        self.cur = new_cur;
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.mmap.ptr.add(pos), bytes.len());
        }
    }

    /// Grow the mmap to at least `min_new_size` bytes.
    ///
    /// Since this grows by at least doubling the current size, the resulting size might be
    /// larger than `min_new_size`.
    ///
    /// Returns `Ok(())` if the mmap was successfully resized.
    #[cold]
    fn grow(&mut self, min_new_size: usize) -> anyhow::Result<()> {
        if self.mmap.size >= MAX_SIZE {
            anyhow::bail!("WAL blob too large (exceeded 128 GiB limit)");
        }

        // Start with current size and double until we cover min_new_size
        let mut new_size = self.mmap.size;
        while new_size < min_new_size && new_size < MAX_SIZE {
            new_size = new_size.saturating_mul(2);
        }
        new_size = new_size.min(MAX_SIZE);

        if self.mmap.resize(new_size) {
            return Ok(());
        }

        // Resizing in place did not succeed. We could try to create a new mapping and copy
        // the data over.
        let new_mmap = Mmap::new(new_size)?;
        unsafe {
            std::ptr::copy_nonoverlapping(self.mmap.ptr, new_mmap.ptr, self.mmap.size);
        }
        self.mmap = new_mmap;
        Ok(())
    }

    /// Resets the builder preparing it for a new batch of writes.
    ///
    /// Provide the sync sequence number.
    pub fn reset(&mut self, sync_seqn: u32) {
        self.cur = 0;

        self.write_byte(WAL_ENTRY_TAG_START);

        // SAFETY: these bytes live on the stack and do not overlap with the map.
        unsafe { self.write(&sync_seqn.to_le_bytes()) }
    }

    /// Finalizes the builder.
    ///
    /// It's possible to overwrite the data in the blob after calling this function so don't keep
    /// the pointer around for too long.
    ///
    /// The pointer is aligned to the page size.
    pub fn finalize(&mut self) {
        self.write_byte(WAL_ENTRY_TAG_END);

        let ptr = self.mmap.ptr;
        // round up to the nearest page size.
        let len = (self.cur + PAGE_SIZE - 1) / PAGE_SIZE * PAGE_SIZE;

        // Note we don't madvise(DONTNEED) or any other tricks for now. If we did, then each time
        // we write a page it would need to be mounted and thus zero filled.
        //
        // The hope is that should there be memory  pressure, the memory would be swapped out as
        // needed.
        let cur = self.cur;
        unsafe {
            // Zero memory from `cur` to the end of the blob (which is `len`).
            let dst = ptr.add(cur);
            let count = len - cur;
            if count > 0 {
                // SAFETY:
                // - `dst` is never null.
                // - `dst` is always naturally aligned because it's a byte.
                // - `cur` is always less than the size of the mmap. Thus `dst + count` never lands
                //   past the end of the mmap.
                // - `0` is a valid value for `u8`.
                std::ptr::write_bytes(dst, 0, count);
            }
        }
        self.cur = len;
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.mmap.ptr, self.cur) }
    }
}

unsafe impl Send for WalBlobBuilder {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blob_builder_smoke() {
        let _builder = WalBlobBuilder::with_initial_size(4096).unwrap();
    }

    #[test]
    fn test_blob_builder_grows() {
        let mut builder = WalBlobBuilder::with_initial_size(4096).unwrap();

        // Fill up most of the initial capacity
        let data = vec![42u8; 4000];
        unsafe { builder.write(&data) };
        assert_eq!(builder.cur, 4000);
        assert_eq!(builder.mmap.size, 4096);

        // Write more data that forces a grow
        let more_data = vec![43u8; 2000];
        unsafe { builder.write(&more_data) };

        // Should have grown to accommodate the additional data
        assert!(builder.mmap.size > 4096);
        assert_eq!(builder.cur, 6000);

        // Verify we can still write after growing
        builder.write_byte(44);
        assert_eq!(builder.cur, 6001);
    }

    #[test]
    fn test_blob_builder_multiple_grows() {
        let mut builder = WalBlobBuilder::with_initial_size(1024).unwrap();

        // Trigger multiple grows by writing increasingly large chunks
        for i in 0..5 {
            let size = 1000 * (i + 1);
            let data = vec![i as u8; size];
            unsafe { builder.write(&data) };
        }

        // Should have grown multiple times to fit all data
        assert!(builder.mmap.size >= 15000);
        assert_eq!(builder.cur, 15000);
    }
}
