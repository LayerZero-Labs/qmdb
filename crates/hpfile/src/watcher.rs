use crate::readonly::RoHPFile;
use anyhow::Result;
use crossbeam::channel as cb_channel;
use log::warn;
use notify::{
    event::{CreateKind, ModifyKind, RemoveKind, RenameMode},
    recommended_watcher, Event, EventKind, RecursiveMode, Watcher,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

/// A chunk of bytes newly written to a file under the watched directory
#[derive(Debug, Clone)]
pub struct FileChunk {
    pub pos: u64,
    pub data: Vec<u8>,
}

/// Watches a directory and streams newly appended file bytes via a crossbeam channel.
///
/// Implementation notes:
/// - Uses notify (debounced watcher) to receive FS events.
/// - Maintains per-file cursors and on write events reads the delta [old_len..new_len].
/// - Handles rotation/truncate by detecting length shrinkage and reading from start.
/// - Non-recursive: only files directly in the provided directory are watched.
pub struct FileWatcher {
    stop_tx: cb_channel::Sender<()>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FileWatcher {
    /// Start watching `dir` for newly written data. Returns the watcher handle and a receiver of `FileChunk`s.
    pub fn start<P: AsRef<Path>>(
        dir: P,
        segment_size: u64,
    ) -> Result<(Self, cb_channel::Receiver<FileChunk>)> {
        let dir = dir.as_ref().to_path_buf();

        // Outgoing stream of file chunks
        let (out_tx, out_rx) = cb_channel::unbounded::<FileChunk>();
        // Stop signal for the background thread
        let (stop_tx, stop_rx) = cb_channel::bounded::<()>(1);

        let handle = thread::spawn(move || {
            let (ev_tx, ev_rx) = std::sync::mpsc::channel::<Event>();
            let mut watcher = match recommended_watcher(move |res: notify::Result<Event>| {
                if let Ok(event) = res {
                    let _ = ev_tx.send(event);
                }
            }) {
                Ok(w) => w,
                Err(_) => return,
            };

            if watcher.watch(&dir, RecursiveMode::NonRecursive).is_err() {
                return;
            }

            // Track known files and the last read offset (by length).
            let mut cursors: HashMap<PathBuf, u64> = HashMap::new();

            // Seed existing files with their current lengths so we only stream newly appended bytes after start.
            if let Ok(read_dir) = std::fs::read_dir(&dir) {
                for entry in read_dir.flatten() {
                    let path = entry.path();
                    if path.is_file() {
                        if let Some((_file_id, fnum)) = Self::parse_file_id_and_segment_size(&path) {
                            if fnum == segment_size {
                                if let Ok(meta) = entry.metadata() {
                                    cursors.insert(path, meta.len());
                                }
                            }
                        }
                    }
                }
            }

            // Main loop: interleave stop signal with notify events via timeouts
            loop {
                // Check for stop signal without blocking indefinitely
                if stop_rx.try_recv().is_ok() {
                    break;
                }

                match ev_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(event) => {
                        match event.kind {
                            EventKind::Modify(ModifyKind::Data(_)) => {
                                for path in event.paths {
                                    Self::read_new_bytes(
                                        &path,
                                        segment_size,
                                        &mut cursors,
                                        &out_tx,
                                    );
                                }
                            }
                            EventKind::Create(CreateKind::File)
                            | EventKind::Create(CreateKind::Any) => {
                                for path in event.paths {
                                    cursors.insert(path.clone(), 0);
                                    Self::read_new_bytes(
                                        &path,
                                        segment_size,
                                        &mut cursors,
                                        &out_tx,
                                    );
                                }
                            }
                            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                                if event.paths.len() == 2 {
                                    let src = event.paths[0].clone();
                                    let dst = event.paths[1].clone();
                                    cursors.remove(&src);
                                    cursors.insert(dst.clone(), 0);
                                    Self::read_new_bytes(&dst, segment_size, &mut cursors, &out_tx);
                                }
                            }
                            EventKind::Remove(RemoveKind::File)
                            | EventKind::Remove(RemoveKind::Any) => {
                                for path in event.paths {
                                    cursors.remove(&path);
                                }
                            }
                            _ => {
                                // ignore other kinds
                            }
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // periodic stop check
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        break;
                    }
                }
            }
        });

        let watcher = FileWatcher {
            stop_tx,
            handle: Some(handle),
        };
        Ok((watcher, out_rx))
    }

    fn read_new_bytes(
        path: &Path,
        segment_size: u64,
        cursors: &mut HashMap<PathBuf, u64>,
        out_tx: &cb_channel::Sender<FileChunk>,
    ) {
        // Only handle files directly under the watched directory
        if !path.is_file() {
            return;
        }

        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return,
        };

        // Parse and validate filename matches pattern "fileID-segment_size"
        let (file_id, fnum) = match Self::parse_file_id_and_segment_size(path) {
            Some(v) => v,
            None => return,
        };
        if fnum != segment_size {
            return;
        }

        // Treat any bytes beyond segment_size as invalid for watcher stream.
        // Cross-segment overflow bytes will be streamed from the next segment instead.
        let new_len = meta.len().min(segment_size);
        let cur = cursors.get(path).copied().unwrap_or(0).min(segment_size);

        let start = if new_len < cur { 0 } else { cur };
        if new_len <= start {
            // nothing new
            cursors.insert(path.to_path_buf(), new_len);
            return;
        }

        let read_len = (new_len - start) as usize;
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return,
        };
        if file.seek(SeekFrom::Start(start)).is_err() {
            return;
        }

        let mut buf = Vec::with_capacity(read_len);
        // Safety of capacity: we reserve but still read in a loop in case lengths drift
        let mut tmp = vec![0u8; 64 * 1024];
        let mut remaining = read_len as u64;
        while remaining > 0 {
            let to_read = std::cmp::min(tmp.len() as u64, remaining) as usize;
            match file.read(&mut tmp[..to_read]) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    remaining -= n as u64;
                }
                Err(_) => return,
            }
        }

        cursors.insert(path.to_path_buf(), start + buf.len() as u64);
        let pos = file_id.wrapping_mul(segment_size).wrapping_add(start);
        let _ = out_tx.send(FileChunk { pos, data: buf });
    }

    fn parse_file_id_and_segment_size(path: &Path) -> Option<(u64, u64)> {
        let file_name = path.file_name()?.to_str()?;
        let mut parts = file_name.split('-');
        let id_part = parts.next()?;
        let fixed_part = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        let file_id = id_part.parse::<u64>().ok()?;
        let fixed_num = fixed_part.parse::<u64>().ok()?;
        Some((file_id, fixed_num))
    }
}

impl Drop for FileWatcher {
    fn drop(&mut self) {
        // Signal stop and join the background thread
        let _ = self.stop_tx.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Consume chunks from a receiver and invoke a callback for each `FileChunk` by reference.
///
/// This function blocks until the channel is closed.
///
/// # Parameters
/// - `segment_size`: The segment size of the HPFile. Used to avoid reading overflow bytes
///   that exist beyond segment boundaries in old segments.
///
/// # Gap Filling
/// If a chunk arrives with `pos > cursor`, the gap is filled by reading from `rohpfile`.
/// This handles scenarios where ArchivalDB starts late or experiences sync interruptions.
/// Gap filling respects segment boundaries to avoid reading overflow bytes.
///
/// # Defensive Handling
/// - Duplicate chunks (`pos < cursor`): logged and skipped
/// - Out-of-order chunks: handled via gap filling
/// - This avoids panics from notify event anomalies (duplicates, reordering, truncation)
pub fn consume_chunks(
    start_pos: u64,
    segment_size: u64,
    rohpfile: &RoHPFile,
    rx: cb_channel::Receiver<FileChunk>,
    mut on_chunk: impl FnMut(&FileChunk),
) {
    fn fill_gap_once(
        rohpfile: &RoHPFile,
        segment_size: u64,
        cursor: u64,
        target_pos: u64,
        mut on_chunk: impl FnMut(&FileChunk),
    ) -> u64 {
        if target_pos <= cursor {
            return cursor;
        }
        let gap_len = (target_pos - cursor) as usize;
        let mut filled = Vec::with_capacity(gap_len);
        let mut remaining = gap_len;
        let mut offset = cursor as i64;
        while remaining > 0 {
            // Calculate how many bytes we can read within the current segment to avoid
            // reading overflow bytes beyond segment_size.
            let pos_in_segment = (offset as u64) % segment_size;
            let bytes_until_segment_end = (segment_size - pos_in_segment) as usize;
            let step = remaining.min(64 * 1024).min(bytes_until_segment_end);
            let mut tmp = vec![0u8; step];
            match rohpfile.read_at(&mut tmp, offset) {
                Ok(0) => break,
                Ok(n) => {
                    filled.extend_from_slice(&tmp[..n]);
                    remaining -= n;
                    offset += n as i64;
                }
                Err(_) => break,
            }
        }
        if filled.is_empty() {
            return cursor;
        }
        let pre_chunk = FileChunk {
            pos: cursor,
            data: filled,
        };
        on_chunk(&pre_chunk);
        pre_chunk
            .pos
            .saturating_add(pre_chunk.data.len() as u64)
    }

    let mut cursor = start_pos;
    // Short backoff retries for notify visibility delays (early event or flush timing).
    const GAP_FILL_RETRIES: usize = 4;
    const GAP_FILL_BACKOFF_MS: [u64; GAP_FILL_RETRIES] = [20, 50, 100, 200];
    for chunk in rx.iter() {
        if chunk.pos > cursor {
            // Fill the gap [cursor, chunk.pos). notify events can race flush/visibility,
            // so we do a best-effort fill and allow a single retry below.
            cursor = fill_gap_once(rohpfile, segment_size, cursor, chunk.pos, &mut on_chunk);
        }

        // Handle chunk position relative to cursor
        if chunk.pos < cursor {
            // Duplicate or out-of-order chunk that we've already processed
            // This can happen due to notify event anomalies (duplicate events, reordering)
            warn!(
                "Skipping duplicate/stale chunk: chunk.pos={} < cursor={}, skipping {} bytes",
                chunk.pos,
                cursor,
                chunk.data.len()
            );
            continue;
        }

        // At this point chunk.pos >= cursor; after gap filling, they should be equal.
        // If not equal, retry gap filling once (notify can be early / data not yet visible),
        // then assert to catch real logical inconsistencies:
        // - start_pos was invalid (pointing to pruned data)
        // - prune_head() was called while watcher was running without proper coordination
        // - or a serious filesystem error occurred
        if chunk.pos > cursor {
            thread::sleep(Duration::from_millis(20));
            cursor = fill_gap_once(rohpfile, segment_size, cursor, chunk.pos, &mut on_chunk);
        }

        // If still not equal, do a few short backoff retries to handle notify visibility delays.
        if chunk.pos > cursor {
            for &delay in GAP_FILL_BACKOFF_MS.iter() {
                thread::sleep(Duration::from_millis(delay));
                cursor = fill_gap_once(rohpfile, segment_size, cursor, chunk.pos, &mut on_chunk);
                if chunk.pos <= cursor {
                    break;
                }
            }
        }

        // If still not equal after retries, panic (by design).
        assert_eq!(
            chunk.pos, cursor,
            "Gap filling failed after retries: expected cursor={}, got chunk.pos={}. \
             This likely means start_pos was invalid or prune_head() raced with watcher.",
            cursor, chunk.pos
        );

        on_chunk(&chunk);
        cursor = cursor.saturating_add(chunk.data.len() as u64);
    }
}

/*
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Instant;

    fn recv_with_timeout<T>(rx: &cb_channel::Receiver<T>, timeout_ms: u64) -> Option<T> {
        let start = Instant::now();
        let to = Duration::from_millis(5);
        loop {
            if let Ok(v) = rx.try_recv() {
                return Some(v);
            }
            if start.elapsed() > Duration::from_millis(timeout_ms) {
                return None;
            }
            std::thread::sleep(to);
        }
    }

    #[test]
    fn test_filewatcher_basic_append_and_offsets() {
        let temp_dir = tempfile::tempdir().unwrap();
        let dir = temp_dir.path();
        let segment_size: u64 = 4096;

        // Create a file that matches the pattern and one that doesn't
        let good_path = dir.join(format!("1-{}", segment_size));
        let bad_path = dir.join("invalid_name");

        // Write some initial contents before starting watcher
        {
            let mut f = std::fs::File::create(&good_path).unwrap();
            f.write_all(b"hello").unwrap();
            f.sync_all().unwrap();
        }
        {
            let mut f = std::fs::File::create(&bad_path).unwrap();
            f.write_all(b"should be ignored").unwrap();
            f.sync_all().unwrap();
        }

        // Start watcher; since file existed, it should seed cursor to current len and only stream new bytes
        let (_watcher, rx) = FileWatcher::start(dir, segment_size).unwrap();

        // Append new data to the good file
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&good_path).unwrap();
            f.write_all(b" world").unwrap();
            f.sync_all().unwrap();
        }

        // We should receive a chunk with offset 5 and data " world"
        let chunk = recv_with_timeout(&rx, 2000).expect("didn't receive chunk");
        assert_eq!(chunk.data, b" world");
        assert_eq!(chunk.pos, 1 * segment_size + 5);

        // Create a new matching file; initial contents should be streamed from offset 0
        let good2_path = dir.join(format!("2-{}", segment_size));
        {
            let mut f = std::fs::File::create(&good2_path).unwrap();
            f.write_all(b"abc").unwrap();
            f.sync_all().unwrap();
        }

        let chunk2 = recv_with_timeout(&rx, 2000).expect("didn't receive chunk for create");
        assert_eq!(chunk2.data, b"abc");
        assert_eq!(chunk2.pos, 2 * segment_size + 0);

        // Ensure invalid filename writes are ignored
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&bad_path).unwrap();
            f.write_all(b" more").unwrap();
            f.sync_all().unwrap();
        }
        // No additional message should arrive for the bad file shortly
        assert!(recv_with_timeout(&rx, 500).is_none());
    }
} */