# HPFile: Head-Prunable File

`HPFile` (Head-Prunable File) is a high-performance, append-only storage engine designed for scenarios requiring extreme concurrent read performance and the ability to reclaim space from the beginning of the file. It is a core component of [QMDB](../doc/Explain-QMDB.md) and is also used in `rpcdb`.

## Why HPFile?

Standard filesystems do not support efficient pruning from the head. For systems like QMDB that use a Write-Ahead Log (WAL) and perform periodic compaction, old data at the beginning of the file becomes redundant. `HPFile` solves this by:

1.  **Segmented Storage**: Splitting a large logical file into multiple physical "segments" (small files).
2.  **Head Pruning**: Reclaiming space by simply deleting the oldest segment files.
3.  **Zero-Lock Concurrency**: Implementing a custom lock-free slot system to handle high-concurrency reads without CPU cacheline contention.
4.  **Direct I/O**: Bypassing the OS Page Cache to eliminate kernel-level lock contention and ensure deterministic performance.

## Core Architecture

### 1. Segmented Layout
An `HPFile` is stored in a directory. Each segment is named as `{FileID}-{SegmentSize}`.
- `FileID`: A monotonically increasing integer starting from 0.
- `SegmentSize`: A fixed size (e.g., 1GB) defined at creation.

The logical offset in the virtual big file is calculated as:
`LogicalOffset = FileID * SegmentSize + OffsetInSegment`

### 2. High-Performance Reading (O_DIRECT)
On Linux, `HPFile` opens all segments with the `O_DIRECT` flag.
- **Why?** OS Page Cache can become a bottleneck under high-concurrency random reads due to internal locks and cacheline "ping-pong" (multiple cores fighting over the same reference counts).
- **Alignment**: `O_DIRECT` requires I/O to be aligned to the filesystem block size (typically 512 bytes). `HPFile` provides `AlignedBuf512` and handles the necessary padding/alignment logic internally, allowing users to read from any logical offset.
- **Limitation**: Since reads use `O_DIRECT`, data must be flushed to disk and aligned to 512 bytes to be readable. If `flush()` is called with a buffer whose length is not aligned to 512 bytes, the trailing data in the last block may not be readable via `O_DIRECT` until more data is appended and flushed, or a segment rotation occurs (which automatically pads the tail to 512 bytes).

### 3. Lock-Free File Slots
To map `FileID` to an open file handle without using heavy-weight locks (like `RwLock`), `HPFile` uses a custom `FileSlots` implementation based on `ArcSwap` and Epoch-based Reclamation.
- **No Refcount Contention**: Standard `Arc<File>` cloning involves atomic increments that cause cacheline invalidation across cores. `FileSlots` uses `ArcSwap::load()` which returns a guard without touching the reference count in the fast path.
- **O(1) Access**: Uses a ring-buffer-like array for near-instant lookup.
- **Lock-Free Resize**: When the number of segments exceeds the current slot capacity, the array grows in the background without blocking readers.
- **Concurrency Note**: While reads are lock-free, certain administrative operations like `truncate` are not safe to run concurrently with reads. `FileSlots` is specifically optimized for `HPFile`'s single-writer, multi-reader scenario and is not intended as a general-purpose concurrent map.

### 4. Hybrid Writing Strategy
While reads use `O_DIRECT`, writes to the *latest* segment use standard buffered I/O (non-`O_DIRECT`).
- **Buffer Management**: The caller provides a `Vec<u8>` as a write buffer. `HPFile` manages the logic of when to flush this buffer to disk, but the caller maintains ownership of the memory. `HPFile` itself does not hold or persist this buffer.
- **Append Size Constraint**: Each `append` call must provide a byteslice `bz` where `bz.len() <= wr_buf_size`. Since `segment_size` is required to be a multiple of `wr_buf_size`, this also ensures that a single append cannot span more than two segments.
- **Stability**: By maintaining two handles for the latest segment (one `O_DIRECT` for readers, one buffered for the writer), file descriptors remain stable even during segment rotation.
- **Segment Overflow**: To maintain a simple mapping between logical offsets and segments, `HPFile` allows the physical file of a segment to slightly exceed `segment_size` if a single `append` crosses the boundary. The overflow bytes are also duplicated into the next segment at offset 0 so the watcher stream can read them from the new segment. This implies that normal users should only read from offsets returned by `append` (which still point into the old segment).

## Key Operations

### Append & Flush
- `append`: Appends data to a write buffer. If the buffer exceeds `wr_buf_size`, it is flushed to the current segment.
- `flush`: Forces the write buffer to disk and performs an `fsync`.
- **Read-Your-Writes**: Since `HPFile` doesn't write to disk immediately, the upper layer (e.g., QMDB) is responsible for reading from its own write buffer if the data hasn't been flushed yet.
- **Crash/Restart Consistency**: HPFile relies on the upper layer to provide block-level atomicity. If a crash happens after a partial block write, the upper layer must truncate the tail garbage on restart to restore a consistent state. **Important**: After reopening an HPFile, you must always call `truncate(correct_size)` to restore the correct logical size, because segment rotation may have added padding bytes that inflate the on-disk file size.

### Prune & Truncate
- `prune_head(offset)`: Deletes all segment files that are entirely before the given `offset`. This is used after compaction to reclaim space.
- `truncate(size)`: Discards data from the end of the file.
  - **Primary Purpose**: It is a tool for achieving block-level atomicity during crash recovery. If a crash occurs, the upper layer (e.g., QMDB) uses `truncate` to remove trailing garbage or partial writes.
  - **Strict Usage Timing**: `truncate` is intended to be used **immediately after `HPFile::new`** and **before any read operations begin**.
  - **Precondition**: `truncate` can only operate on segments that have not been pruned. The target `size` must fall within the range of existing (non-pruned) segments.
  - **Note**: `prune_head` does not shrink the logical size; it only makes the head region unreadable while reclaiming disk space.

### Streaming (Watcher)
`HPFile` includes a `FileWatcher` (in `watcher.rs`) that uses OS filesystem events (`notify`) to stream newly appended data to other processes or threads.
- **Loose Coupling**: Allows components like `rpcdb` to follow the `HPFile` updates without direct function calls or shared memory.
- **Fault Tolerance**: Since data is persisted in files, the consumer can crash and resume without losing any streamed information.
- **Cross-Segment Handling**: The watcher treats bytes beyond `segment_size` in an old segment as invalid and will not stream them. It expects any overflow bytes to be re-emitted from the next segment starting at offset 0. This is specific to the watcher stream; normal reads still require offsets returned by `append`.

## Use Cases

1.  **QMDB Shard Storage**: Storing the immutable entries of a Merkle Tree shard. Compaction moves active entries to the tail, and `prune_head` removes the old head.
2.  **Stateless Execution Witness**: `PreReader` is used to sequentially scan segments to construct witnesses for ZKVMs.
3.  **Cross-Process Sync**: Using the `Watcher` to build a real-time replica of the state in a separate process.

## Implementation Notes

- **Directory Must Be Clean**: `HPFile::new` validates filenames in the directory and currently expects files to match the `{FileID}-{segment_size}` naming scheme. Extra unrelated files in the directory may cause initialization to fail.
- **Segment Size is Permanent**: Once a directory is initialized with a specific `segment_size`, it cannot be changed.
- **`segment_size` Must Be Divisible by `wr_buf_size`**: Creation requires `segment_size % wr_buf_size == 0`.
- **Single Writer, Multi-Reader**: `HPFile` supports only one writer thread but an unlimited number of concurrent reader threads.
- **RoHPFile**: A specialized read-only version (`RoHPFile`) is available for processes that only need to query existing segments. Note: the current implementation uses lazy open + `DashMap` (not the same lock-free `FileSlots` design), and does not use `O_DIRECT`.

## Limitations / Gotchas (Read This Before Using)

### 1. Read Semantics Are Offset-Based, Not Record-Based
- **Only read from offsets returned by `append`**: `read_at(offset)` is intended to read from the *start* of a previously appended byteslice. Reading from the middle of a record, or "manually computing offsets", is explicitly unsupported and can return unexpected bytes (especially around segment boundaries).
- **Sequential scanning is equivalent**: If each record contains its own length field (and padding is deterministic), you can compute `next_offset = current_offset + record_length + padding`. This is mathematically equivalent to using offsets returned by `append`, because that's exactly how HPFile calculates the next append position internally. Systems like `rpcdb` use this pattern to sequentially scan all records without storing every individual offset. The key constraint is: always start reading from the **beginning** of a record, never from the middle.
- **Cross-segment append duplicates overflow**: If an `append` crosses the segment boundary, the current segment file may temporarily exceed `segment_size`, and the overflow bytes are duplicated into the next segment starting at offset 0 for the watcher stream. Normal reads still require offsets returned by `append` (which read from the old segment).
- **Watcher is a special case**: The watcher stream is byte-oriented and may read at non-append offsets when gap-filling. To avoid ambiguity, it ignores any bytes beyond `segment_size` in an old segment and expects those overflow bytes to appear at the start of the next segment. This rule applies only to the watcher stream; normal reads still rely on append-returned offsets (and thus read from the old segment).

### 2. O_DIRECT Tail Alignment and How HPFile Mitigates It for the Latest Segment
- On Linux, historical segments are read using `O_DIRECT`, which requires alignment of **offset / length / buffer address** (typically 512 bytes).
- A non-512-aligned on-disk tail (common after `flush()` because it does **not** pad EOF) can cause **short reads** near EOF when using `O_DIRECT`.
- **Mitigation in current implementation**: `HPFile::read_at` and `HPFile::read_at_with_pre_reader` will read from the **latest segment** using the buffered (non-`O_DIRECT`) writable file handle when available. This avoids `O_DIRECT` tail-alignment short reads for the newest segment after `flush()`.
- **Still important**:
  - If you bypass `HPFile::read_at` and use raw fds from `get_file_ref()` / io_uring, you are reading the `O_DIRECT` handle and must still handle tail short reads on the latest segment (or wait for segment rotation, which pads the old segment tail to 512 bytes).
  - `close()` only clears internal file handles; it does **not** flush the caller-owned write buffer. Any unwritten data in the caller's buffer will be lost if not flushed before the `HPFile` object is closed or dropped.

### 3. Buffered Writes Mean “Read Your Writes” Is the Caller’s Job
- `append` writes into a caller-owned `Vec<u8>` buffer and may not hit disk immediately.
- Readers using `read_at` will only see bytes that have been flushed to disk; upper layers must implement **read-your-writes** by checking their own write buffer when necessary.

### 4. Panics on Misuse (Programmer Errors)
The current API chooses `panic!` for certain invalid usage patterns:
- `append` panics if `bz.len() > wr_buf_size`.
- `read_at_with_pre_reader` panics if `num_bytes >= 512KiB` (internal pre-read buffer limit).
Treat these as invariants enforced by the library; callers should validate inputs.

### 5. Prune/Truncate Concurrency and io_uring Safety
- Reads are lock-free, but **administrative operations are not safe to run concurrently with reads**:
  - `truncate` and `prune_head` remove/rotate segments and can race with readers.
  - **WARNING**: The caller must ensure no concurrent `read_at` or `get_file_ref` calls are made to segments being truncated or pruned. `HPFile` does not provide internal locking for these administrative operations.
- **io_uring users must ensure no in-flight reads on segments being removed**:
  - If a segment file descriptor is closed while an async read is pending, the read can fail or return garbage due to fd reuse.
  - Typical safe sequence: stop submitting reads for pruned segments → wait in-flight completion → call `prune_head`.

### 6. `truncate(size)` for Crash Recovery
- `truncate` operates on segment files on disk and is primarily used during system startup to restore consistency.
- **Initialization Only**: It should only be called after `HPFile::new` and before the file is opened for general reading or appending.
- **Ordering**: The correct sequence for recovery is: `HPFile::new` → `truncate(consistent_size)` → Start Readers/Writers.
- **Recommendation**: if you use `truncate` for recovery, be prepared to handle filesystem-level errors and consider validating/reconstructing missing segment files at the upper layer before calling `truncate`.

#### Truncate and FileWatcher Interaction

If `truncate` is called while a `FileWatcher` is running, the watcher will detect the file size decrease and **re-stream data from offset 0**, causing duplicate chunks to be sent. This is because the watcher's internal logic resets to the beginning when it detects `new_len < cursor`:

```rust
let start = if new_len < cur { 0 } else { cur };
```

**Safe Usage Pattern**: In the normal startup sequence, `truncate` is called **before** `FileWatcher::start()`. When `FileWatcher` starts, it initializes its cursor to the current file size, so only newly appended data will be streamed — no duplicates occur.

**If you must handle duplicates**: Consumers like `rpcdb` use `consume_chunks_interruptible` which filters out duplicate/overlapping chunks by comparing `chunk.pos` against its own processing cursor. If `chunk.pos < cursor`, the chunk is either skipped entirely (full overlap) or trimmed (partial overlap). This provides a safety net, but the recommended approach is to complete all `truncate` operations before starting the watcher.

**Block-Level Atomicity Alternative**: Upper layers (e.g., `ArchivalDB`, `ReceiptDB`) that maintain their own checkpoints (like `SCAN_END`) at block boundaries may not need to call `truncate` at all. As long as the checkpoint always points to a valid, complete block boundary, the bootstrap scan will simply re-parse any trailing garbage or partial data without indexing it (since incomplete blocks are not committed). This design effectively achieves crash recovery at the application level without relying on HPFile's `truncate`.

### 7. Partial Reads: When They Can Still Happen
- `read_at` / `read_at_with_pre_reader` return a `usize` and may legitimately return fewer bytes than requested (e.g., reading past EOF).
- On Linux:
  - For the **latest** segment, HPFile uses the buffered handle in `read_at*`, which greatly reduces tail-alignment-induced partial reads after `flush()`.
  - For **historical** segments, reads use `O_DIRECT`, and callers should still treat the returned `usize` as authoritative.
- **Recommendation**: Always check the returned `usize` from `read_at` and handle partial reads. For “read-your-writes”, continue to read directly from your in-memory write buffer for data not yet flushed to disk.