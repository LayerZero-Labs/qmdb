use anyhow::{anyhow, Result};
use dashmap::DashMap;
use std::fs::{self, File};
use std::io;
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::fs::FileExt;

/// Read-only Head-prunable file that can read from an existing HPFile directory layout
#[derive(Debug)]
pub struct RoHPFile {
    dir_name: String,
    segment_size: i64,
    file_map: DashMap<i64, Arc<File>>, // lazily opened read-only files
}

impl RoHPFile {
    /// Create a `RoHPFile` bound to a directory with files named as `fileId-segmentSize`.
    /// It opens files lazily when reading crosses into a new segment.
    pub fn new(segment_size: i64, dir_name: String) -> Result<RoHPFile> {
        // Best-effort validation: ensure existing files (if any) conform to naming scheme
        // and segment size. We don't have to eagerly open them.
        let _ = Self::get_file_ids(&dir_name, segment_size)?;
        Ok(RoHPFile {
            dir_name,
            segment_size,
            file_map: DashMap::new(),
        })
    }

    fn get_file_ids(dir_name: &str, segment_size: i64) -> Result<Vec<i64>> {
        let mut id_list = Vec::new();
        for entry in fs::read_dir(dir_name)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                continue;
            }
            let file_name = entry.file_name().to_string_lossy().to_string();
            let id = Self::parse_filename(segment_size, &file_name)?;
            id_list.push(id);
        }
        Ok(id_list)
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

    fn open_file(&self, file_id: i64) -> io::Result<Arc<File>> {
        let file_name = format!("{}/{}-{}", self.dir_name, file_id, self.segment_size);
        let file = File::options().read(true).open(file_name)?;
        Ok(Arc::new(file))
    }

    /// Read data from file at `offset` to fill `bz`.
    /// If the target segment file has not been opened yet, it will be opened automatically.
    ///
    /// # Segment boundary & overflow semantics
    ///
    /// This method **allows crossing segment boundaries**: if `bz.len()` spans multiple
    /// segments, it will automatically split the read at segment boundaries and continue
    /// in the next segment.
    ///
    /// To prevent ambiguity with HPFile's cross-segment append overflow duplication, this
    /// read path **never reads bytes beyond `segment_size` within any segment**, even if
    /// the underlying physical file temporarily exceeds `segment_size`.
    pub fn read_at(&self, bz: &mut [u8], offset: i64) -> io::Result<usize> {
        if self.segment_size <= 0 {
            return Ok(0);
        }
        if offset < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RoHPFile::read_at offset must be non-negative",
            ));
        }

        let mut remaining = bz.len();
        let mut written = 0usize;
        let mut cur_offset = offset;

        while remaining > 0 {
            let file_id = cur_offset / self.segment_size;
            let pos = cur_offset % self.segment_size;

            // Never read beyond segment_size within a segment (avoid overflow region).
            let bytes_until_end = (self.segment_size - pos) as usize;
            let step = remaining.min(bytes_until_end);

            // Get or lazily open file.
            let file_arc: Arc<File> = if let Some(f) = self.file_map.get(&file_id) {
                f.clone()
            } else {
                let arc_file = self.open_file(file_id)?;
                match self.file_map.entry(file_id) {
                    dashmap::mapref::entry::Entry::Occupied(o) => o.get().clone(),
                    dashmap::mapref::entry::Entry::Vacant(v) => {
                        v.insert(arc_file.clone());
                        arc_file
                    }
                }
            };

            let n = file_arc.read_at(&mut bz[written..written + step], pos as u64)?;
            if n == 0 {
                break;
            }
            written += n;
            remaining -= n;
            cur_offset += n as i64;

            if n < step {
                // Short read/EOF for this segment -> stop.
                break;
            }
        }

        Ok(written)
    }

    /// Close all opened small files
    pub fn close(&self) {
        self.file_map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HPFile;

    #[test]
    fn test_rohpfile_basic_reads_and_lazy_open() {
        let temp_dir = tempfile::Builder::new()
            .prefix("rohp_file_test")
            .tempdir()
            .unwrap();
        let dir = temp_dir.path().to_str().unwrap().to_string();

        let buffer_size = 64i64;
        let segment_size = 128i64;

        let hp = HPFile::new(buffer_size, segment_size, dir.clone()).unwrap();

        // Prepare data across multiple segments
        let slice0 = [1u8; 44];
        let mut buffer = vec![];
        let mut pos = hp.append(&slice0, &mut buffer).unwrap();
        assert_eq!(0, pos);

        let slice1a = [2u8; 16];
        let slice1b = [3u8; 10];
        let mut slice1 = Vec::new();
        slice1.extend_from_slice(&slice1a);
        slice1.extend_from_slice(&slice1b);
        pos = hp.append(&slice1, &mut buffer).unwrap();
        assert_eq!(44, pos);

        let slice2a = [4u8; 25];
        let slice2b = [5u8; 25];
        let mut slice2 = Vec::new();
        slice2.extend_from_slice(&slice2a);
        slice2.extend_from_slice(&slice2b);
        pos = hp.append(&slice2, &mut buffer).unwrap();
        assert_eq!(70, pos);

        // Force creation of a second segment by filling up the first
        // slice3 crosses segment boundary: [120, 136) spans segment 0 and 1
        let slice3 = [6u8; 16];
        pos = hp.append(&slice3, &mut buffer).unwrap();
        assert_eq!(120, pos);

        // Add data entirely in segment 1 for testing
        let slice4 = [7u8; 20];
        pos = hp.append(&slice4, &mut buffer).unwrap();
        // slice4 starts at offset 136 (in segment 1, pos = 136 % 128 = 8)
        assert_eq!(136, pos);

        // Flush buffered writes to disk
        hp.flush(&mut buffer).unwrap();

        // Now read using RoHPFile from the same directory
        let ro = RoHPFile::new(segment_size, dir.clone()).unwrap();

        let mut check0 = [0u8; 44];
        ro.read_at(&mut check0, 0).unwrap();
        assert_eq!(slice0.to_vec(), check0.to_vec());

        let mut check1 = vec![0u8; slice1.len()];
        ro.read_at(&mut check1, 44).unwrap();
        assert_eq!(slice1, check1);

        let mut check2 = vec![0u8; slice2.len()];
        ro.read_at(&mut check2, 70).unwrap();
        assert_eq!(slice2, check2);

        // Read the part of slice3 that's in segment 0: [120, 128)
        let mut check3_seg0 = [0u8; 8];
        ro.read_at(&mut check3_seg0, 120).unwrap();
        assert_eq!(&slice3[..8], &check3_seg0);

        // Read the part of slice3 that's in segment 1: [128, 136) = segment 1's [0, 8)
        let mut check3_seg1 = [0u8; 8];
        ro.read_at(&mut check3_seg1, 128).unwrap();
        assert_eq!(&slice3[8..], &check3_seg1);

        // Cross-segment read should work and should not read overflow bytes beyond segment_size.
        let mut check3_cross = [0u8; 16];
        let n = ro.read_at(&mut check3_cross, 120).unwrap();
        assert_eq!(n, 16);
        assert_eq!(slice3.to_vec(), check3_cross.to_vec());

        // Read slice4 entirely from segment 1 (lazy open of a new file)
        let mut check4 = [0u8; 20];
        ro.read_at(&mut check4, 136).unwrap();
        assert_eq!(slice4.to_vec(), check4.to_vec());
    }
}