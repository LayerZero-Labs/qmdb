//! MultiEntryFileUringReader: io_uring-based multi-EntryFile reader
//!
//! Provides efficient read services for `Vec<Arc<EntryFile>>` using a single io_uring instance.
//! Callers specify the EntryFile to read via `shard_id` index.
//!
//! Design features:
//! 1. Single io_uring instance for simplified management
//! 2. EntryFile indexed by shard_id
//! 3. Supports both sync and async read modes
//! 4. Buffer pool reuse to reduce allocation overhead
//! 5. Handles O_DIRECT alignment requirements
//! 6. Automatic decryption (if EntryFile has encryption enabled)

#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
use hpfile::{AlignedBuf512, IO_BLK_SIZE};
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
use qmdb_common::entryfile::entry::EntryBz;
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
use qmdb_common::entryfile::readbuf::ReadBuf;
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
use std::mem;
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
use std::ops::Deref;
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
use std::os::unix::io::AsRawFd;
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
use std::sync::Arc;
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
use tokio::sync::{mpsc, oneshot, RwLock};

#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
use super::EntryFile;

/// Sender for completion notification - supports both async and sync receivers
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
pub enum DoneSender {
    /// For tokio tasks
    Async(oneshot::Sender<usize>),
    /// For blocking threads
    Sync(std::sync::mpsc::SyncSender<usize>),
}

#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
impl DoneSender {
    fn send(self, value: usize) {
        match self {
            DoneSender::Async(tx) => {
                let _ = tx.send(value);
            }
            DoneSender::Sync(tx) => {
                let _ = tx.send(value);
            }
        }
    }
}

/// Receiver for completion notification - supports both async and sync
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
pub enum DoneReceiver {
    /// For tokio tasks
    Async(oneshot::Receiver<usize>),
    /// For blocking threads
    Sync(std::sync::mpsc::Receiver<usize>),
}

#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
impl DoneReceiver {
    /// Await completion asynchronously (for tokio tasks)
    pub async fn recv_async(self) -> Option<usize> {
        match self {
            DoneReceiver::Async(rx) => rx.await.ok(),
            DoneReceiver::Sync(rx) => {
                // Wrap blocking recv in spawn_blocking to not block tokio runtime
                tokio::task::spawn_blocking(move || rx.recv().ok())
                    .await
                    .ok()
                    .flatten()
            }
        }
    }

    /// Block until completion (for regular threads)
    pub fn recv_blocking(self) -> Option<usize> {
        match self {
            DoneReceiver::Async(rx) => rx.blocking_recv().ok(),
            DoneReceiver::Sync(rx) => rx.recv().ok(),
        }
    }
}

/// Read request sent to the uring worker thread.
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
pub struct UringReadRequest {
    /// Shard ID (index into entry_files)
    pub shard_id: usize,
    /// Raw file descriptor for io_uring operation
    pub fd: std::os::unix::io::RawFd,
    /// Aligned offset within the file to read from (aligned to IO_BLK_SIZE)
    pub aligned_offset: i64,
    /// Offset within the block where actual data starts
    pub off_in_blk: usize,
    /// Original file position (used for decryption nonce)
    pub file_pos: i64,
    /// Shared buffer for the result (512-byte aligned).
    pub buffer: Arc<RwLock<AlignedBuf512>>,
    /// Expected read length (aligned to IO_BLK_SIZE)
    pub expected_len: usize,
    /// Channel to send read_len on completion
    pub done_tx: DoneSender,
}

/// Tracks in-flight io_uring requests
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
struct InFlightRequest {
    /// Shard ID for decryption
    shard_id: usize,
    /// Offset within the block where actual data starts
    off_in_blk: usize,
    /// Original file position for decryption
    file_pos: i64,
    /// Shared buffer to return result (512-byte aligned)
    buffer: Arc<RwLock<AlignedBuf512>>,
    /// Local buffer taken from the shared buffer
    local_buf: AlignedBuf512,
    /// Channel to send read_len on completion
    done_tx: DoneSender,
}

/// PoolGuard automatically returns a buffer to the pool when dropped.
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
pub struct PoolGuard {
    buffer: Option<Arc<RwLock<AlignedBuf512>>>,
    pool: Arc<std::sync::Mutex<Vec<Arc<RwLock<AlignedBuf512>>>>>,
}

#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
impl Deref for PoolGuard {
    type Target = Arc<RwLock<AlignedBuf512>>;
    fn deref(&self) -> &Self::Target {
        self.buffer.as_ref().unwrap()
    }
}

#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
impl Drop for PoolGuard {
    fn drop(&mut self) {
        if let Some(buf) = self.buffer.take() {
            // Only recycle if this is the last reference
            if Arc::strong_count(&buf) == 1 {
                let mut pool = self.pool.lock().unwrap();
                const MAX_POOL_SIZE: usize = 64;
                if pool.len() < MAX_POOL_SIZE {
                    pool.push(buf);
                }
            }
        }
    }
}

/// Handle to the io_uring reader for sending read requests.
/// Supports reading from multiple EntryFiles indexed by shard_id.
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
#[derive(Clone)]
pub struct MultiEntryFileUringReader {
    tx: mpsc::Sender<UringReadRequest>,
    /// Shared buffer pool for reducing allocation overhead
    buffer_pool: Arc<std::sync::Mutex<Vec<Arc<RwLock<AlignedBuf512>>>>>,
    /// Reference to entry files for file handle lookup
    entry_files: Arc<Vec<Arc<EntryFile>>>,
}

/// Receiver half, used to start the io_uring worker thread
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
pub struct MultiEntryFileUringReaderReceiver {
    rx: mpsc::Receiver<UringReadRequest>,
    queue_depth: u32,
}

#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
impl MultiEntryFileUringReaderReceiver {
    /// Starts the io_uring worker thread.
    /// Consumes self and spawns a dedicated thread for io_uring operations.
    ///
    /// # Arguments
    /// * `entry_files` - Vec of EntryFiles for decryption
    pub fn start(self, entry_files: Arc<Vec<Arc<EntryFile>>>) {
        let rx = self.rx;
        let queue_depth = self.queue_depth;
        std::thread::spawn(move || {
            run_uring_loop(entry_files, rx, queue_depth);
        });
    }
}

#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
impl MultiEntryFileUringReader {
    /// Creates a new MultiEntryFileUringReader and its receiver.
    /// Call `receiver.start(entry_files)` to spawn the io_uring worker thread.
    ///
    /// # Arguments
    /// * `entry_files` - Vec of EntryFiles indexed by shard_id
    /// * `queue_depth` - io_uring queue depth (number of concurrent operations)
    /// * `channel_size` - Size of the mpsc channel for pending requests
    pub fn new(
        entry_files: Vec<Arc<EntryFile>>,
        queue_depth: u32,
        channel_size: usize,
    ) -> (Self, MultiEntryFileUringReaderReceiver) {
        let (tx, rx) = mpsc::channel::<UringReadRequest>(channel_size);
        let entry_files = Arc::new(entry_files);
        (
            Self {
                tx,
                buffer_pool: Arc::new(std::sync::Mutex::new(Vec::new())),
                entry_files,
            },
            MultiEntryFileUringReaderReceiver { rx, queue_depth },
        )
    }

    /// Returns the number of shards (entry files).
    pub fn shard_count(&self) -> usize {
        self.entry_files.len()
    }

    /// Returns reference to entry files.
    pub fn entry_files(&self) -> &Arc<Vec<Arc<EntryFile>>> {
        &self.entry_files
    }

    /// Submits a read request and waits for completion (async version for tokio tasks).
    ///
    /// # Returns
    /// - `Ok(0)`: Error (I/O failure or insufficient bytes for header)
    ///   NOTE: this also includes "outdated" reads racing with pruning.
    ///   If the underlying segment was pruned (and its FD closed/reused) while an io_uring
    ///   read was in-flight, the read may fail or yield garbage. This API intentionally
    ///   surfaces that as `Ok(0)` so callers can fall back / retry / ignore.
    /// - `Ok(read_len)` where `read_len > 0`: Data available at `buffer[0..read_len]`.
    ///   Parse `entry_len` from header (first 5 bytes), then:
    ///   - If `read_len >= entry_len`: Complete entry, **decrypted**
    ///   - If `read_len < entry_len`: Incomplete, only header is usable (unencrypted).
    ///     Retry with a buffer of at least `entry_len` bytes.
    ///
    /// # Arguments
    /// * `shard_id` - Index of the EntryFile to read from
    /// * `file_pos` - Position in the logical file
    /// * `buffer` - Shared buffer for the result (must be 512-byte aligned)
    /// * `expected_len` - Expected read length (must be a multiple of IO_BLK_SIZE)
    ///
    /// # Panics
    /// Panics if `expected_len` is not a multiple of IO_BLK_SIZE or if `shard_id` is out of bounds.
    pub async fn read(
        &self,
        shard_id: usize,
        file_pos: i64,
        buffer: Arc<RwLock<AlignedBuf512>>,
        expected_len: usize,
    ) -> Result<usize, mpsc::error::SendError<UringReadRequest>> {
        assert!(
            expected_len % IO_BLK_SIZE == 0,
            "expected_len ({expected_len}) must be a multiple of IO_BLK_SIZE ({IO_BLK_SIZE})"
        );
        assert!(
            shard_id < self.entry_files.len(),
            "shard_id ({shard_id}) out of bounds ({})",
            self.entry_files.len()
        );

        let entry_file = &self.entry_files[shard_id];
        // Fast-path: outdated read (already pruned). Treat as I/O failure.
        if file_pos < entry_file.pruned_before() {
            return Ok(0);
        }

        // Get file and extract fd
        //
        // NOTE: We only pass RawFd to io_uring. The `file_guard` is dropped after this
        // function returns (or awaits), while the io_uring operation may still be in-flight.
        // This is an intentional design trade-off to avoid the overhead of passing the guard.
        //
        // Safety reasoning:
        // The post-completion check in `run_uring_loop` compares the original `req.file_pos`
        // against the current `pruned_before` watermark. This catches ALL stale reads:
        //
        // - fd closed (not reused): io_uring returns EBADF → detected, returns 0 ✓
        // - fd reused by another file: This implies the original segment was pruned,
        //   which means `pruned_before` was updated BEFORE the fd was closed/reused.
        //   The post-completion check `file_pos < pruned_before` will catch this → returns 0 ✓
        // - Read too short: length validation fails → returns 0 ✓
        //
        // Key invariant: `pruned_before` must be updated atomically BEFORE closing the fd.
        // As long as this invariant holds, no stale/corrupt data can escape to the caller.
        //
        // We use try_get_file_ref to gracefully handle the race where pruning happens
        // between our pruned_before check and the actual file access.
        let Some((file_guard, offset)) = entry_file.try_get_file_ref(file_pos) else {
            return Ok(0); // File segment was pruned, treat as read failure
        };
        let Some(file) = file_guard.as_ref() else {
            return Ok(0); // File not available
        };
        let fd = file.as_raw_fd();

        // Calculate block-aligned offset for O_DIRECT compatibility
        let off_in_blk = (offset % IO_BLK_SIZE as i64) as usize;
        let aligned_offset = offset - off_in_blk as i64;
        // Adjust expected_len to cover from aligned boundary
        let aligned_len =
            ((off_in_blk + expected_len + IO_BLK_SIZE - 1) / IO_BLK_SIZE) * IO_BLK_SIZE;

        let (done_tx, done_rx) = oneshot::channel();
        let req = UringReadRequest {
            shard_id,
            fd,
            aligned_offset,
            off_in_blk,
            file_pos,
            buffer,
            expected_len: aligned_len,
            done_tx: DoneSender::Async(done_tx),
        };

        self.tx.send(req).await?;
        Ok(done_rx.await.unwrap_or(0))
    }

    /// Submits a read request and blocks until completion (sync version for regular threads).
    ///
    /// # Returns
    /// - `Ok(0)`: Error (I/O failure or insufficient bytes for header)
    ///   NOTE: this also includes "outdated" reads racing with pruning.
    ///   If the underlying segment was pruned (and its FD closed/reused) while an io_uring
    ///   read was in-flight, the read may fail or yield garbage. This API intentionally
    ///   surfaces that as `Ok(0)` so callers can fall back / retry / ignore.
    /// - `Ok(read_len)` where `read_len > 0`: Data available. Check `read_len >= entry_len`
    ///   (from header) to determine if complete and decrypted.
    ///
    /// # Arguments
    /// * `shard_id` - Index of the EntryFile to read from
    /// * `file_pos` - Position in the logical file
    /// * `buffer` - Shared buffer for the result (must be 512-byte aligned)
    /// * `expected_len` - Expected read length (must be a multiple of IO_BLK_SIZE)
    ///
    /// # Panics
    /// Panics if `expected_len` is not a multiple of IO_BLK_SIZE or if `shard_id` is out of bounds.
    pub fn read_blocking(
        &self,
        shard_id: usize,
        file_pos: i64,
        buffer: Arc<RwLock<AlignedBuf512>>,
        expected_len: usize,
    ) -> Result<usize, mpsc::error::SendError<UringReadRequest>> {
        assert!(
            expected_len % IO_BLK_SIZE == 0,
            "expected_len ({expected_len}) must be a multiple of IO_BLK_SIZE ({IO_BLK_SIZE})"
        );
        assert!(
            shard_id < self.entry_files.len(),
            "shard_id ({shard_id}) out of bounds ({})",
            self.entry_files.len()
        );

        let entry_file = &self.entry_files[shard_id];
        // Fast-path: outdated read (already pruned). Treat as I/O failure.
        if file_pos < entry_file.pruned_before() {
            return Ok(0);
        }

        // See detailed fd lifetime notes in `read()` method.
        let Some((file_guard, offset)) = entry_file.try_get_file_ref(file_pos) else {
            return Ok(0); // File segment was pruned
        };
        let Some(file) = file_guard.as_ref() else {
            return Ok(0); // File not available
        };
        let fd = file.as_raw_fd();

        // Calculate block-aligned offset for O_DIRECT compatibility
        let off_in_blk = (offset % IO_BLK_SIZE as i64) as usize;
        let aligned_offset = offset - off_in_blk as i64;
        let aligned_len =
            ((off_in_blk + expected_len + IO_BLK_SIZE - 1) / IO_BLK_SIZE) * IO_BLK_SIZE;

        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let req = UringReadRequest {
            shard_id,
            fd,
            aligned_offset,
            off_in_blk,
            file_pos,
            buffer,
            expected_len: aligned_len,
            done_tx: DoneSender::Sync(done_tx),
        };

        self.tx.blocking_send(req)?;
        Ok(done_rx.recv().unwrap_or(0))
    }

    /// Try to submit a read request without blocking (async version).
    /// Returns a DoneReceiver if successful, None if the channel is full.
    ///
    /// # Returns
    /// - `Some(receiver)`: Request submitted. Call `receiver.recv_async()` to get `read_len`.
    /// - `None`: Channel full or shard_id out of bounds, try again later.
    ///
    /// # Arguments
    /// * `shard_id` - Index of the EntryFile to read from
    /// * `file_pos` - Position in the logical file
    /// * `buffer` - Shared buffer for the result (must be 512-byte aligned)
    /// * `expected_len` - Expected read length (must be a multiple of IO_BLK_SIZE)
    pub fn try_read(
        &self,
        shard_id: usize,
        file_pos: i64,
        buffer: Arc<RwLock<AlignedBuf512>>,
        expected_len: usize,
    ) -> Option<DoneReceiver> {
        if expected_len % IO_BLK_SIZE != 0 || shard_id >= self.entry_files.len() {
            return None;
        }

        let entry_file = &self.entry_files[shard_id];
        // Fast-path: outdated read (already pruned).
        if file_pos < entry_file.pruned_before() {
            return None;
        }

        // See detailed fd lifetime notes in `read()` method.
        let (file_guard, offset) = entry_file.try_get_file_ref(file_pos)?;
        let file = file_guard.as_ref()?;
        let fd = file.as_raw_fd();

        let off_in_blk = (offset % IO_BLK_SIZE as i64) as usize;
        let aligned_offset = offset - off_in_blk as i64;
        let aligned_len =
            ((off_in_blk + expected_len + IO_BLK_SIZE - 1) / IO_BLK_SIZE) * IO_BLK_SIZE;

        let (done_tx, done_rx) = oneshot::channel();
        let req = UringReadRequest {
            shard_id,
            fd,
            aligned_offset,
            off_in_blk,
            file_pos,
            buffer,
            expected_len: aligned_len,
            done_tx: DoneSender::Async(done_tx),
        };

        match self.tx.try_send(req) {
            Ok(()) => Some(DoneReceiver::Async(done_rx)),
            Err(_) => None,
        }
    }

    /// Try to submit a read request without blocking (sync version for regular threads).
    /// Returns a DoneReceiver if successful, None if the channel is full.
    ///
    /// # Returns
    /// - `Some(receiver)`: Request submitted. Call `receiver.recv_blocking()` to get `read_len`.
    /// - `None`: Channel full or shard_id out of bounds, try again later.
    ///
    /// # Arguments
    /// * `shard_id` - Index of the EntryFile to read from
    /// * `file_pos` - Position in the logical file
    /// * `buffer` - Shared buffer for the result (must be 512-byte aligned)
    /// * `expected_len` - Expected read length (must be a multiple of IO_BLK_SIZE)
    pub fn try_read_blocking(
        &self,
        shard_id: usize,
        file_pos: i64,
        buffer: Arc<RwLock<AlignedBuf512>>,
        expected_len: usize,
    ) -> Option<DoneReceiver> {
        if expected_len % IO_BLK_SIZE != 0 || shard_id >= self.entry_files.len() {
            return None;
        }

        let entry_file = &self.entry_files[shard_id];
        // Fast-path: outdated read (already pruned).
        if file_pos < entry_file.pruned_before() {
            return None;
        }

        // See detailed fd lifetime notes in `read()` method.
        let (file_guard, offset) = entry_file.try_get_file_ref(file_pos)?;
        let file = file_guard.as_ref()?;
        let fd = file.as_raw_fd();

        let off_in_blk = (offset % IO_BLK_SIZE as i64) as usize;
        let aligned_offset = offset - off_in_blk as i64;
        let aligned_len =
            ((off_in_blk + expected_len + IO_BLK_SIZE - 1) / IO_BLK_SIZE) * IO_BLK_SIZE;

        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let req = UringReadRequest {
            shard_id,
            fd,
            aligned_offset,
            off_in_blk,
            file_pos,
            buffer,
            expected_len: aligned_len,
            done_tx: DoneSender::Sync(done_tx),
        };

        match self.tx.try_send(req) {
            Ok(()) => Some(DoneReceiver::Sync(done_rx)),
            Err(_) => None,
        }
    }

    /// Allocates a NEW aligned buffer (not from pool).
    ///
    /// # Arguments
    /// * `size` - Size of the buffer in bytes
    ///
    /// # Note
    /// For high-frequency reads, prefer `get_buffer()` which uses a PoolGuard to
    /// automatically reuse buffers. This method always allocates fresh memory.
    pub fn alloc_buffer(size: usize) -> Arc<RwLock<AlignedBuf512>> {
        Arc::new(RwLock::new(AlignedBuf512::new_zeroed(size)))
    }

    /// Gets a buffer from the pool, or allocates a new one if pool is empty.
    ///
    /// This is more efficient than `alloc_buffer()` for high-frequency reads
    /// because it reuses previously allocated buffers.
    ///
    /// # Arguments
    /// * `min_size` - Minimum size for newly allocated buffers. Pooled buffers may be
    ///   smaller but will be resized by the io_uring worker if needed.
    ///
    /// # Returns
    /// A `PoolGuard` that automatically returns the buffer to the pool on drop.
    pub fn get_buffer(&self, min_size: usize) -> PoolGuard {
        let mut pool = self.buffer_pool.lock().unwrap();
        let buf = if let Some(buf) = pool.pop() {
            buf
        } else {
            drop(pool);
            let b = Arc::new(RwLock::new(AlignedBuf512::new_zeroed(min_size)));
            return PoolGuard {
                buffer: Some(b),
                pool: self.buffer_pool.clone(),
            };
        };

        PoolGuard {
            buffer: Some(buf),
            pool: self.buffer_pool.clone(),
        }
    }

    /// Clears the buffer pool, freeing all cached buffers.
    pub fn clear_pool(&self) {
        let mut pool = self.buffer_pool.lock().unwrap();
        pool.clear();
    }

    /// High-level convenience method: reads a complete entry into ReadBuf.
    /// Automatically handles two-read case when entry is larger than initial buffer.
    ///
    /// # Returns
    /// * `true` - Read successful
    /// * `false` - Read failed, caller should use fallback method
    ///
    /// # Arguments
    /// * `shard_id` - Index of the EntryFile to read from
    /// * `file_pos` - Position in the logical file
    /// * `buf` - ReadBuf to store the result
    pub fn read_entry(&self, shard_id: usize, file_pos: i64, buf: &mut ReadBuf) -> bool {
        if shard_id >= self.entry_files.len() {
            return false;
        }
        if file_pos < self.entry_files[shard_id].pruned_before() {
            return false;
        }

        // First read: use 2 * IO_BLK_SIZE to handle entries that may cross block boundary
        let initial_size = 2 * IO_BLK_SIZE;
        let aligned_buf = self.get_buffer(initial_size);
        let first_read_len =
            match self.read_blocking(shard_id, file_pos, (*aligned_buf).clone(), initial_size) {
                Ok(len) => len,
                Err(_) => return false,
            };

        if first_read_len == 0 {
            return false;
        }

        // Parse entry length from header
        let guard = aligned_buf.blocking_read();
        if first_read_len < 5 {
            return false;
        }
        let entry_len = EntryBz::get_entry_len(&guard[..5]);

        // Check if entry is complete
        if first_read_len >= entry_len {
            buf.resize(entry_len);
            buf.as_mut_slice()[..entry_len].copy_from_slice(&guard[..entry_len]);
            return true;
        }

        // Entry is larger - need a second read
        drop(guard);
        drop(aligned_buf);

        let aligned_size = entry_len.div_ceil(IO_BLK_SIZE) * IO_BLK_SIZE;
        let larger_buf = self.get_buffer(aligned_size);

        let second_read_len =
            match self.read_blocking(shard_id, file_pos, (*larger_buf).clone(), aligned_size) {
                Ok(len) => len,
                Err(_) => return false,
            };

        if second_read_len < entry_len {
            return false;
        }

        let guard2 = larger_buf.blocking_read();
        buf.resize(entry_len);
        buf.as_mut_slice()[..entry_len].copy_from_slice(&guard2[..entry_len]);

        true
    }
}

/// Drains all pending requests from the channel and notifies callers with failure (0).
/// This must be called before exiting the uring loop to avoid leaving callers blocked forever.
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
fn drain_channel(rx: &mut mpsc::Receiver<UringReadRequest>) {
    while let Ok(req) = rx.try_recv() {
        let _ = req.done_tx.send(0);
    }
}

/// Main loop for the io_uring worker thread.
/// Receives requests via mpsc channel, submits them to io_uring,
/// and notifies callers when complete.
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
fn run_uring_loop(
    entry_files: Arc<Vec<Arc<EntryFile>>>,
    mut rx: mpsc::Receiver<UringReadRequest>,
    queue_depth: u32,
) {
    let mut uring = match io_uring::IoUring::new(queue_depth) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("Failed to create io_uring: {e}");
            // Drain channel to notify all waiting callers before exiting
            drain_channel(&mut rx);
            return;
        }
    };

    let qd = queue_depth as usize;
    let mut in_flight: Vec<Option<InFlightRequest>> = (0..qd).map(|_| None).collect();
    let mut free_list: Vec<usize> = (0..qd).collect();
    let mut channel_closed = false;
    let mut completions: Vec<(u64, i32)> = Vec::with_capacity(qd);

    loop {
        // 1. As long as we have free slots, try to receive and submit requests
        while let Some(slot) = free_list.pop() {
            match rx.try_recv() {
                Ok(req) => {
                    if !submit_request(&mut uring, &mut in_flight, slot, req) {
                        free_list.push(slot);
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    free_list.push(slot);
                    break;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    free_list.push(slot);
                    channel_closed = true;
                    break;
                }
            }
        }

        let in_flight_count = qd - free_list.len();

        if channel_closed && in_flight_count == 0 {
            return;
        }

        // 2. If we have in-flight requests, submit and wait for at least one completion
        if in_flight_count > 0 {
            if let Err(e) = uring.submit_and_wait(1) {
                eprintln!("io_uring submit_and_wait failed: {e}");
                // Clean up in-flight requests
                for (slot, opt) in in_flight.iter_mut().enumerate() {
                    if let Some(req_info) = opt.take() {
                        let mut guard = req_info.buffer.blocking_write();
                        *guard = req_info.local_buf;
                        drop(guard);
                        let _ = req_info.done_tx.send(0);
                        free_list.push(slot);
                    }
                }
                // Drain channel to notify all waiting callers before exiting
                drain_channel(&mut rx);
                return;
            }

            // 3. Process completed requests
            completions.clear();
            completions.extend(
                uring
                    .completion()
                    .map(|cqe| (cqe.user_data(), cqe.result())),
            );

            for &(user_data, result) in &completions {
                let slot = user_data as usize;
                if slot >= qd {
                    continue;
                }

                if let Some(mut req_info) = in_flight[slot].take() {
                    let off_in_blk = req_info.off_in_blk;
                    // Drop outdated reads before parsing header / decrypting.
                    let pruned_before = entry_files
                        .get(req_info.shard_id)
                        .map(|ef| ef.pruned_before())
                        .unwrap_or(0);

                    let read_len = if req_info.file_pos < pruned_before {
                        0
                    } else if result > 0 {
                        let len = result as usize;
                        if len < off_in_blk + 5 {
                            eprintln!(
                                "io_uring read too short: got {} bytes, need at least {}",
                                len,
                                off_in_blk + 5
                            );
                            0
                        } else {
                            let header_start = off_in_blk;
                            let entry_len = EntryBz::get_entry_len(
                                &req_info.local_buf[header_start..header_start + 5],
                            );
                            let data_end = off_in_blk + entry_len;
                            let available = len - off_in_blk;

                            if data_end > len {
                                // Incomplete - copy available bytes to buffer start
                                req_info.local_buf.copy_within(off_in_blk..len, 0);
                                available
                            } else {
                                // Complete - decrypt and move to buffer start
                                if req_info.shard_id < entry_files.len() {
                                    entry_files[req_info.shard_id].decrypt_entry_in_place(
                                        req_info.file_pos,
                                        &mut req_info.local_buf[off_in_blk..data_end],
                                    );
                                }
                                req_info.local_buf.copy_within(off_in_blk..data_end, 0);
                                entry_len
                            }
                        }
                    } else {
                        if result < 0 {
                            eprintln!(
                                "io_uring read failed: {}",
                                std::io::Error::from_raw_os_error(-result)
                            );
                        }
                        0
                    };

                    // Swap buffer back
                    {
                        let mut guard = req_info.buffer.blocking_write();
                        mem::swap(&mut *guard, &mut req_info.local_buf);
                    }

                    let _ = req_info.done_tx.send(read_len);
                    free_list.push(slot);
                }
            }
        } else {
            // No in-flight requests, block waiting for new request
            match rx.blocking_recv() {
                Some(req) => {
                    if let Some(slot) = free_list.pop() {
                        if !submit_request(&mut uring, &mut in_flight, slot, req) {
                            free_list.push(slot);
                        }
                    }
                }
                None => {
                    return;
                }
            }
        }
    }
}

/// Helper function to submit a single request to io_uring.
#[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
fn submit_request(
    uring: &mut io_uring::IoUring,
    in_flight: &mut [Option<InFlightRequest>],
    slot: usize,
    req: UringReadRequest,
) -> bool {
    // Take the buffer from shared storage
    let mut local_buf = {
        let mut guard = req.buffer.blocking_write();
        mem::take(&mut *guard)
    };

    // Ensure buffer is large enough
    if local_buf.allocated_size() < req.expected_len {
        local_buf.resize(req.expected_len);
    }

    let fd = req.fd;

    let read_e = io_uring::opcode::Read::new(
        io_uring::types::Fd(fd),
        local_buf.as_mut_ptr(),
        local_buf.allocated_size() as u32,
    )
    .offset(req.aligned_offset as u64)
    .build()
    .user_data(slot as u64);

    unsafe {
        if let Err(e) = uring.submission().push(&read_e) {
            eprintln!("Failed to push to submission queue: {e}");
            let mut guard = req.buffer.blocking_write();
            *guard = local_buf;
            let _ = req.done_tx.send(0);
            return false;
        }
    }

    in_flight[slot] = Some(InFlightRequest {
        shard_id: req.shard_id,
        off_in_blk: req.off_in_blk,
        file_pos: req.file_pos,
        buffer: req.buffer,
        local_buf,
        done_tx: req.done_tx,
    });
    true
}

#[cfg(all(test, target_os = "linux", not(feature = "disable_dio")))]
mod tests {
    use super::*;
    use crate::entryfile::EntryFileWriter;
    use qmdb_common::def::TAG_SIZE;
    use qmdb_common::entryfile::entry::{entry_to_bytes, Entry};

    const ALIGNED_READ_SIZE: usize = IO_BLK_SIZE;

    fn create_test_entry_file(dir_path: &str) -> (Arc<EntryFile>, Vec<i64>, Vec<Vec<u8>>) {
        static KEY0: &[u8] = b"Key0Key0Key0Key0Key0Key0Key0Key0Key0";
        static VAL0: &[u8] = b"Value0Value0Value0Value0Value0Value0";
        static KEY1: &[u8] = b"Key1Key ILOVEYOU 1Key1Key1";
        static VAL1: &[u8] = b"Value1Value1";
        static HASH: [u8; 32] = [0u8; 32];

        let entries = [
            Entry {
                key: KEY0,
                value: VAL0,
                next_key_hash: &HASH,
                version: 0,
                serial_number: 0,
            },
            Entry {
                key: KEY1,
                value: VAL1,
                next_key_hash: &HASH,
                version: 10,
                serial_number: 1,
            },
        ];

        let ef = EntryFile::new(8 * 1024, 128 * 1024, dir_path.to_string(), None);
        let ef = Arc::new(ef);
        let mut writer = EntryFileWriter::new(ef.clone(), 8 * 1024);

        let mut positions = Vec::new();
        let mut entry_bytes = Vec::new();

        for entry in &entries {
            let mut total_len =
                (qmdb_common::def::ENTRY_FIXED_LENGTH + entry.key.len() + entry.value.len())
                    .div_ceil(8)
                    * 8;
            if cfg!(feature = "tee_cipher") {
                total_len += TAG_SIZE;
            }
            let mut bz = vec![0; total_len];
            entry_to_bytes(entry, &[], &mut bz);

            let pos = writer.append(&EntryBz { bz: &bz }).unwrap();
            positions.push(pos);
            entry_bytes.push(bz);
        }

        writer.flush().unwrap();
        (ef, positions, entry_bytes)
    }

    #[tokio::test]
    async fn test_multi_entry_file_uring_reader_basic() {
        let temp_dir = tempfile::Builder::new()
            .prefix("multi_uring_test")
            .tempdir()
            .unwrap();
        let dir_path = temp_dir.path().to_str().unwrap();

        // Create two entry files (simulating 2 shards)
        let (ef0, positions0, expected0) = create_test_entry_file(&format!("{}/shard0", dir_path));
        let (ef1, positions1, expected1) = create_test_entry_file(&format!("{}/shard1", dir_path));

        let entry_files = vec![ef0.clone(), ef1.clone()];

        // Create MultiEntryFileUringReader
        let (reader, receiver) = MultiEntryFileUringReader::new(entry_files.clone(), 32, 256);
        let entry_files_arc = reader.entry_files().clone();
        receiver.start(entry_files_arc);

        // Test reading from shard 0
        for (i, &pos) in positions0.iter().enumerate() {
            let buffer = MultiEntryFileUringReader::alloc_buffer(ALIGNED_READ_SIZE);
            let read_len = reader
                .read(0, pos, buffer.clone(), ALIGNED_READ_SIZE)
                .await
                .expect("read should succeed");

            assert!(read_len > 0, "Read length should be > 0");

            let result = buffer.read().await;
            let entry_bz = EntryBz {
                bz: &result.as_slice()[..read_len],
            };
            let expected_entry = EntryBz {
                bz: &expected0[i],
            };

            assert_eq!(entry_bz.key(), expected_entry.key());
            assert_eq!(entry_bz.value(), expected_entry.value());
        }

        // Test reading from shard 1
        for (i, &pos) in positions1.iter().enumerate() {
            let buffer = MultiEntryFileUringReader::alloc_buffer(ALIGNED_READ_SIZE);
            let read_len = reader
                .read(1, pos, buffer.clone(), ALIGNED_READ_SIZE)
                .await
                .expect("read should succeed");

            assert!(read_len > 0, "Read length should be > 0");

            let result = buffer.read().await;
            let entry_bz = EntryBz {
                bz: &result.as_slice()[..read_len],
            };
            let expected_entry = EntryBz {
                bz: &expected1[i],
            };

            assert_eq!(entry_bz.key(), expected_entry.key());
            assert_eq!(entry_bz.value(), expected_entry.value());
        }

        ef0.close();
        ef1.close();
    }

    #[test]
    fn test_read_entry_convenience_method() {
        let temp_dir = tempfile::Builder::new()
            .prefix("read_entry_test")
            .tempdir()
            .unwrap();
        let dir_path = temp_dir.path().to_str().unwrap();

        let (ef, positions, expected) = create_test_entry_file(dir_path);
        let entry_files = vec![ef.clone()];

        let (reader, receiver) = MultiEntryFileUringReader::new(entry_files.clone(), 32, 256);
        let entry_files_arc = reader.entry_files().clone();
        receiver.start(entry_files_arc);

        for (i, &pos) in positions.iter().enumerate() {
            let mut buf = ReadBuf::new();
            let success = reader.read_entry(0, pos, &mut buf);

            assert!(success, "read_entry should succeed");

            let entry_bz = EntryBz { bz: buf.as_slice() };
            let expected_entry = EntryBz { bz: &expected[i] };

            assert_eq!(entry_bz.key(), expected_entry.key());
            assert_eq!(entry_bz.value(), expected_entry.value());
        }

        ef.close();
    }
}

