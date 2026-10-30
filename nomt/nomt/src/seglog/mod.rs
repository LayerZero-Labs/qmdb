//! A segment log is a file that provides a storage abstraction which allows appending data.
//!
//! The segment log is not suitable for random access. The only way to read data from the
//! segment log is to start from the oldest record and read all the way to the newest upon recovery.
//!
//! The segment log allows pruning of old data (from the beginning of the file) to reclaim space.
//!
//! The log is designed to operate efficiently with a small number of segments. It is the
//! responsibility of the user to prune the log periodically to maintain this efficiency.
//!
//! `max_segment_size` is a crucial parameter that determines the maximum size of a segment.
//! When setting this parameter, users should consider several factors:
//! 1. It should be large enough to prevent the creation of too many segments for the anticipated workload.
//! 2. It should not be excessively large, as this would make it harder to reclaim space efficiently.
//! 3. It should be balanced to allow for efficient pruning, as pruning involves linearly scanning the file.
//!
//! The user must carefully choose this parameter based on their expected record sizes and
//! append/prune patterns. The log is pruned on a segment basis, and a segment cannot be
//! removed if it still contains any used elements.
//!
//! There is a limit on the maximum size of a record payload, which is currently set to 1 GiB.
//! However, this limit may be subject to change in future versions.

use anyhow::{ensure, Context, Result};
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{Seek, SeekFrom},
    mem,
    path::{Path, PathBuf},
    sync::Arc,
};

mod segment_filename;
mod segment_rw;

use self::segment_rw::{SegmentFileReader, SegmentFileWriter};

const RECORD_ALIGNMENT: u32 = 4096; // 4K alignment
const HEADER_SIZE: u32 = 12; // 8 bytes for record ID, 4 bytes for payload length
const MAX_RECORD_PAYLOAD_SIZE: u32 = 1 << 30; // 1 GiB

/// A record ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RecordId(pub u64);

impl RecordId {
    pub fn nil() -> Self {
        RecordId(0)
    }

    pub fn from_bytes(bytes: [u8; 8]) -> Self {
        RecordId(u64::from_le_bytes(bytes))
    }

    pub fn next(&self) -> Self {
        RecordId(self.0 + 1)
    }

    pub fn prev(&self) -> Option<RecordId> {
        if self.0 == 0 {
            None
        } else {
            Some(RecordId(self.0 - 1))
        }
    }

    fn bytes(&self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    pub fn is_nil(&self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for RecordId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u64> for RecordId {
    fn from(id: u64) -> Self {
        RecordId(id)
    }
}

struct RecordHeader<'a> {
    data: &'a [u8],
}

impl<'a> RecordHeader<'a> {
    fn new(data: &'a [u8]) -> Self {
        assert!(data.len() >= HEADER_SIZE as usize);
        Self { data }
    }

    fn payload_length(&self) -> u32 {
        u32::from_le_bytes(self.data[0..4].try_into().unwrap())
    }

    fn record_id(&self) -> RecordId {
        RecordId::from_bytes(self.data[4..12].try_into().unwrap())
    }
}

struct RecordHeaderMut<'a> {
    data: &'a mut [u8],
}

impl<'a> RecordHeaderMut<'a> {
    fn new(data: &'a mut [u8]) -> Self {
        assert!(data.len() >= HEADER_SIZE as usize);
        Self { data }
    }

    fn set_payload_length(&mut self, length: u32) {
        self.data[0..4].copy_from_slice(&length.to_le_bytes());
    }

    fn set_record_id(&mut self, id: RecordId) {
        self.data[4..12].copy_from_slice(&id.bytes());
    }
}

#[derive(Debug)]
struct Segment {
    /// The ID of the segment.
    id: u32,
    path: PathBuf,
    /// The lowest record ID that this segment includes.
    ///
    /// If `nil`, then the segment is empty. Can happen upon recovery of the head segment with
    /// no committed records.
    min: RecordId,
    /// The highest record ID that this segment includes.
    ///
    /// Should be greater or equal to `min`.
    max: RecordId,
}

pub struct SegmentedLog {
    /// The path to the directory that contains the segment files. This is the path to the file
    /// opened in [`Self::root_dir_fd`].
    root_dir_path: PathBuf,
    /// The file descriptor of the directory that contains the segment files.
    root_dir_fd: Arc<File>,
    /// The prefix the segment files have.
    filename_prefix: String,
    /// The maximum size of a segment file.
    ///
    /// This is a soft limit. The last written record in a segment file may make it exceed this
    /// limit.
    max_segment_size: u64,
    /// The first record ID in the live range, inclusive.
    ///
    /// The records with IDs less than this are not present in the log. 0 is a special value
    /// that means that no records have been written yet.
    start_live: RecordId,
    /// The last record ID in the live range, inclusive.
    ///
    /// The records with IDs greater than this are not present in the log. 0 is a special value
    /// that means that no records have been written yet.
    end_live: RecordId,
    /// Segments' metadata.
    ///
    /// The segments are stored in the order that they were created. The oldest segments are at
    /// the beginning of the vector and the newest segments are at the end of the vector.
    /// We don't expect too many segments, so a vector is a good fit.
    ///
    /// The last segment is the head segment, if any.
    segments: Vec<Segment>,
    /// The head segment file writer.
    head_segment_writer: Option<SegmentFileWriter>,
}

impl SegmentedLog {
    /// Append a record to the log and return its ID.
    ///
    /// After this function returned, the data is guaranteed to be persisted.
    pub fn append(&mut self, data: &[u8]) -> Result<RecordId> {
        if data.len() > MAX_RECORD_PAYLOAD_SIZE as usize {
            return Err(anyhow::anyhow!(
                "Record payload size is too large: {}",
                data.len()
            ));
        }

        let record_id = self.end_live.next();

        // If the head segment is full or doesn't exist, create a new one.
        let root_dir_fsync = match self.head_segment_writer {
            None => {
                self.create_segment(record_id)?;
                true
            }
            Some(ref w) if w.file_size() >= self.max_segment_size => {
                self.create_segment(record_id)?;
                true
            }
            _ => false,
        };

        // UNWRAP: we know that the head segment exists and is the last one.
        let writer = self.head_segment_writer.as_mut().unwrap();
        let segment = self.segments.last_mut().unwrap();

        // Write the record to the segment file, fsync and update the segment metadata.
        writer.write_header(data.len() as u32, record_id)?;
        writer.write_payload(data)?;
        writer.fsync()?;

        // Once the write succeeded, update the live range.
        self.end_live = record_id;
        if self.start_live.is_nil() {
            self.start_live = record_id;
        }
        if segment.min.is_nil() {
            segment.min = record_id;
        }
        segment.max = record_id;

        if root_dir_fsync {
            // To uphold the guarantees provided by this function we should fsync the directory
            // after a new segment file is created.
            self.root_dir_fd.sync_all()?;
        }

        Ok(record_id)
    }

    /// Create a new segment.
    ///
    /// The new segment file will be created. The ex-head segment will be closed.
    fn create_segment(&mut self, min: RecordId) -> Result<()> {
        let new_segment_id = self.gen_segment_id();
        let filename = segment_filename::format(&self.filename_prefix, new_segment_id);
        let path = self.root_dir_path.join(filename);
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)?;
        let new_segment = Segment {
            id: new_segment_id,
            min,
            max: min,
            path,
        };
        // Replace the ex-head segment writer (closing it).
        self.head_segment_writer = Some(SegmentFileWriter::new(file, 0));
        self.segments.push(new_segment);
        Ok(())
    }

    fn gen_segment_id(&self) -> u32 {
        let last_segment_id = self.segments.last().map(|s| s.id).unwrap_or(0);
        last_segment_id.wrapping_add(1)
    }

    /// Prunes the items from the back of the log (oldest records).
    ///
    /// Prunes segments that lie outside of the new live range, i.e. deletes the old segments.
    ///
    /// This function updates the live range. Note, that this function is destructive, it deletes
    /// segments from the file system. Should a crash happen after this function was executed but
    /// before the manifest was updated, the recovery will fail. Therefore, you must ensure that
    /// the manifest was updated before calling this function.
    ///
    /// It's possible to return remove all items from the log, i.e. to reset the log to an empty
    /// state, by setting the new live end to zero. That would update the live range to `(0, 0)`.
    ///
    /// # Panics
    ///
    /// The new live range must be a subset of the old live range.
    pub fn prune_oldest(&mut self, new_start_live: RecordId) -> std::io::Result<()> {
        if new_start_live.is_nil() {
            self.start_live = RecordId::nil();
            self.end_live = RecordId::nil();
            self.remove_all_segments()?;
            return Ok(());
        }

        if self.segments.is_empty() {
            return Ok(());
        }

        if new_start_live > self.end_live {
            panic!(
                "New live start is greater than the live end: {} > {}",
                new_start_live, self.end_live
            );
        }
        if new_start_live < self.start_live {
            panic!(
                "The new start of the live range ({}) is less than the existing live start ({})",
                new_start_live, self.start_live
            );
        }
        self.start_live = new_start_live;

        while self.segments.len() > 1 {
            let oldest_segment = self.segments.first().unwrap();
            if oldest_segment.max >= new_start_live {
                // Segments are ordered by their min/max record IDs, all what follows is live and
                // we can stop pruning.
                break;
            }

            // Remove the segment file from the file system.
            let filename = segment_filename::format(&self.filename_prefix, oldest_segment.id);
            fs::remove_file(self.root_dir_path.join(filename))?;

            // Remove the segment from the in-memory list preserving the order.
            self.segments.remove(0);
        }
        Ok(())
    }

    /// Prunes the items from the front of the log (newest records).
    ///
    /// Prunes segments that lie outside of the new live range, i.e. deletes and truncates the
    /// segments to not include the items that lie after the new live end.
    ///
    /// This function updates the live range. Note, that this function is destructive, it deletes
    /// segments from the file system. Should a crash happen after this function was executed but
    /// before the manifest was updated, the recovery will fail. Therefore, you must ensure that
    /// the manifest was updated before calling this function.
    ///
    /// It's possible to return remove all items from the log, i.e. to reset the log to an empty
    /// state, by setting the new live end to zero. That would update the live range to `(0, 0)`.
    pub fn prune_recent(&mut self, new_end_live: RecordId) -> std::io::Result<()> {
        if new_end_live.is_nil() {
            self.start_live = RecordId::nil();
            self.end_live = RecordId::nil();
            self.remove_all_segments()?;
            return Ok(());
        }

        if self.segments.is_empty() {
            return Ok(());
        }

        // We should start from the newest segment (i.e. highest index) and iterate backwards.
        // We need to locate the segment that contains the new live end. That segment should be
        // truncated, removing all records that lie after the new live end. The segments with
        // IDs greater than the found segment will be deleted entirely.

        // Locate the segment that contains the new live end.
        let mut seg_index = self.segments.len() - 1;
        while seg_index > 0 {
            let segment = &self.segments[seg_index];
            if segment.min <= new_end_live {
                break;
            }
            seg_index -= 1;
        }

        // The segments that lie after the new head segment can be deleted.
        while self.segments.len() > seg_index + 1 {
            let filename =
                segment_filename::format(&self.filename_prefix, self.segments.last().unwrap().id);
            fs::remove_file(self.root_dir_path.join(filename))?;
            self.segments.pop();
        }
        self.root_dir_fd.sync_data()?;

        if let Some(head_segment_writer) = self.head_segment_writer.take().take() {
            let file = head_segment_writer.into_inner();
            drop(file);
        }

        // Segments do not contain an index, so we have to locate the last live record by
        // iterating over the records.
        let segment = &mut self.segments[seg_index];
        self.head_segment_writer = Some(truncate_head_segment(
            &self
                .root_dir_path
                .join(segment_filename::format(&self.filename_prefix, segment.id)),
            new_end_live,
        )?);
        segment.max = new_end_live;

        self.end_live = new_end_live;

        Ok(())
    }

    fn remove_all_segments(&mut self) -> std::io::Result<()> {
        let _ = self.head_segment_writer.take();

        for segment in &self.segments {
            fs::remove_file(&segment.path)?;
        }
        self.segments.clear();
        Ok(())
    }

    /// Get the live range.
    ///
    /// If there are no records in the log, the live range is `(0, 0)`.
    pub fn live_range(&self) -> (RecordId, RecordId) {
        (self.start_live, self.end_live)
    }
}

struct Recovery {
    candidates: Vec<Segment>,
    start_live: RecordId,
    end_live: RecordId,
    payload_buf: Vec<u8>,
    live_segment_start: Option<usize>,
    live_segment_end: Option<usize>,
}

impl Recovery {
    fn new(start_live: RecordId, end_live: RecordId) -> Self {
        Self {
            candidates: Vec::new(),
            start_live,
            end_live,
            payload_buf: Vec::new(),
            live_segment_start: None,
            live_segment_end: None,
        }
    }

    fn scan_root_dir(&mut self, root_dir_path: &PathBuf, filename_prefix: &str) -> Result<()> {
        let dir = fs::read_dir(&root_dir_path)?;
        for entry in dir {
            let entry = entry?;
            let filename = entry.file_name();
            if let Some(filename) = filename.to_str() {
                let path = entry.path();
                if filename.starts_with(&filename_prefix) {
                    let id = segment_filename::parse(filename_prefix, filename)?;
                    self.candidates.push(Segment {
                        id,
                        path,
                        min: RecordId::nil(),
                        max: RecordId::nil(),
                    });
                }
            }
        }
        self.candidates.sort_by_key(|c| c.id);
        if cfg!(debug_assertions) {
            let orig_len = self.candidates.len();
            self.candidates.dedup_by_key(|c| c.id);
            assert_eq!(orig_len, self.candidates.len());
        }

        // Now do some checks.
        for i in 0..self.candidates.len() {
            let candidate = &self.candidates[i];
            if candidate.id == 0 {
                return Err(anyhow::anyhow!(
                    "Segment ID is nil, file: {}",
                    candidate.path.display()
                ));
            }
            if i > 0 {
                let prev = &self.candidates[i - 1];
                if prev.id != candidate.id - 1 {
                    return Err(anyhow::anyhow!(
                        "Gap in segment IDs: this {}, last {}",
                        candidate.id,
                        prev.id
                    ));
                }
            }
        }

        Ok(())
    }

    fn scan_segment<F>(&mut self, index: usize, mut process_record: F) -> Result<()>
    where
        F: FnMut(RecordId, &[u8]) -> Result<()>,
    {
        let candidate = &self.candidates[index];

        let file = File::open(&candidate.path)?;
        let file_size = file.metadata()?.len();
        let mut seg_reader = SegmentFileReader::new(file, Some(file_size))?;

        let mut min: Option<RecordId> = None;
        let mut max: Option<RecordId> = None;
        let mut last: Option<RecordId> = None;
        loop {
            let header = match seg_reader.read_header()? {
                None => {
                    break;
                }
                Some(header) => header,
            };
            let record_id = header.record_id();
            if let Some(last) = last {
                ensure!(
                    record_id == last.next(),
                    "IDs are not ordered: this {}, expected {}",
                    record_id,
                    last.next(),
                );
            }

            let (was_live, became_live, became_nonlive) = self.on_next_record(index, record_id);

            if was_live || became_live || became_nonlive {
                seg_reader.read_payload(&mut self.payload_buf)?;
                process_record(record_id, &self.payload_buf)?;
            } else {
                seg_reader.skip_payload()?;
            }

            // Update the minimum and maximum record IDs observed in the segment.
            if min.is_none() {
                min = Some(record_id);
            }
            if max.map_or(true, |max| record_id > max) {
                max = Some(record_id);
            }
            last = Some(record_id);
        }
        // If the segment is empty, we set the min and max to nil. They are always Some after this
        // function returns.
        let candidate = &mut self.candidates[index];
        candidate.min = min.unwrap_or(0.into()).into();
        candidate.max = max.unwrap_or(0.into()).into();
        Ok(())
    }

    fn on_next_record(&mut self, segment_index: usize, record_id: RecordId) -> (bool, bool, bool) {
        let was_live = self.is_live();
        let became_live = self.enter_live(segment_index, record_id);
        let became_nonlive = self.exit_live(segment_index, record_id);
        (was_live, became_live, became_nonlive)
    }

    fn enter_live(&mut self, segment_index: usize, record_id: RecordId) -> bool {
        if self.start_live.is_nil() {
            return false;
        }
        if self.live_segment_start.is_none() && record_id >= self.start_live {
            self.live_segment_start = Some(segment_index);
            return true;
        }
        false
    }

    fn exit_live(&mut self, segment_index: usize, record_id: RecordId) -> bool {
        if self.start_live.is_nil() {
            return false;
        }
        if self.is_live() && self.live_segment_end.is_none() && record_id >= self.end_live {
            self.live_segment_end = Some(segment_index);
            return true;
        }
        false
    }

    /// Returns `true` if the given record ID is considered live.
    fn is_live(&self) -> bool {
        // If the `start_live` is nil, then the record trivially doesn't belong to the live
        // range.
        if self.start_live.is_nil() {
            assert!(self.end_live.is_nil());
            return false;
        }
        self.live_segment_start.is_some() && self.live_segment_end.is_none()
    }

    /// Remove the segments that do not contain any live records.
    fn remove_nonlive_segments(mut self) -> Result<Vec<Segment>> {
        let live_segments_indices: Option<(usize, usize)> =
            match (self.live_segment_start, self.live_segment_end) {
                (None, None) => None,
                (Some(start), Some(end)) => Some((start, end)),
                _ => anyhow::bail!("Invalid live segment indices"),
            };

        let nonlive_segments;
        let live_segments;

        if let Some((start, end)) = live_segments_indices {
            live_segments = self.candidates.drain(start..=end).collect::<Vec<_>>();
            nonlive_segments = mem::take(&mut self.candidates);
        } else {
            live_segments = Vec::new();
            nonlive_segments = mem::take(&mut self.candidates);
        }

        for segment in nonlive_segments {
            fs::remove_file(segment.path)?;
        }
        Ok(live_segments)
    }
}

/// Scans the segment file and returns the file offset of the end of the specified record.
fn scan_record_end(path: &Path, end_live: RecordId) -> std::io::Result<Option<u64>> {
    let mut seg_reader = SegmentFileReader::new(File::open(path)?, None)?;
    loop {
        let header = match seg_reader.read_header()? {
            None => {
                break Ok(None);
            }
            Some(header) => header,
        };
        let record_id = header.record_id();
        seg_reader.skip_payload()?;
        if record_id == end_live {
            // We have encountered the last live record. Truncate the segment to the
            // last committed record.
            let pos = seg_reader.pos()?;
            break Ok(Some(pos));
        }
    }
}

fn truncate_head_segment(
    path: &Path,
    new_end_live: RecordId,
) -> std::io::Result<SegmentFileWriter> {
    let end = match scan_record_end(path, new_end_live)? {
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                anyhow::anyhow!("Failed to find the last live record in the head segment"),
            ));
        }
        Some(offset) => offset,
    };

    let mut file = OpenOptions::new().append(true).write(true).open(path)?;
    file.set_len(end)?;
    file.sync_data()?;
    file.seek(SeekFrom::Start(end))?;

    Ok(SegmentFileWriter::new(file, end))
}

/// Opens a segmented log reading the records in the live range.
///
/// This function will read the records in the live range and pass them to the provided
/// callback. The records passed in the callback fall in the live range.
///
/// Returns early in case an error is encountered.
pub fn open<F>(
    root_dir_path: PathBuf,
    root_dir_fd: Arc<File>,
    filename_prefix: String,
    max_segment_size: u64,
    start_live: RecordId,
    end_live: RecordId,
    mut process_record: F,
) -> anyhow::Result<SegmentedLog>
where
    F: FnMut(RecordId, &[u8]) -> anyhow::Result<()>,
{
    if start_live.is_nil() ^ end_live.is_nil() {
        return Err(anyhow::anyhow!(
            "Start live and end live must both be nil or both be non-nil, got start: {}, end: {}",
            start_live,
            end_live,
        ));
    }
    let empty_live_range = start_live.is_nil();

    // Examine segments in the (segment) ID ascending order. For each the segment file, go over
    // the records in that file. Discard records until the record ID `live_start` is met.
    // At that point, start passing the records to the callback and stop once the record ID
    // `end_live` is met.
    //
    // The record IDs are strictly increasing in each segment file with no gaps. In case we
    // encounter a gap in record IDs, that's a corruption.
    //
    // The segment files are numbered sequentially. Gaps are possible due to file deletion, but
    // this only affects the segments that are pruned. The segments in the live range must be
    // gapless, otherwise the log is corrupted.
    //
    // The last segment must be the head, this is by construction because we only ever
    // append to the last segment and once the segment is filled, it is sealed and a new head
    // segment is created. In case a commit was interrupted, the last segment may contain records
    // in inconsistent state. We shall repair the log by truncating the last segment to the last
    // successfully committed record indicated by `end_live`.

    let mut recovery = Recovery::new(start_live, end_live);

    recovery.scan_root_dir(&root_dir_path, &filename_prefix)?;
    if !empty_live_range {
        for i in 0..recovery.candidates.len() {
            recovery
                .scan_segment(i, &mut process_record)
                .with_context(|| {
                    format!(
                        "Error during scanning segment {}",
                        recovery.candidates[i].path.display()
                    )
                })?;
        }
    }

    // At this point, the recovery struct has been populated with the segments with live records
    // and it should have identified the first and last live segments. Check that.
    if !empty_live_range {
        ensure!(
            recovery.live_segment_start.is_some(),
            "Failed to find the first live segment",
        );
        ensure!(
            recovery.live_segment_end.is_some(),
            "Failed to find the last live segment",
        );
    }

    let mut segments = recovery.remove_nonlive_segments()?;
    let mut head_segment_writer = None;
    if let Some(head) = segments.last_mut() {
        head.max = end_live;
        head_segment_writer = Some(truncate_head_segment(&head.path, end_live)?);
    }

    Ok(SegmentedLog {
        root_dir_path,
        root_dir_fd,
        filename_prefix,
        max_segment_size,
        start_live,
        end_live,
        segments,
        head_segment_writer,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        open, segment_filename, RecordId, Result, SegmentFileWriter, SegmentedLog, RECORD_ALIGNMENT,
    };
    use std::{
        fs::{self, File},
        path::Path,
        sync::Arc,
    };
    use tempfile::TempDir;

    /// Returns the size of the log that can fit `n` segments.
    ///
    /// This is relevant for appending records to the log, as the new segments are created when the
    /// head segment is filled to its maximum size.
    fn max_segment_size_for_n_records(n: usize) -> u64 {
        RECORD_ALIGNMENT as u64 * n as u64
    }

    /// Returns the default maximum segment size. This is used for the tests that don't do
    /// appending.
    fn default_max_segment_size() -> u64 {
        max_segment_size_for_n_records(2)
    }

    struct TestHarness {
        temp_dir: TempDir,
        filename_prefix: String,
    }

    impl TestHarness {
        fn new() -> Result<Self> {
            let temp_dir = TempDir::new()?;
            Ok(Self {
                temp_dir,
                filename_prefix: "test".to_string(),
            })
        }

        fn open_log(
            &self,
            max_segment_size: u64,
            start_live: impl Into<RecordId>,
            end_live: impl Into<RecordId>,
        ) -> Result<(SegmentedLog, Vec<(RecordId, Vec<u8>)>)> {
            let root_dir_fd = File::open(self.temp_dir.path())?;
            let mut records = Vec::new();
            let log = open(
                self.temp_dir.path().to_path_buf(),
                Arc::new(root_dir_fd),
                self.filename_prefix.clone(),
                max_segment_size,
                start_live.into(),
                end_live.into(),
                |record_id, payload| {
                    records.push((record_id, payload.to_vec()));
                    Ok(())
                },
            )?;
            Ok((log, records))
        }

        /// Creates a new segment file with the given segment ID.
        ///
        /// Useful for testing recovery from an unclean shutdown.
        fn new_segment(&self, segment_id: u32) -> Result<SegmentBuilder> {
            SegmentBuilder::new(self.temp_dir.path(), &self.filename_prefix, segment_id)
        }

        fn assert_segment_file_count(&self, count: usize) -> Result<()> {
            let dir = fs::read_dir(self.temp_dir.path())?;
            let mut segment_files = Vec::new();
            for entry_result in dir {
                match entry_result {
                    Ok(entry) => {
                        let path = entry.path();
                        if let Some(file_name) = path.file_name() {
                            if let Some(file_str) = file_name.to_str() {
                                if file_str.starts_with(&self.filename_prefix) {
                                    segment_files.push(path);
                                }
                            }
                        }
                    }
                    Err(_) => continue,
                }
            }
            assert_eq!(
                segment_files.len(),
                count,
                "Expected {} segment files, found {}",
                count,
                segment_files.len()
            );
            Ok(())
        }
    }

    struct SegmentBuilder {
        writer: Option<SegmentFileWriter>,
    }

    impl SegmentBuilder {
        fn new(root_dir_path: &Path, filename_prefix: &str, segment_id: u32) -> Result<Self> {
            let filename = segment_filename::format(filename_prefix, segment_id);
            let file = File::create(root_dir_path.join(filename))?;
            let writer = SegmentFileWriter::new(file, 0);
            Ok(Self {
                writer: Some(writer),
            })
        }

        fn write_record(
            &mut self,
            record_id: impl Into<RecordId>,
            payload: &[u8],
        ) -> Result<&mut Self> {
            self.writer
                .as_mut()
                .unwrap()
                .write_header(payload.len() as u32, record_id.into())?;
            self.writer.as_mut().unwrap().write_payload(payload)?;
            Ok(self)
        }

        fn write_header_only(
            &mut self,
            record_id: impl Into<RecordId>,
            payload_length: u32,
        ) -> Result<&mut Self> {
            self.writer
                .as_mut()
                .unwrap()
                .write_header(payload_length, record_id.into())?;
            Ok(self)
        }

        fn write(&mut self) -> Result<()> {
            let mut writer = self.writer.take().unwrap();
            writer.fsync()?;
            drop(writer);
            Ok(())
        }
    }

    #[test]
    fn append_exceeds_max_segment_size() -> Result<()> {
        let max_segment_size = max_segment_size_for_n_records(1);
        let h = TestHarness::new()?;
        let (mut log, records) = h.open_log(max_segment_size, RecordId::nil(), RecordId::nil())?;
        assert_eq!(records.len(), 0);
        drop(records);

        let large_data = vec![0u8; 150];
        let record_id = log.append(&large_data)?;
        drop(log);

        assert_eq!(record_id, RecordId::from(1));

        let (_log, records) = h.open_log(100, RecordId::from(1), RecordId::from(1))?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0], (RecordId::from(1), large_data));

        h.assert_segment_file_count(1)?;

        Ok(())
    }

    #[test]
    fn recovery_no_segments() -> Result<()> {
        let max_segment_size = default_max_segment_size();
        let harness = TestHarness::new()?;
        let (_log, records) =
            harness.open_log(max_segment_size, RecordId::nil(), RecordId::nil())?;
        assert_eq!(records.len(), 0);
        Ok(())
    }

    #[test]
    fn append_works() -> Result<()> {
        // Pick a max segment size that can fit at least 2 records. That will lead to creation of
        // 2 segments.
        let max_segment_size = max_segment_size_for_n_records(2);

        let h = TestHarness::new()?;
        let (mut log, records) = h.open_log(max_segment_size, RecordId::nil(), RecordId::nil())?;
        assert_eq!(records.len(), 0);
        drop(records);

        let record_id_1 = log.append(&vec![1u8; 10])?;
        let record_id_2 = log.append(&vec![2u8; 10])?;
        let record_id_3 = log.append(&vec![3u8; 10])?;
        drop(log);

        assert_eq!(record_id_1, RecordId::from(1));
        assert_eq!(record_id_2, RecordId::from(2));
        assert_eq!(record_id_3, RecordId::from(3));

        let (_log, records) = h.open_log(max_segment_size, RecordId::from(1), RecordId::from(3))?;
        assert_eq!(records.len(), 3);
        assert_eq!(records[0], (RecordId::from(1), vec![1u8; 10]));
        assert_eq!(records[1], (RecordId::from(2), vec![2u8; 10]));
        assert_eq!(records[2], (RecordId::from(3), vec![3u8; 10]));

        h.assert_segment_file_count(2)?;

        Ok(())
    }

    #[test]
    fn recovery_existing_records() -> Result<()> {
        let max_segment_size = default_max_segment_size();
        let h = TestHarness::new()?;

        h.new_segment(1)?.write_record(1, &vec![1u8; 10])?.write()?;
        h.new_segment(2)?.write_record(2, &vec![2u8; 5])?.write()?;
        h.new_segment(3)?.write_record(3, &vec![3u8; 10])?.write()?;
        let (_log, records) = h.open_log(max_segment_size, 1, 3)?;

        assert_eq!(records.len(), 3);
        assert_eq!(records[0], (RecordId::from(1), vec![1u8; 10]));
        assert_eq!(records[1], (RecordId::from(2), vec![2u8; 5]));
        assert_eq!(records[2], (RecordId::from(3), vec![3u8; 10]));
        h.assert_segment_file_count(3)?;

        Ok(())
    }

    #[test]
    fn recovery_uncommitted() -> Result<()> {
        // This test checks that the uncommitted records in the head segment are truncated.
        let h = TestHarness::new()?;

        h.new_segment(1)?
            .write_record(1, &vec![1u8; 10])?
            .write_record(2, &vec![2u8; 10])?
            .write()?;
        h.new_segment(2)?.write_record(3, &vec![3u8; 10])?.write()?;
        h.assert_segment_file_count(2)?;
        let max_segment_size = default_max_segment_size();
        let (_log, records) = h.open_log(max_segment_size, 1, 2)?;

        assert_eq!(records.len(), 2);
        assert_eq!(records[0], (RecordId::from(1), vec![1u8; 10]));
        assert_eq!(records[1], (RecordId::from(2), vec![2u8; 10]));

        // One segment file should be removed, expect 1 file.
        h.assert_segment_file_count(1)?;

        Ok(())
    }

    #[test]
    fn recovery_uncomitted_partial() -> Result<()> {
        // This test checks that an uncomitted partial record at the end of the head segment
        // is truncated.
        let h = TestHarness::new()?;

        h.new_segment(1)?
            .write_record(1, &vec![1u8; 10])?
            .write_record(2, &vec![2u8; 10])?
            .write()?;
        h.new_segment(2)?
            .write_header_only(RecordId::from(3), 16)?
            .write()?;
        h.assert_segment_file_count(2)?;
        let (_log, records) = h.open_log(default_max_segment_size(), 1, 2)?;

        assert_eq!(records.len(), 2);
        assert_eq!(records[0], (RecordId::from(1), vec![1u8; 10]));
        assert_eq!(records[1], (RecordId::from(2), vec![2u8; 10]));

        h.assert_segment_file_count(1)?;

        Ok(())
    }

    #[test]
    fn recovery_complicated() -> Result<()> {
        // The plan is to create a log with 3 segments.
        // - The first segment will contain values outside of the live range.
        // - The second segment will contain one record outside of the live range and one record
        //   inside of the live range.
        // - The third segment will be the head and will contain one record inside of the live
        //   range and one record outside of the live range.

        let h = TestHarness::new()?;
        h.new_segment(1)?
            .write_record(1, &vec![1u8; 10])?
            .write_record(2, &vec![2u8; 10])?
            .write()?;
        h.new_segment(2)?
            .write_record(3, &vec![3u8; 10])?
            .write_record(4, &vec![4u8; 10])?
            .write()?;
        h.new_segment(3)?
            .write_record(5, &vec![5u8; 10])?
            .write_record(6, &vec![6u8; 10])?
            .write()?;

        let (_log, records) = h.open_log(default_max_segment_size(), 4, 5)?;

        assert_eq!(records.len(), 2);
        assert_eq!(records[0], (RecordId::from(4), vec![4u8; 10]));
        assert_eq!(records[1], (RecordId::from(5), vec![5u8; 10]));

        Ok(())
    }

    #[test]
    fn recovery_fail_gap_in_segments() -> Result<()> {
        let h = TestHarness::new()?;

        h.new_segment(1)?.write_record(1, &vec![1u8; 10])?.write()?;
        h.new_segment(3)?.write_record(3, &vec![3u8; 10])?.write()?;
        let err = h.open_log(default_max_segment_size(), 1, 3);
        match err {
            Err(err) => {
                assert_eq!(err.to_string(), "Gap in segment IDs: this 3, last 1");
            }
            _ => panic!("Expected gap error"),
        }

        Ok(())
    }

    #[test]
    fn recovery_fail_gap_in_record_ids() -> Result<()> {
        let h = TestHarness::new()?;

        h.new_segment(1)?
            .write_record(1, &vec![1u8])?
            .write_record(3, &vec![2u8])?
            .write()?;
        let err = h.open_log(default_max_segment_size(), 1, 3);
        match err {
            Err(err) => {
                assert_eq!(
                    err.root_cause().to_string(),
                    "IDs are not ordered: this 3, expected 2"
                );
            }
            _ => panic!("Expected gap error"),
        }

        Ok(())
    }

    #[test]
    fn recovery_fail_misaligned_nonhead_segment() -> Result<()> {
        // Test that recovery fails if the non-head segment has a size that is not a multiple of
        // RECORD_ALIGNMENT.
        let h = TestHarness::new()?;
        // An non-head segment that has only a header, that should make size not a multiple of
        // RECORD_ALIGNMENT.
        h.new_segment(1)?
            .write_header_only(RecordId::from(1), 10)?
            .write()?;
        h.new_segment(2)?.write_record(2, &vec![2u8; 10])?.write()?;
        let err = h.open_log(default_max_segment_size(), 1, 2);
        match err {
            Ok(_) => panic!("Expected error"),
            Err(err) => {
                assert_eq!(err.root_cause().to_string(), "failed to fill whole buffer");
            }
        }
        Ok(())
    }

    #[test]
    fn recovery_empty_nonhead_segment() -> Result<()> {
        // Test that recovery if the non-head segment is empty.
        let h = TestHarness::new()?;
        // Empty non-head segment.
        h.new_segment(1)?.write()?;
        h.new_segment(2)?.write_record(2, &vec![2u8; 10])?.write()?;
        h.open_log(default_max_segment_size(), 2, 2).unwrap();
        Ok(())
    }

    #[test]
    fn append_after_recovery_empty_head_segment() -> Result<()> {
        // This checks that appending to the empty head segment works.
        let max_segment_size = max_segment_size_for_n_records(2);
        let h = TestHarness::new()?;
        h.new_segment(1)?
            .write_record(1, &vec![1u8; 10])?
            .write_record(2, &vec![2u8; 10])?
            .write()?;
        h.new_segment(2)?
            .write_record(3, &vec![3u8; 10])?
            .write_record(4, &vec![4u8; 10])?
            .write()?;

        // Recover the log. Only first two records should be present.
        let (mut log, records) = h.open_log(max_segment_size, 1, 2)?;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], (RecordId::from(1), vec![1u8; 10]));
        assert_eq!(records[1], (RecordId::from(2), vec![2u8; 10]));

        // Append new records.
        let record_id_3 = log.append(&vec![5u8; 10])?;
        let record_id_4 = log.append(&vec![6u8; 10])?;
        drop(log);

        assert_eq!(record_id_3, RecordId::from(3));
        assert_eq!(record_id_4, RecordId::from(4));

        let (_log, records) = h.open_log(max_segment_size, 1, 4)?;
        assert_eq!(records.len(), 4);
        assert_eq!(records[0], (RecordId::from(1), vec![1u8; 10]));
        assert_eq!(records[1], (RecordId::from(2), vec![2u8; 10]));
        assert_eq!(records[2], (RecordId::from(3), vec![5u8; 10]));
        assert_eq!(records[3], (RecordId::from(4), vec![6u8; 10]));

        // We expect 2 segment files to be present, because the first segment was truncated to 0
        // and thus should fit 2 records.
        h.assert_segment_file_count(2)?;

        Ok(())
    }

    #[test]
    fn append_after_recovery_nonempty_head_segment() -> Result<()> {
        let max_segment_size = max_segment_size_for_n_records(2);
        let h = TestHarness::new()?;
        h.new_segment(1)?
            .write_record(1, &vec![1u8; 10])?
            .write_record(2, &vec![2u8; 10])?
            .write()?;
        h.new_segment(2)?
            .write_record(3, &vec![3u8; 10])?
            .write_header_only(RecordId::from(4), 10)?
            .write()?;

        // Recover the log.
        //
        // This time the first record in the head segment should be present and the second
        // truncated.
        let (mut log, records) = h.open_log(max_segment_size, 1, 3)?;
        assert_eq!(records.len(), 3);
        assert_eq!(records[0], (RecordId::from(1), vec![1u8; 10]));
        assert_eq!(records[1], (RecordId::from(2), vec![2u8; 10]));
        assert_eq!(records[2], (RecordId::from(3), vec![3u8; 10]));

        // Append new records.
        let record_id_4 = log.append(&vec![5u8; 10])?;
        let record_id_5 = log.append(&vec![6u8; 10])?;
        drop(log);

        assert_eq!(record_id_4, RecordId::from(4));
        assert_eq!(record_id_5, RecordId::from(5));

        let (_log, records) = h.open_log(max_segment_size, 1, 5)?;
        assert_eq!(records.len(), 5);
        assert_eq!(records[0], (RecordId::from(1), vec![1u8; 10]));
        assert_eq!(records[1], (RecordId::from(2), vec![2u8; 10]));
        assert_eq!(records[2], (RecordId::from(3), vec![3u8; 10]));
        assert_eq!(records[3], (RecordId::from(4), vec![5u8; 10]));
        assert_eq!(records[4], (RecordId::from(5), vec![6u8; 10]));

        // This time we expect 3 segment files to be present, because the first segment was not
        // truncated to empty and a new segment should be created.
        h.assert_segment_file_count(3)?;

        Ok(())
    }

    #[test]
    fn prune_oldest() -> Result<()> {
        let h = TestHarness::new()?;
        h.new_segment(1)?.write_record(1, &vec![1u8; 10])?.write()?;
        h.new_segment(2)?.write_record(2, &vec![2u8; 10])?.write()?;
        h.new_segment(3)?.write_record(3, &vec![3u8; 10])?.write()?;
        let (mut log, _records) = h.open_log(default_max_segment_size(), 1, 3)?;
        h.assert_segment_file_count(3)?;

        // Prune 1 record.
        log.prune_oldest(2.into())?;
        h.assert_segment_file_count(2)?;

        // Prune 1 more record.
        log.prune_oldest(3.into())?;
        h.assert_segment_file_count(1)?;

        // Prune the last record. Ensure that the head segment is not truncated.
        log.prune_oldest(3.into())?;
        h.assert_segment_file_count(1)?;

        Ok(())
    }

    #[test]
    fn prune_recent() -> Result<()> {
        let h = TestHarness::new()?;
        h.new_segment(1)?.write_record(1, &vec![1u8; 10])?.write()?;
        h.new_segment(2)?.write_record(2, &vec![2u8; 10])?.write()?;
        h.new_segment(3)?.write_record(3, &vec![3u8; 10])?.write()?;
        let (mut log, _records) = h.open_log(default_max_segment_size(), 1, 3)?;
        h.assert_segment_file_count(3)?;

        // Prune 1 record.
        log.prune_recent(2.into())?;
        h.assert_segment_file_count(2)?;

        // Prune 1 more record.
        log.prune_recent(1.into())?;
        h.assert_segment_file_count(1)?;

        Ok(())
    }

    #[test]
    fn test_live_range_empty_log() -> Result<()> {
        let h = TestHarness::new()?;
        let (log, _) = h.open_log(default_max_segment_size(), 0, 0)?;
        assert_eq!(log.live_range(), (RecordId::nil(), RecordId::nil()));
        Ok(())
    }

    #[test]
    fn test_live_range_empty_log_append() -> Result<()> {
        let h = TestHarness::new()?;
        let (mut log, _) = h.open_log(default_max_segment_size(), 0, 0)?;
        let record_id_1 = log.append(&vec![1u8; 10])?;
        assert_eq!(record_id_1, RecordId::from(1));
        assert_eq!(log.live_range(), (RecordId::from(1), RecordId::from(1)));
        Ok(())
    }

    #[test]
    fn test_live_range() -> Result<()> {
        let h = TestHarness::new()?;

        // Test that the new log has live range (0,0)
        let (mut log, _) = h.open_log(default_max_segment_size(), 0, 0)?;
        assert_eq!(log.live_range(), (RecordId::nil(), RecordId::nil()));
        let record_id_1 = log.append(&vec![1u8; 10])?;
        assert_eq!(log.live_range(), (RecordId::from(1), RecordId::from(1)));
        drop(log);

        // Reopen the log and append another record.
        let (mut log, _) = h.open_log(default_max_segment_size(), 1, 1)?;
        let record_id_2 = log.append(&vec![2u8; 10])?;
        assert_eq!(log.live_range(), (record_id_1, record_id_2));

        // Test that appending to a log with live range (1, 1) updates its live range to (1, 2)
        let record_id_3 = log.append(&vec![3u8; 10])?;
        assert_eq!(log.live_range(), (record_id_1, record_id_3));

        Ok(())
    }

    #[test]
    fn test_live_range_append_to_nonempty_log() -> Result<()> {
        let h = TestHarness::new()?;
        h.new_segment(1)?.write_record(1, &vec![1u8; 10])?.write()?;
        h.new_segment(2)?.write_record(2, &vec![2u8; 10])?.write()?;
        let (mut log, _) = h.open_log(default_max_segment_size(), 1, 2)?;
        assert_eq!(log.live_range(), (RecordId::from(1), RecordId::from(2)));
        let record_id_3 = log.append(&vec![3u8; 10])?;
        assert_eq!(log.live_range(), (RecordId::from(1), record_id_3));
        Ok(())
    }

    #[test]
    fn test_live_range_prune_oldest() -> Result<()> {
        let h = TestHarness::new()?;
        h.new_segment(1)?.write_record(1, &vec![1u8; 10])?.write()?;
        h.new_segment(2)?.write_record(2, &vec![2u8; 10])?.write()?;
        let (mut log, _) = h.open_log(default_max_segment_size(), 1, 2)?;
        assert_eq!(log.live_range(), (RecordId::from(1), RecordId::from(2)));
        log.prune_oldest(2.into())?;
        assert_eq!(log.live_range(), (RecordId::from(2), RecordId::from(2)));
        Ok(())
    }

    #[test]
    fn test_live_range_prune_recent() -> Result<()> {
        let h = TestHarness::new()?;
        h.new_segment(1)?.write_record(1, &vec![1u8; 10])?.write()?;
        h.new_segment(2)?.write_record(2, &vec![2u8; 10])?.write()?;
        let (mut log, _) = h.open_log(default_max_segment_size(), 1, 2)?;
        assert_eq!(log.live_range(), (RecordId::from(1), RecordId::from(2)));
        log.prune_recent(1.into())?;
        assert_eq!(log.live_range(), (RecordId::from(1), RecordId::from(1)));
        Ok(())
    }

    #[test]
    fn test_prune_recent_segment_choice() -> Result<()> {
        let h = TestHarness::new()?;
        h.new_segment(1)?
            .write_record(1, &[1u8; 10])?
            .write_record(2, &[2u8; 10])?
            .write_record(3, &[3u8; 10])?
            .write()?;

        h.new_segment(2)?
            .write_record(4, &[4u8; 10])?
            .write_record(5, &[5u8; 10])?
            .write_record(6, &[6u8; 10])?
            .write()?;

        h.new_segment(3)?
            .write_record(7, &[4u8; 10])?
            .write_record(8, &[5u8; 10])?
            .write_record(9, &[6u8; 10])?
            .write()?;

        let (mut log, _) = h.open_log(default_max_segment_size(), 1, 9)?;
        assert_eq!(log.live_range(), (RecordId::from(1), RecordId::from(9)));
        log.prune_recent(5.into())?;

        Ok(())
    }

    #[test]
    fn test_live_range_prune_recent_to_empty() -> Result<()> {
        let h = TestHarness::new()?;
        h.new_segment(1)?.write_record(1, &vec![1u8; 10])?.write()?;
        h.new_segment(2)?.write_record(2, &vec![2u8; 10])?.write()?;
        let (mut log, _) = h.open_log(default_max_segment_size(), 1, 2)?;
        assert_eq!(log.live_range(), (RecordId::from(1), RecordId::from(2)));
        log.prune_recent(0.into())?;
        assert_eq!(log.live_range(), (RecordId::nil(), RecordId::nil()));
        h.assert_segment_file_count(0)?;
        Ok(())
    }

    #[test]
    fn test_live_range_prune_oldest_to_empty() -> Result<()> {
        let h = TestHarness::new()?;
        h.new_segment(1)?.write_record(1, &vec![1u8; 10])?.write()?;
        h.new_segment(2)?.write_record(2, &vec![2u8; 10])?.write()?;
        let (mut log, _) = h.open_log(default_max_segment_size(), 1, 2)?;
        assert_eq!(log.live_range(), (RecordId::from(1), RecordId::from(2)));
        log.prune_oldest(0.into())?;
        assert_eq!(log.live_range(), (RecordId::nil(), RecordId::nil()));
        h.assert_segment_file_count(0)?;
        Ok(())
    }
}
