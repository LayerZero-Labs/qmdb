use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom, Write},
};

use super::{
    RecordHeader, RecordHeaderMut, RecordId, HEADER_SIZE, MAX_RECORD_PAYLOAD_SIZE, RECORD_ALIGNMENT,
};

pub struct SegmentFileWriter {
    /// The file that the segment is written to. The file is opened with append(true) but may also
    /// include write(true).
    file: File,
    /// The size of the segment file in bytes.
    file_size: u64,
}

impl SegmentFileWriter {
    /// Create a new segment file writer.
    ///
    /// The file is opened with append(true) but may also include write(true). The file stream is
    /// positioned at the end of the file.
    pub fn new(file: File, file_size: u64) -> Self {
        Self { file, file_size }
    }

    pub fn write_header(
        &mut self,
        payload_length: u32,
        record_id: RecordId,
    ) -> std::io::Result<()> {
        let mut header = [0u8; HEADER_SIZE as usize];
        {
            let mut header = RecordHeaderMut::new(&mut header);
            header.set_payload_length(payload_length);
            header.set_record_id(record_id);
        }
        self.file.write_all(&header)?;
        self.file_size += HEADER_SIZE as u64;
        Ok(())
    }

    pub fn write_payload(&mut self, payload: &[u8]) -> std::io::Result<()> {
        self.file.write_all(payload)?;
        // Calculate the next aligned position.
        let record_alignment = RECORD_ALIGNMENT as u64;
        let current_end = self.file_size + payload.len() as u64;
        let next_pos = if current_end % record_alignment == 0 {
            current_end
        } else {
            ((current_end / record_alignment) + 1) * record_alignment
        };
        // The reason we are setting the length here is because otherwise if we just seek and not
        // set the length, then the underlying file may not be extended.
        self.file.set_len(next_pos)?;
        self.file.seek(SeekFrom::Start(next_pos))?;
        self.file_size = next_pos;
        Ok(())
    }

    pub fn fsync(&mut self) -> std::io::Result<()> {
        self.file.sync_data()?;
        Ok(())
    }

    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    pub fn into_inner(self) -> File {
        self.file
    }
}

pub struct SegmentFileReader {
    buf_reader: BufReader<File>,
    header: [u8; HEADER_SIZE as usize],
    /// The position of the next record.
    ///
    /// `None` if the header has not been read yet.
    next_pos: Option<u64>,
    /// The payload length of the current record.
    ///
    /// `None` if the header has not been read yet.
    payload_length: Option<u32>,
    /// The size of the file.
    file_size: u64,
}

impl SegmentFileReader {
    pub fn new(file: File, file_size: Option<u64>) -> std::io::Result<Self> {
        let file_size = if let Some(file_size) = file_size {
            file_size
        } else {
            file.metadata()?.len()
        };
        Ok(Self {
            buf_reader: BufReader::new(file),
            header: [0u8; HEADER_SIZE as usize],
            next_pos: None,
            payload_length: None,
            file_size,
        })
    }

    /// Reads the header of the record.
    ///
    /// Returns `None` if the end of the file is reached.
    pub fn read_header(&mut self) -> std::io::Result<Option<RecordHeader<'_>>> {
        if self.next_pos.unwrap_or(0) >= self.file_size {
            return Ok(None);
        }
        self.buf_reader.read_exact(&mut self.header)?;
        let header = RecordHeader::new(&self.header);
        if header.payload_length() > MAX_RECORD_PAYLOAD_SIZE {
            // Rewind the buffer reader to the beginning of the header so that the next read
            // will read the next header.
            self.buf_reader
                .seek(SeekFrom::Current(-(HEADER_SIZE as i64)))?;

            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                anyhow::anyhow!(
                    "Record payload length is too large: {}",
                    header.payload_length()
                ),
            ));
        }
        self.payload_length = Some(header.payload_length());
        // It's not enough to just seek to the payload length because the records always span
        // an integer number of pages.
        let cur_pos = self.buf_reader.stream_position()?;
        let record_alignment = RECORD_ALIGNMENT as u64;
        let next_pos = ((cur_pos + self.payload_length.unwrap() as u64 + record_alignment - 1)
            / record_alignment)
            * record_alignment;
        self.next_pos = Some(next_pos);
        Ok(Some(header))
    }

    /// Skips the payload of the record.
    ///
    /// Must be called after `read_header`.
    pub fn skip_payload(&mut self) -> std::io::Result<()> {
        self.seek_next()?;
        Ok(())
    }

    /// Reads the payload of the record.
    ///
    /// Must be called after `read_header`.
    pub fn read_payload(&mut self, payload_buf: &mut Vec<u8>) -> std::io::Result<()> {
        payload_buf.resize(self.payload_length.unwrap() as usize, 0);
        self.buf_reader.read_exact(payload_buf)?;
        self.seek_next()?;
        Ok(())
    }

    /// Positions the reader at the beginning of the next record.
    ///
    /// Must be called after `read_header`.
    pub fn seek_next(&mut self) -> std::io::Result<()> {
        self.buf_reader
            .seek(SeekFrom::Start(self.next_pos.unwrap()))?;
        Ok(())
    }

    /// Returns the current position of the reader.
    pub fn pos(&mut self) -> std::io::Result<u64> {
        Ok(self.buf_reader.stream_position()?)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        File, RecordId, SegmentFileReader, SegmentFileWriter, HEADER_SIZE, MAX_RECORD_PAYLOAD_SIZE,
        RECORD_ALIGNMENT,
    };
    use tempfile::NamedTempFile;

    #[test]
    fn simple_roundtrip() {
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path().to_owned();

        {
            let mut writer = SegmentFileWriter::new(temp_file.reopen().unwrap(), 0);
            let payload = b"hello";
            writer.write_header(payload.len() as u32, 1.into()).unwrap();
            writer.write_payload(payload).unwrap();
            writer.fsync().unwrap();
        }

        {
            let mut reader = SegmentFileReader::new(File::open(&file_path).unwrap(), None).unwrap();
            let header = reader.read_header().unwrap().unwrap();
            assert_eq!(header.payload_length(), 5);
            assert_eq!(header.record_id(), 1.into());

            let mut payload = Vec::new();
            reader.read_payload(&mut payload).unwrap();
            assert_eq!(payload, b"hello");
        }
    }

    #[test]
    fn test_segment_file_writer_reader_compatibility() {
        let test_cases = vec![
            (
                "Complete segment",
                vec![(b"payload1", 1), (b"payload2", 2)],
                false,
            ),
            (
                "Multiple segments",
                vec![(b"payload1", 1), (b"payload2", 2), (b"payload3", 3)],
                false,
            ),
            (
                "Segment ending with header",
                vec![(b"payload1", 1), (b"payload2", 2)],
                true,
            ),
        ];

        for (case_name, payloads, end_with_header) in test_cases {
            let temp_file = NamedTempFile::new().unwrap();
            let file_path = temp_file.path().to_owned();

            // Write segment
            {
                let mut writer = SegmentFileWriter::new(temp_file.reopen().unwrap(), 0);
                for (payload, record_id) in payloads.iter() {
                    writer
                        .write_header(payload.len() as u32, (*record_id).into())
                        .unwrap();
                    writer.write_payload(*payload).unwrap();
                }
                if end_with_header {
                    writer
                        .write_header(0, (payloads.len() as u64 + 1).into())
                        .unwrap();
                }
                writer.fsync().unwrap();
            }

            // Read and verify
            {
                let mut reader =
                    SegmentFileReader::new(File::open(&file_path).unwrap(), None).unwrap();
                for (expected_payload, expected_record_id) in payloads.iter() {
                    let header = reader.read_header().unwrap().unwrap();
                    assert_eq!(
                        header.payload_length(),
                        expected_payload.len() as u32,
                        "Case '{}': Incorrect payload length",
                        case_name
                    );
                    assert_eq!(
                        header.record_id(),
                        (*expected_record_id).into(),
                        "Case '{}': Incorrect record ID",
                        case_name
                    );

                    let mut payload = Vec::new();
                    reader.read_payload(&mut payload).unwrap();
                    assert_eq!(
                        &payload, expected_payload,
                        "Case '{}': Incorrect payload",
                        case_name
                    );
                }

                if end_with_header {
                    let header = reader.read_header().unwrap().unwrap();
                    assert_eq!(
                        header.payload_length(),
                        0,
                        "Case '{}': Expected zero-length payload for ending header",
                        case_name
                    );
                    assert_eq!(
                        header.record_id(),
                        (payloads.len() as u64 + 1).into(),
                        "Case '{}': Incorrect record ID for ending header",
                        case_name
                    );
                }
            }
        }
    }

    #[test]
    fn test_single_record_file_size_alignment() {
        let test_cases = vec![
            ("Empty payload", vec![0]),
            ("Small payload", vec![1, 2, 3]),
            (
                "Payload size equal to alignment",
                vec![0; RECORD_ALIGNMENT as usize - HEADER_SIZE as usize],
            ),
            (
                "Payload size one less than alignment",
                vec![0; RECORD_ALIGNMENT as usize - HEADER_SIZE as usize - 1],
            ),
            (
                "Payload size one more than alignment",
                vec![0; RECORD_ALIGNMENT as usize - HEADER_SIZE as usize + 1],
            ),
            (
                "Large payload",
                vec![0; RECORD_ALIGNMENT as usize * 3 - HEADER_SIZE as usize + 42],
            ),
        ];

        for (case_name, payload) in test_cases {
            let temp_file = NamedTempFile::new().unwrap();
            let mut writer = SegmentFileWriter::new(temp_file.reopen().unwrap(), 0);

            // Write the payload
            writer
                .write_header(payload.len() as u32, RecordId(1))
                .unwrap();
            writer.write_payload(&payload).unwrap();
            writer.fsync().unwrap();
            let counted_file_size = writer.file_size();

            // Check file size
            let file = temp_file.reopen().unwrap();
            let file_size = file.metadata().unwrap().len();
            assert_eq!(file_size, counted_file_size);
            assert_eq!(
                file_size % RECORD_ALIGNMENT as u64,
                0,
                "Case '{}': File size {} is not a multiple of record alignment {}",
                case_name,
                file_size,
                RECORD_ALIGNMENT
            );

            // Verify that we can read the entire file
            let mut reader = SegmentFileReader::new(file, Some(file_size)).unwrap();
            let header = reader.read_header().unwrap().unwrap();
            assert_eq!(header.payload_length(), payload.len() as u32);
            assert_eq!(header.record_id(), RecordId(1));

            let mut read_payload = Vec::new();
            reader.read_payload(&mut read_payload).unwrap();
            assert_eq!(read_payload, payload);

            // Ensure we're at the end of the file
            assert!(reader.read_header().unwrap().is_none());
        }
    }

    #[test]
    fn test_multiple_records_file_size_alignment() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = SegmentFileWriter::new(temp_file.reopen().unwrap(), 0);

        let payloads = vec![
            vec![1, 2, 3],
            vec![4, 5, 6, 7],
            vec![8; RECORD_ALIGNMENT as usize - HEADER_SIZE as usize],
            vec![9; RECORD_ALIGNMENT as usize * 2 - HEADER_SIZE as usize + 1],
        ];

        for (i, payload) in payloads.iter().enumerate() {
            writer
                .write_header(payload.len() as u32, RecordId((i + 1) as u64))
                .unwrap();
            writer.write_payload(payload).unwrap();
            writer.fsync().unwrap();

            // Check file size after each write
            let file_size = writer.file_size();
            assert_eq!(
                file_size % RECORD_ALIGNMENT as u64,
                0,
                "Multiple records case: After writing record {}, file size {} is not a multiple of record alignment {}",
                i + 1,
                file_size,
                RECORD_ALIGNMENT
            );
        }

        // Verify reading multiple records
        let file = temp_file.reopen().unwrap();
        let file_size = file.metadata().unwrap().len();
        let mut reader = SegmentFileReader::new(file, Some(file_size)).unwrap();

        for (i, expected_payload) in payloads.iter().enumerate() {
            let header = reader.read_header().unwrap().unwrap();
            assert_eq!(header.payload_length(), expected_payload.len() as u32);
            assert_eq!(header.record_id(), RecordId((i + 1) as u64));

            let mut read_payload = Vec::new();
            reader.read_payload(&mut read_payload).unwrap();
            assert_eq!(&read_payload, expected_payload);
        }

        // Ensure we're at the end of the file
        assert!(reader.read_header().unwrap().is_none());
    }

    #[test]
    fn test_refuse_large_payload() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut writer = SegmentFileWriter::new(temp_file.reopen().unwrap(), 0);

        // Write a header with a payload length larger than MAX_RECORD_PAYLOAD_SIZE
        let oversized_payload_length = MAX_RECORD_PAYLOAD_SIZE as u32 + 1;
        writer
            .write_header(oversized_payload_length, RecordId(1))
            .unwrap();

        // Write some dummy data (doesn't matter what, as it shouldn't be read)
        writer.write_payload(&[0; 100]).unwrap();
        writer.fsync().unwrap();

        // Try to read the header
        let file = temp_file.reopen().unwrap();
        let file_size = file.metadata().unwrap().len();
        let mut reader = SegmentFileReader::new(file, Some(file_size)).unwrap();

        // The read_header method should return an error
        let result = reader.read_header();
        assert!(result.is_err());

        // Check that the error message mentions the payload length
        let error_message = match result {
            Ok(_) => panic!("expected error, got Ok()"),
            Err(e) => e,
        }
        .to_string();
        assert!(error_message.contains(&oversized_payload_length.to_string()));
        assert!(error_message.contains("too large"));
    }

    #[ignore]
    #[test]
    fn test_max_payload_size() {
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path().to_owned();

        // Write max-sized payload
        {
            let mut writer = SegmentFileWriter::new(temp_file.reopen().unwrap(), 0);
            let payload = vec![0u8; MAX_RECORD_PAYLOAD_SIZE as usize];
            writer
                .write_header(MAX_RECORD_PAYLOAD_SIZE, 1.into())
                .unwrap();
            writer.write_payload(&payload).unwrap();
            writer.fsync().unwrap();
        }

        // Read and verify max-sized payload
        {
            let mut reader = SegmentFileReader::new(File::open(&file_path).unwrap(), None).unwrap();
            let header = reader.read_header().unwrap().unwrap();
            assert_eq!(header.payload_length(), MAX_RECORD_PAYLOAD_SIZE);
            assert_eq!(header.record_id(), 1.into());

            let mut payload = Vec::new();
            reader.read_payload(&mut payload).unwrap();
            assert_eq!(payload.len(), MAX_RECORD_PAYLOAD_SIZE as usize);
        }
    }
}
