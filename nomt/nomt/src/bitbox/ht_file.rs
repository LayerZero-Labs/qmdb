/// The HT file.
///
/// The file that stores the hash-table buckets and the meta map.
use super::meta_map::MetaMap;
use crate::io::{self, PagePool, PAGE_SIZE};
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
};

/// The offsets of the HT file.
#[derive(Clone)]
pub struct HTOffsets {
    // the number of pages to add to a page number to find its real location in the file,
    // taking account of the meta page and meta byte pages.
    data_page_offset: u64,
}

impl HTOffsets {
    /// Returns the page number of the `ix`th item in the data section of the store.
    pub fn data_page_index(&self, ix: u64) -> u64 {
        self.data_page_offset + ix
    }

    /// Returns the page number of the `ix`th item in the meta bytes section of the store.
    pub fn meta_bytes_index(&self, ix: u64) -> u64 {
        ix
    }
}

fn expected_file_len(num_pages: u32) -> u64 {
    (num_meta_byte_pages(num_pages) + num_pages) as u64 * PAGE_SIZE as u64
}

fn num_meta_byte_pages(num_pages: u32) -> u32 {
    (num_pages + 4095) / PAGE_SIZE as u32
}

/// Opens the HT file, checks its length and reads the meta map.
pub fn open(
    num_pages: u32,
    page_pool: &PagePool,
    ht_fd: &File,
) -> anyhow::Result<(HTOffsets, MetaMap)> {
    if ht_fd.metadata()?.len() != expected_file_len(num_pages) {
        anyhow::bail!("Store corrupted; unexpected file length");
    }

    let num_meta_byte_pages = num_meta_byte_pages(num_pages);
    let mut meta_bytes = Vec::with_capacity(num_meta_byte_pages as usize * PAGE_SIZE);
    for pn in 0..num_meta_byte_pages {
        let extra_meta_page = io::read_page(page_pool, ht_fd, pn as u64)?;
        meta_bytes.extend_from_slice(&*extra_meta_page);
    }

    let data_page_offset = num_meta_byte_pages as u64;
    Ok((
        HTOffsets { data_page_offset },
        MetaMap::from_bytes(meta_bytes, num_pages as usize),
    ))
}

/// Creates the store file. Fails if store file already exists.
///
/// Lays out the meta page. If `preallocate` is true, preallocates the blocks for the file.
pub fn create(path: PathBuf, num_pages: u32, preallocate: bool) -> std::io::Result<()> {
    let ht_path = path.join("ht");
    let ht_file = OpenOptions::new().write(true).create(true).open(ht_path)?;

    // number of pages + pages required for meta bits.
    let page_count = num_pages + num_meta_byte_pages(num_pages);
    let len = page_count as usize * PAGE_SIZE;

    resize_and_prealloc(&ht_file, len as u64, preallocate)?;

    ht_file.sync_all()?;
    drop(ht_file);

    let wal_path = path.join("wal");
    let wal_file = OpenOptions::new().write(true).create(true).open(wal_path)?;
    wal_file.sync_all()?;
    drop(wal_file);
    Ok(())
}

/// Sets the file size and attempts to preallocate the file if `preallocate` is true.
///
/// Returns an error if setting the file size fails. File preallocation is done on a best-effort basis
/// and may silently fall back to regular allocation.
///
/// After this call, if successful, the file size is set to `len` bytes.
fn resize_and_prealloc(ht_file: &File, len: u64, preallocate: bool) -> std::io::Result<()> {
    if !preallocate {
        // If not preallocating, just set the file size and return.
        ht_file.set_len(len)?;
        return Ok(());
    }

    cfg_if::cfg_if! {
        if #[cfg(target_os = "linux")] {
            // To preallocate on Linux systems, try using fallocate with ZERO_RANGE first as it's more
            // efficient. fallocate sets the file size as well, so ftruncate (aka file.set_len()) is
            // not needed.
            if crate::sys::linux::fs_check(ht_file).map_or(false, |fsck| fsck.is_tmpfs()) {
                // Skip preallocation for tmpfs. It doesn't support fallocate and it's
                // memory-backed anyway. ftruncate and bail.
                ht_file.set_len(len)?;
                return Ok(());
            }
            if let Err(_) = crate::sys::linux::falloc_zero_file(ht_file, len) {
                // If fallocate fails, fall back to zeroing the file with write.
                resize_and_zero_file(ht_file, len)?;
            }
        } else {
            resize_and_zero_file(ht_file, len)?;
        }
    }

    Ok(())
}

// Fallback method for allocating extents for the file: just incrementally write zeroes to the file.
fn resize_and_zero_file(mut file: &File, len: u64) -> std::io::Result<()> {
    use std::io::Write;

    // Set the file size first.
    file.set_len(len)?;

    // Zero the file.
    let len = len as usize;
    let buf = [0u8; PAGE_SIZE * 4];
    let mut remaining = len;
    while remaining > 0 {
        let len = std::cmp::min(remaining, buf.len());
        file.write_all(&buf[..len])?;
        remaining -= len;
    }
    Ok(())
}
