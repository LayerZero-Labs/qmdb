//! # Head-prunable file
//!
//! Normal files can not be pruned (truncated) from the beginning to some middle position.
//! A `HPFile` uses a sequence of small files to simulate one big virtual file. Thus, pruning
//! from the beginning is to delete the first several small files.
//!
//! A `HPFile` can only be read and appended. Any byteslice which was written to it is
//! immutable.
//!
//! To append a new byteslice into a `HPFile`, use the `append` function, which will return
//! the start position of this byteslice. Later, just pass this start position to `read_at`
//! for reading this byteslice out. The position passed to `read_at` must be the beginning of a
//! byteslice that was written before, instead of its middle. Do NOT try to read the later
//! half (from a middle point to the end) of a byteslice.
//!
//! A `HPFile` can also be truncated: discarding the content from a given position to the
//! end of the file. During truncation, several small files may be removed and one small file
//! may get truncated.
//!
//! A `HPFile` can serve many reader threads. If a reader thread just read random positions,
//! plain `read_at` is enough. If a reader tends to read many adjacent byteslices in sequence, it
//! can take advantage of spatial locality by using `read_at_with_pre_reader`, which uses a
//! `PreReader` to read large chunks of data from file and cache them. Each reader thread can have
//! its own `PreReader`. A `PreReader` cannot be shared by different `HPFile`s.
//!
//! A `HPFile` can serve only one writer thread. The writer thread must own a write buffer that
//! collects small pieces of written data into one big single write to the underlying OS file,
//! to avoid the cost of many syscalls writing the OS file. This write buffer must be provided
//! when calling `append` and `flush`. It is owned by the writer thread, instead of `HPFile`,
//! because we want `HPFile` to be shared between many reader threads.

pub mod aligned_buf;
pub mod file_slots;
pub mod readonly;
pub mod watcher;

pub use aligned_buf::AlignedBuf512;
pub use file_slots::{FileGuard, FileSlots, DEFAULT_CAPACITY};

use anyhow::{anyhow, Result};
use std::{
    fs, io,
    sync::atomic::{AtomicI64, Ordering},
    sync::Arc,
};

use std::fs::File;

#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;

const PRE_READ_BUF_SIZE: usize = 512 * 1024;
pub const IO_BLK_SIZE: usize = 512;

#[cfg(target_os = "linux")]
const DIRECT: i32 = libc::O_DIRECT;

/// Head-prunable file
#[derive(Debug)]
pub struct HPFile {
    dir_name: String,  // where we store the small files
    segment_size: i64, // the size of each small file
    buffer_size: i64,  // the write buffer's size
    /// Lock-free file slots for high-performance concurrent reads
    file_slots: file_slots::FileSlots,
    latest_file_size: AtomicI64,
    file_size: AtomicI64,
}

impl HPFile {
    /// Create a `HPFile` with a directory. If this directory was used by an old HPFile, the old
    /// HPFile must have the same `segment_size` as this one.
    ///
    /// # Parameters
    ///
    /// - `wr_buf_size`: The write buffer used in `append` will not exceed this size
    /// - `segment_size`: The target size of the small files
    /// - `dir_name`: The name of the directory used to store the small files
    ///
    /// # Returns
    ///
    /// A `Result` which is:
    ///
    /// - `Ok`: A successfully initialized `HPFile`
    /// - `Err`: Encounted some file system error.
    ///
    /// # Note
    ///
    /// On Linux, O_DIRECT is automatically enabled for read operations.
    pub fn new(wr_buf_size: i64, segment_size: i64, dir_name: String) -> Result<HPFile> {
        if segment_size % wr_buf_size != 0 {
            return Err(anyhow!(
                "Invalid segmentSize:{} writeBufferSize:{}",
                segment_size,
                wr_buf_size
            ));
        }

        let (id_list, largest_id) = Self::get_file_ids(&dir_name, segment_size)?;
        // Capacity = current files + 256 extra slots for growth
        let capacity = id_list.len() + file_slots::DEFAULT_CAPACITY;
        let file_slots = file_slots::FileSlots::with_capacity(capacity);
        let latest_file_size =
            Self::load_files_into_slots(&dir_name, segment_size, id_list, largest_id, &file_slots)?;

        let file_size = largest_id * segment_size + latest_file_size;
        Ok(HPFile {
            dir_name: dir_name.clone(),
            segment_size,
            buffer_size: wr_buf_size,
            file_slots,
            latest_file_size: AtomicI64::new(latest_file_size),
            file_size: AtomicI64::new(file_size),
        })
    }

    /// Returns the configured segment size (bytes) for this HPFile.
    #[inline]
    pub fn segment_size(&self) -> i64 {
        self.segment_size
    }

    /// Create an empty `HPFile` that has no function and can only be used as placeholder.
    pub fn empty() -> HPFile {
        HPFile {
            dir_name: "".to_owned(),
            segment_size: 0,
            buffer_size: 0,
            file_slots: file_slots::FileSlots::with_capacity(0),
            latest_file_size: AtomicI64::new(0),
            file_size: AtomicI64::new(0),
        }
    }

    /// Returns whether this `HPFile` is empty.
    pub fn is_empty(&self) -> bool {
        self.segment_size == 0
    }

    fn get_file_ids(dir_name: &str, segment_size: i64) -> Result<(Vec<i64>, i64)> {
        let mut largest_id = 0;
        let mut id_list = Vec::new();

        for entry in fs::read_dir(dir_name)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                continue;
            }

            let file_name = entry.file_name().to_string_lossy().to_string();
            let id = Self::parse_filename(segment_size, &file_name)?;
            largest_id = largest_id.max(id);
            id_list.push(id);
        }

        Ok((id_list, largest_id))
    }

    fn parse_filename(segment_size: i64, file_name: &str) -> Result<i64> {
        let parts: Vec<_> = file_name.split("-").collect();
        if parts.len() != 2 {
            return Err(anyhow!(
                "{} does not match the pattern 'FileId-segmentSize'",
                file_name
            ));
        }

        let id: i64 = parts[0].parse()?;
        let size: i64 = parts[1].parse()?;

        if segment_size != size {
            return Err(anyhow!("Invalid Size! {}!={}", size, segment_size));
        }

        Ok(id)
    }

    fn load_files_into_slots(
        dir_name: &str,
        segment_size: i64,
        id_list: Vec<i64>,
        largest_id: i64,
        file_slots: &file_slots::FileSlots,
    ) -> Result<i64> {
        let mut latest_file_size = 0;

        for &id in &id_list {
            let file_path = format!("{dir_name}/{id}-{segment_size}");

            if id == largest_id {
                // Latest segment: open both O_DIRECT (for reading) and non-O_DIRECT (for writing)
                let write_file = File::options().read(true).write(true).open(&file_path)?;
                latest_file_size = write_file.metadata()?.len() as i64;

                #[cfg(target_os = "linux")]
                let direct_file = {
                    let mut opts = File::options();
                    opts.read(true).custom_flags(DIRECT);
                    opts.open(&file_path)?
                };
                #[cfg(not(target_os = "linux"))]
                let direct_file = write_file.try_clone()?;

                file_slots.insert(id, Arc::new(direct_file));
                file_slots.set_current_file(Arc::new(write_file));
            } else {
                // Historical segment: O_DIRECT for reading
                #[cfg(target_os = "linux")]
                let direct_file = {
                    let mut opts = File::options();
                    opts.read(true).custom_flags(DIRECT);
                    opts.open(&file_path)?
                };
                #[cfg(not(target_os = "linux"))]
                let direct_file = File::options().read(true).open(&file_path)?;

                file_slots.insert(id, Arc::new(direct_file));
            }
        }

        if id_list.is_empty() {
            // Create new empty file
            let file_path = format!("{}/{}-{}", &dir_name, 0, segment_size);
            let write_file = File::create_new(&file_path)?;

            #[cfg(target_os = "linux")]
            let direct_file = {
                let mut opts = File::options();
                opts.read(true).custom_flags(DIRECT);
                opts.open(&file_path)?
            };
            #[cfg(not(target_os = "linux"))]
            let direct_file = write_file.try_clone()?;

            file_slots.insert(0, Arc::new(direct_file));
            file_slots.set_current_file(Arc::new(write_file));
        }

        Ok(latest_file_size)
    }

    /// Returns the size of the virtual large file, including the non-flushed bytes
    pub fn size(&self) -> i64 {
        self.file_size.load(Ordering::SeqCst)
    }

    /// Returns the flushed size of the virtual large file
    pub fn size_on_disk(&self) -> i64 {
        if self.is_empty() {
            return 0;
        }
        let largest_id = self.file_slots.largest_id();
        if let Some(guard) = self.file_slots.get(largest_id) {
            if let Some(file) = guard.as_ref() {
                if let Ok(metadata) = file.metadata() {
                    return largest_id * self.segment_size + metadata.len() as i64;
                }
            }
        }
        0
    }

    /// Truncate the file to make it smaller.
    ///
    /// Removes complete segments that exceed `size`, then truncates the last remaining
    /// segment to the exact size needed.
    ///
    /// # Parameters
    ///
    /// - `size`: the size of the virtual large file after truncation. It must be smaller
    ///           than the original size.
    ///
    /// # Edge Case
    ///
    /// When `size` is exactly divisible by `segment_size`, the last segment will be
    /// truncated to 0 bytes (an empty file). This is allowed and the empty segment
    /// file is kept. Example: `size=2GB, segment_size=1GB` results in segments 0,1
    /// (each 1GB) plus an empty segment 2.
    pub fn truncate(&self, size: i64) -> io::Result<()> {
        if self.is_empty() {
            return Ok(());
        }

        if size > self.size() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "truncate size {} cannot exceed current file size {}",
                    size,
                    self.size()
                ),
            ));
        }

        let target_segment_id = size / self.segment_size;
        let remaining_size = size - target_segment_id * self.segment_size;
        let file_path = format!(
            "{}/{}-{}",
            self.dir_name, target_segment_id, self.segment_size
        );

        // Step 1: Remove all segments >= target_segment_id
        while self.file_slots.largest_id() >= target_segment_id {
            if let Some((id, _file)) = self.file_slots.remove_largest() {
                if id != target_segment_id {
                    // Delete file for segments > target_segment_id
                    let path = format!("{}/{}-{}", self.dir_name, id, self.segment_size);
                    let _ = fs::remove_file(&path);
                }
            }
        }

        // Step 2: Truncate the target segment file and use rotate to add it
        let write_file = File::options().read(true).write(true).open(&file_path)?;
        write_file.set_len(remaining_size as u64)?;

        #[cfg(target_os = "linux")]
        let direct_file = {
            let mut opts = File::options();
            opts.read(true).custom_flags(DIRECT);
            opts.open(&file_path)?
        };
        #[cfg(not(target_os = "linux"))]
        let direct_file = write_file.try_clone()?;

        // rotate() will set largest_id to (current + 1) = target_segment_id
        self.file_slots
            .rotate(Arc::new(direct_file), Arc::new(write_file));

        // Step 3: Update metadata
        self.latest_file_size
            .store(remaining_size, Ordering::SeqCst);
        self.file_size.store(size, Ordering::SeqCst);

        Ok(())
    }

    /// Flush the remained data in `buffer` into file system
    ///
    /// # Parameters
    ///
    /// - `buffer`: the write buffer, which is used by the client to call `append`.
    ///
    /// # Returns
    ///
    /// A `Result` which is:
    ///
    /// - `Ok`: It's flushed successfully
    /// - `Err`: Encounted some file system error.
    ///
    pub fn flush(&self, buffer: &mut Vec<u8>) -> io::Result<()> {
        self.flush_internal(buffer, false)
    }

    /// Internal flush with EOF padding option.
    ///
    /// When `eof=true`, padding bytes are added to align the file size to 512 bytes.
    /// This is only used internally during segment rotation and should NOT be called
    /// directly by external code, as continuing to append after `eof=true` will cause
    /// offset misalignment bugs.
    fn flush_internal(&self, buffer: &mut Vec<u8>, eof: bool) -> io::Result<()> {
        if self.is_empty() {
            return Ok(());
        }
        let file_guard = self
            .file_slots
            .get_current()
            .expect("current file must exist");
        let f: &File = file_guard.as_ref().unwrap();
        if !buffer.is_empty() {
            let tail_len = buffer.len() % IO_BLK_SIZE;
            if eof && tail_len != 0 {
                // force the file size aligned with IO_BLK_SIZE
                buffer.resize(buffer.len() + IO_BLK_SIZE - tail_len, 0);
            }
            // Write at the end of file using write_all_at (no seek needed)
            let file_size = f.metadata()?.len();
            f.write_all_at(buffer, file_size)?;
            buffer.clear();
        }

        f.sync_all()
    }

    /// Close the opened small files.
    ///
    /// # Safety Note (io_uring)
    ///
    /// The caller must ensure all in-flight io_uring operations have completed before
    /// calling this. Closing files while async reads are pending leads to undefined behavior.
    pub fn close(&self) {
        self.file_slots.clear();
    }

    /// Returns the corresponding file and in-file position given a logical offset. When we
    /// use io_uring to read data from HPFile, the underlying segment files must be exposed.
    ///
    /// # Parameters
    ///
    /// - `offset`: a logical offset of this HPFile
    ///
    /// # Returns
    ///
    /// A tuple of (FileGuard, pos). Use `guard.as_ref().unwrap()` to get `&File`.
    ///
    /// # Example
    /// ```ignore
    /// let (guard, pos) = hp.get_file_ref(offset);
    /// let file = guard.as_ref().unwrap();
    /// file.read_at(&mut buf, pos as u64)?;
    /// ```
    pub fn get_file_ref(&self, offset: i64) -> (FileGuard, i64) {
        let file_id = offset / self.segment_size;
        let pos = offset % self.segment_size;
        let guard = self.file_slots.get(file_id).expect("file should exist");
        (guard, pos)
    }

    /// Try to get a reference to the file segment containing the given offset.
    ///
    /// Unlike `get_file_ref`, this method returns `None` if the file segment
    /// does not exist, instead of panicking.
    ///
    /// # Returns
    ///
    /// `Some((FileGuard, pos))` if the file exists, `None` otherwise.
    pub fn try_get_file_ref(&self, offset: i64) -> Option<(FileGuard, i64)> {
        let file_id = offset / self.segment_size;
        let pos = offset % self.segment_size;
        let guard = self.file_slots.get(file_id)?;
        Some((guard, pos))
    }

    /// Read data from file at `offset` to fill `bz`
    ///
    /// # Parameters
    ///
    /// - `offset`: the start position of a byteslice that was written before
    ///
    /// # Returns
    ///
    /// A `Result` which is:
    ///
    /// - `Ok`: Number of bytes that was filled into `bz`
    /// - `Err`: Encounted some file system error.
    ///
    pub fn read_at(&self, bz: &mut [u8], offset: i64) -> io::Result<usize> {
        let file_id = offset / self.segment_size;
        let pos = offset % self.segment_size;
        // On Linux, files in slots are opened with O_DIRECT. This can cause short reads near
        // EOF when the on-disk file length is not 512-aligned (common after flush(eof=false)).
        // To avoid tail-alignment issues for the *latest* segment, read it via the buffered
        // (non-O_DIRECT) writable handle when available.
        #[cfg(target_os = "linux")]
        {
            let largest = self.file_slots.largest_id();
            if file_id == largest {
                if let Some(cur_guard) = self.file_slots.get_current() {
                    // Prevent a rare race with segment rotation: if largest_id changed after
                    // we loaded the current handle, fall back to the O_DIRECT slot read.
                    if self.file_slots.largest_id() == largest {
                        if let Some(f) = cur_guard.as_ref() {
                            return f.read_at(bz, pos as u64);
                        }
                    }
                }
            }
            let guard = self.file_slots.get(file_id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("File ID {file_id} not found"),
                )
            })?;
            let f: &File = guard.as_ref().unwrap();
            Self::read_at_aligned(f, bz, pos)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let guard = self.file_slots.get(file_id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("File ID {file_id} not found"),
                )
            })?;
            let f: &File = guard.as_ref().unwrap();
            f.read_at(bz, pos as u64)
        }
    }

    fn read_at_aligned(f: &File, bz: &mut [u8], offset: i64) -> io::Result<usize> {
        let off_in_blk = (offset % IO_BLK_SIZE as i64) as usize;
        let aligned_offset = offset - off_in_blk as i64;

        // Calculate how many bytes we need to read (aligned to IO_BLK_SIZE)
        let bytes_needed = off_in_blk + bz.len();
        let aligned_len = ((bytes_needed + IO_BLK_SIZE - 1) / IO_BLK_SIZE) * IO_BLK_SIZE;

        // Use AlignedBuf512 for proper alignment
        let mut buf = AlignedBuf512::new_zeroed(aligned_len);

        let read_len = f.read_at(buf.as_mut_slice(), aligned_offset as u64)?;

        // Copy the requested portion to output buffer
        let available = read_len.saturating_sub(off_in_blk);
        let copy_len = usize::min(available, bz.len());
        if copy_len > 0 {
            bz[..copy_len].copy_from_slice(&buf.as_slice()[off_in_blk..off_in_blk + copy_len]);
        }
        Ok(copy_len)
    }

    /// Read at most `num_bytes` from file at `offset` to fill `buf`
    ///
    /// # Parameters
    ///
    /// - `buf`: a vector to be filled
    /// - `num_bytes`: the wanted number of bytes to be read
    /// - `offset`: the start position of a byteslice that was written before
    /// - `pre_reader`: a PreReader used to take advantage of spatial locality
    ///
    /// # Returns
    ///
    /// A `Result` which is:
    ///
    /// - `Ok`: Number of bytes that was filled into `buf`
    /// - `Err`: Encounted some file system error.
    ///
    pub fn read_at_with_pre_reader(
        &self,
        buf: &mut Vec<u8>,
        num_bytes: usize,
        offset: i64,
        pre_reader: &mut PreReader,
    ) -> io::Result<usize> {
        if buf.len() < num_bytes {
            buf.resize(num_bytes, 0);
        }

        let file_id = offset / self.segment_size;
        let pos = offset % self.segment_size;

        // See notes in `read_at`: for the latest segment on Linux, prefer buffered reads to
        // avoid O_DIRECT tail short-read behavior on non-512-aligned file lengths.
        #[cfg(target_os = "linux")]
        {
            let largest = self.file_slots.largest_id();
            if file_id == largest {
                if let Some(cur_guard) = self.file_slots.get_current() {
                    if self.file_slots.largest_id() == largest {
                        if let Some(f) = cur_guard.as_ref() {
                            return f.read_at(&mut buf[..num_bytes], pos as u64);
                        }
                    }
                }
            }
        }

        if pre_reader.try_read(file_id, pos, &mut buf[..num_bytes]) {
            return Ok(num_bytes);
        }

        let guard = self.file_slots.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("File ID {file_id} not found"),
            )
        })?;
        let f: &File = guard.as_ref().unwrap();

        if num_bytes >= PRE_READ_BUF_SIZE {
            panic!("Read too many bytes");
        }

        if pos + num_bytes as i64 > self.segment_size {
            return Self::read_at_aligned(f, &mut buf[..num_bytes], pos);
        }

        let blk_size = IO_BLK_SIZE as i64;
        let aligned_pos = (pos / blk_size) * blk_size;
        pre_reader.fill_slice(file_id, aligned_pos, |slice| {
            let remaining = (self.segment_size - aligned_pos).max(0) as usize;
            if remaining == 0 {
                return Ok(0);
            }
            let mut to_read = usize::min(slice.len(), remaining);
            // On Linux (O_DIRECT), read length must be aligned to IO_BLK_SIZE.
            #[cfg(target_os = "linux")]
            {
                let blk = IO_BLK_SIZE;
                if to_read % blk != 0 {
                    to_read = ((to_read + blk - 1) / blk) * blk;
                }
                // Never exceed the backing slice.
                if to_read > slice.len() {
                    to_read = slice.len();
                }
                // If we still ended up with a non-aligned length (shouldn't happen), clamp down.
                if to_read % blk != 0 {
                    to_read = (to_read / blk) * blk;
                }
                if to_read == 0 {
                    // Cannot do a 0-byte O_DIRECT read; treat as EOF for this segment.
                    return Ok(0);
                }
            }
            f.read_at(&mut slice[..to_read], aligned_pos as u64)
                .map(|n| n as i64)
        })?;

        // We may legitimately get a short read near EOF (especially with O_DIRECT and
        // non-512-aligned tails when flush(eof=false)). In that case, return the number
        // of bytes actually available instead of panicking.
        if pre_reader.try_read(file_id, pos, &mut buf[..num_bytes]) {
            return Ok(num_bytes);
        }

        // Partial read fallback: copy whatever is available from the pre-read buffer.
        if file_id == pre_reader.file_id && pre_reader.start <= pos && pos < pre_reader.end {
            let off = (pos - pre_reader.start) as usize;
            let avail = (pre_reader.end - pos) as usize;
            let n = usize::min(avail, num_bytes);
            if n > 0 {
                buf[..n].copy_from_slice(&pre_reader.buffer[off..off + n]);
            }
            return Ok(n);
        }

        Ok(0)
    }

    /// Append a byteslice to the file. This byteslice may be temporarily held in
    /// `buffer` before flushing.
    ///
    /// # Parameters
    ///
    /// - `bz`: the byteslice to append. It cannot be longer than `wr_buf_size` specified
    ///         in `HPFile::new`.
    /// - `buffer`: the write buffer. It will never be larger than `wr_buf_size`.
    ///
    /// # Returns
    ///
    /// A `Result` which is:
    ///
    /// - `Ok`: the start position where this byteslice locates in the file
    /// - `Err`: Encounted some file system error.
    ///
    pub fn append(&self, bz: &[u8], buffer: &mut Vec<u8>) -> io::Result<i64> {
        if self.is_empty() {
            return Ok(0);
        }

        if bz.len() as i64 > self.buffer_size {
            panic!("bz is too large");
        }

        let start_pos = self.size();
        let old_size = self
            .latest_file_size
            .fetch_add(bz.len() as i64, Ordering::SeqCst);
        self.file_size.fetch_add(bz.len() as i64, Ordering::SeqCst);
        let mut split_pos = 0;
        let extra_bytes = (buffer.len() + bz.len()) as i64 - self.buffer_size;
        if extra_bytes > 0 {
            // flush buffer_size bytes to disk
            split_pos = bz.len() - extra_bytes as usize;
            buffer.extend_from_slice(&bz[0..split_pos]);
            // Use current_file (non-O_DIRECT) for writing
            let file_guard = self
                .file_slots
                .get_current()
                .expect("current file must exist");
            let f: &File = file_guard.as_ref().unwrap();
            // Write at end of file using write_all_at
            let file_size = f.metadata()?.len();
            if f.write_all_at(buffer, file_size).is_err() {
                panic!("Fail to write file");
            }
            buffer.clear();
        }
        buffer.extend_from_slice(&bz[split_pos..]); //put remained bytes into buffer
        let overflow_byte_count = old_size + bz.len() as i64 - self.segment_size;
        if overflow_byte_count >= 0 {
            self.flush_internal(buffer, true)?;

            // Create new segment with both O_DIRECT and non-O_DIRECT handles
            let new_file_id = self.file_slots.largest_id() + 1;
            let new_file_path = format!("{}/{}-{}", self.dir_name, new_file_id, self.segment_size);
            let new_write_file = match File::create_new(&new_file_path) {
                Ok(file) => file,
                Err(_) => File::options()
                    .read(true)
                    .write(true)
                    .open(&new_file_path)
                    .unwrap(),
            };

            if overflow_byte_count > 0 {
                // Duplicate the overflow bytes into the new segment at offset 0.
                // This ensures watcher stream can read these bytes from the new segment
                // (since watcher ignores bytes beyond segment_size in the old segment).
                let overflow_len = overflow_byte_count as usize;
                let overflow_start = bz.len() - overflow_len;
                if new_write_file
                    .write_all_at(&bz[overflow_start..], 0)
                    .is_err()
                {
                    panic!("Fail to write overflow bytes to new segment");
                }
                // fsync the overflow bytes to ensure watcher stream consistency.
                // Without this, a crash could lose the overflow bytes in the new segment,
                // causing data loss for watcher consumers (they won't read from old segment's
                // overflow region beyond segment_size).
                if new_write_file.sync_data().is_err() {
                    panic!("Fail to sync overflow bytes to new segment");
                }
            }

            // Open O_DIRECT version for reading
            #[cfg(target_os = "linux")]
            let new_direct_file = {
                let mut opts = File::options();
                opts.read(true).custom_flags(DIRECT);
                opts.open(&new_file_path).unwrap()
            };
            #[cfg(not(target_os = "linux"))]
            let new_direct_file = new_write_file.try_clone().unwrap();

            // Rotate: old current_file is dropped, new segment gets both handles
            self.file_slots
                .rotate(Arc::new(new_direct_file), Arc::new(new_write_file));

            self.latest_file_size
                .store(overflow_byte_count, Ordering::SeqCst);
        }

        Ok(start_pos)
    }

    /// Prune from the beginning to `offset`. This part of the file cannot be read hereafter.
    ///
    /// # Safety Note (io_uring)
    ///
    /// The caller must ensure there are no in-flight io_uring operations on the segments
    /// being pruned. If a segment's file descriptor is closed while an io_uring read is
    /// pending, the read may fail or return garbage data from a reused fd.
    ///
    /// Typically, the upper layer (e.g., prefetcher) should:
    /// 1. Stop submitting new reads for segments below `offset`
    /// 2. Wait for all in-flight reads on those segments to complete
    /// 3. Then call `prune_head(offset)`
    pub fn prune_head(&self, offset: i64) -> io::Result<()> {
        if self.is_empty() {
            return Ok(());
        }

        let file_id = offset / self.segment_size;
        // Remove all files with id < file_id
        for id in 0..file_id {
            if self.file_slots.remove(id).is_some() {
                #[cfg(unix)]
                {
                    let file_name = format!("{}/{}-{}", self.dir_name, id, self.segment_size);
                    let _ = fs::remove_file(file_name); // Ignore errors if file already deleted
                }
            }
        }

        Ok(())
    }
}

/// Pre-read a large chunk of data from file for caching
#[derive(Debug)]
pub struct PreReader {
    buffer: AlignedBuf512, // size is PRE_READ_BUF_SIZE
    file_id: i64,
    start: i64,
    end: i64,
}

impl Default for PreReader {
    fn default() -> Self {
        Self::new()
    }
}

impl PreReader {
    pub fn new() -> Self {
        Self {
            buffer: AlignedBuf512::new_zeroed(PRE_READ_BUF_SIZE),
            file_id: 0,
            start: 0,
            end: 0,
        }
    }

    fn fill_slice<F>(&mut self, file_id: i64, start: i64, access: F) -> io::Result<()>
    where
        F: FnOnce(&mut [u8]) -> io::Result<i64>,
    {
        self.file_id = file_id;
        self.start = start;
        let n = access(&mut self.buffer[..])?;
        self.end = start + n;
        Ok(())
    }

    fn try_read(&self, file_id: i64, start: i64, bz: &mut [u8]) -> bool {
        if file_id == self.file_id && self.start <= start && start + bz.len() as i64 <= self.end {
            let offset = (start - self.start) as usize;
            bz.copy_from_slice(&self.buffer[offset..offset + bz.len()]);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt;

    #[test]
    fn test_hp_file_new() {
        let temp_dir = tempfile::Builder::new()
            .prefix("hp_file_test")
            .tempdir()
            .unwrap();
        let dir = temp_dir.path().to_str().unwrap();
        let buffer_size = 64;
        let segment_size = 128;
        let hp = HPFile::new(buffer_size, segment_size, dir.to_string()).unwrap();
        assert_eq!(hp.buffer_size, buffer_size);
        assert_eq!(hp.segment_size, segment_size);
        assert_eq!(hp.file_slots.largest_id(), 0);

        let slice0 = [1; 44];
        let mut buffer = vec![];
        let mut pos = hp.append(slice0.as_ref(), &mut buffer).unwrap();
        assert_eq!(0, pos);
        assert_eq!(44, hp.size());

        let slice1a = [2; 16];
        let slice1b = [3; 10];
        let mut slice1 = vec![];
        slice1.extend_from_slice(&slice1a);
        slice1.extend_from_slice(&slice1b);
        pos = hp.append(slice1.as_ref(), &mut buffer).unwrap();
        assert_eq!(44, pos);
        assert_eq!(70, hp.size());

        let slice2a = [4; 25];
        let slice2b = [5; 25];
        let mut slice2 = vec![];
        slice2.extend_from_slice(&slice2a);
        slice2.extend_from_slice(&slice2b);
        pos = hp.append(slice2.as_ref(), &mut buffer).unwrap();
        assert_eq!(70, pos);
        assert_eq!(120, hp.size());

        let mut check0 = [0; 44];
        hp.read_at(&mut check0, 0).unwrap();
        assert_eq!(slice0.to_vec(), check0.to_vec());

        hp.flush(&mut buffer).unwrap();

        let mut check1 = [0; 26];
        hp.read_at(&mut check1, 44).unwrap();
        assert_eq!(slice1, check1);

        let mut check2 = [0; 50];
        hp.read_at(&mut check2, 70).unwrap();
        assert_eq!(slice2, check2);

        let slice3 = [0; 16];
        pos = hp.append(slice3.to_vec().as_ref(), &mut buffer).unwrap();
        assert_eq!(120, pos);
        assert_eq!(136, hp.size());

        hp.flush(&mut buffer).unwrap();

        let hp_new = HPFile::new(64, 128, dir.to_string()).unwrap();

        hp_new.read_at(&mut check0, 0).unwrap();
        assert_eq!(slice0.to_vec(), check0.to_vec());

        hp_new.read_at(&mut check1, 44).unwrap();
        assert_eq!(slice1, check1);

        hp_new.read_at(&mut check2, 70).unwrap();
        assert_eq!(slice2, check2);

        let mut check3 = [0; 16];
        hp_new.read_at(&mut check3, 120).unwrap();
        assert_eq!(slice3.to_vec(), check3.to_vec());

        hp_new.prune_head(64).unwrap();
        hp_new.truncate(120).unwrap();
        assert_eq!(hp_new.size(), 120);
        let mut slice4 = vec![];
        hp_new.read_at(&mut slice4, 120).unwrap();
        assert_eq!(slice4.len(), 0);
    }

    #[test]
    fn test_cross_segment_overflow_is_duplicated_to_new_segment() {
        let temp_dir = tempfile::Builder::new()
            .prefix("hp_file_overflow_dup_test")
            .tempdir()
            .unwrap();
        let dir = temp_dir.path().to_str().unwrap();
        // buffer_size must be >= max(slice0.len(), slice1.len()) to avoid panic
        let buffer_size = 128;
        let segment_size = 128;
        let hp = HPFile::new(buffer_size, segment_size, dir.to_string()).unwrap();

        let slice0 = vec![1u8; 100];
        let mut buffer = vec![];
        hp.append(&slice0, &mut buffer).unwrap();
        hp.flush(&mut buffer).unwrap();

        let slice1 = (0..40u8).collect::<Vec<u8>>();
        hp.append(&slice1, &mut buffer).unwrap();

        let overflow_len = (100 + 40) - segment_size as usize;
        let overflow_start = slice1.len() - overflow_len;
        let expected_overflow = &slice1[overflow_start..];

        let old_path = format!("{}/{}-{}", dir, 0, segment_size);
        let new_path = format!("{}/{}-{}", dir, 1, segment_size);
        let old_file = File::open(old_path).unwrap();
        let new_file = File::open(new_path).unwrap();

        let mut old_tail = vec![0u8; overflow_len];
        let mut new_head = vec![0u8; overflow_len];

        old_file
            .read_at(&mut old_tail, (segment_size as usize) as u64)
            .unwrap();
        new_file.read_at(&mut new_head, 0).unwrap();

        assert_eq!(old_tail, expected_overflow);
        assert_eq!(new_head, expected_overflow);
    }

    #[test]
    fn test_read_at_with_pre_reader_allows_short_read_near_eof() {
        let temp_dir = tempfile::Builder::new()
            .prefix("hp_file_prereader_short_read_test")
            .tempdir()
            .unwrap();
        let dir = temp_dir.path().to_str().unwrap();

        // Keep segment_size divisible by buffer_size as required.
        let buffer_size = 64i64;
        let segment_size = 128i64;
        let hp = HPFile::new(buffer_size, segment_size, dir.to_string()).unwrap();

        let mut buffer = vec![];
        let data = vec![9u8; 10];
        hp.append(&data, &mut buffer).unwrap();
        hp.flush(&mut buffer).unwrap();

        // Read more bytes than exist on disk at this offset; should return a short read, not panic.
        let mut pre = PreReader::new();
        let mut out = Vec::new();
        let n = hp
            .read_at_with_pre_reader(&mut out, 32, 0, &mut pre)
            .unwrap();
        assert_eq!(n, 10);
        assert_eq!(&out[..10], &data[..]);
    }

}