use crate::entryfile::EntryFile;
use crossbeam::channel::unbounded;
use dashmap::DashMap;
use parking_lot::Mutex;
use qmdb_common::def::{BIG_BUF_SIZE, DEFAULT_ENTRY_SIZE};
use qmdb_common::entryfile::entry::{Entry, EntryBz};
use qmdb_common::entryfile::readbuf::ReadBuf;
use qmdb_common::utils::{new_big_buf_boxed, BigBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
const BIG_BUF_SIZE_I64: i64 = BIG_BUF_SIZE as i64;

#[derive(Clone)]
pub struct ExtEntryFile {
    pub ef: Arc<EntryFile>,
    pub eb: Arc<EntryBuffer>,
    pub ebw: Arc<Mutex<EntryBufferWriter>>,
    pub shard_id: usize,
}

impl ExtEntryFile {
    pub fn get_entry_bz(&self, file_pos: i64, buf: &mut ReadBuf, use_uring: bool) {
        let mut in_disk = self.eb.get_entry_bz(file_pos, buf);
        if !buf.is_empty() {
            return;
        }
        if !in_disk {
            // it is very rare that we must read from EntryBufferWriter
            let mut ebw = self.ebw.lock_arc();
            in_disk = ebw.get_entry_bz_at(file_pos, buf);
        }
        if !buf.is_empty() {
            return;
        }

        #[cfg(all(target_os = "linux", not(feature = "disable_dio")))]
        if use_uring && self.ef.read_entry_with_uring(file_pos, buf) {
            return;
        }
        self.ef.read_entry(file_pos, buf);

        assert!(!buf.is_empty(), "pos {file_pos} is empty");
    }
}

// It contains content of the new range [Start, End] of EntryFile
// This content can be read from DRAM before it is flushed to EntryFile,
// One updater appends entries to it, and then one flusher pops entries out from it.

pub struct EntryBufferWriter {
    pub entry_buffer: Arc<EntryBuffer>,
    curr_buf: Option<Box<BigBuf>>,
}

impl EntryBufferWriter {
    pub fn into_ext_entry_file(self, ef: &Arc<EntryFile>, shard_id: usize) -> ExtEntryFile {
        ExtEntryFile {
            ef: ef.clone(),
            eb: self.entry_buffer.clone(),
            ebw: Arc::new(Mutex::new(self)),
            shard_id,
        }
    }

    pub fn append(&mut self, entry: &Entry, deactived_serial_num_list: &[u64]) -> i64 {
        let (pos, curr_buf) = self.entry_buffer.append(
            entry,
            deactived_serial_num_list,
            self.curr_buf.take().unwrap(),
        );
        self.curr_buf = Some(curr_buf);
        pos
    }

    pub fn end_block(&mut self, compact_done_pos: i64, compact_done_sn: u64, sn_end: u64) {
        let curr_buf: &Box<BigBuf> = self.curr_buf.as_ref().unwrap();
        let curr_buf_clone = curr_buf.clone();
        self.entry_buffer.end_block(
            compact_done_pos,
            compact_done_sn,
            sn_end,
            self.curr_buf.take().unwrap(),
        );
        self.curr_buf = Some(curr_buf_clone);
    }

    pub fn get_entry_bz_at(&mut self, file_pos: i64, buf: &mut ReadBuf) -> bool {
        let in_disk;
        let curr_buf = self.curr_buf.take().unwrap();
        (in_disk, self.curr_buf) = self
            .entry_buffer
            .get_entry_bz_at(file_pos, Some(curr_buf), buf);
        in_disk
    }
}

pub struct EntryBuffer {
    start: AtomicI64,
    end: AtomicI64,
    buf_map: DashMap<i64, Box<BigBuf>>,
    free_list: Mutex<Vec<Box<BigBuf>>>,
    pos_sender: crossbeam::channel::Sender<i64>,
}

pub fn new(start: i64, buf_margin: usize) -> (EntryBufferWriter, EntryBufferReader) {
    let (pos_sender, pos_receiver) = unbounded();

    let entry_buffer = EntryBuffer {
        start: AtomicI64::new(start),
        end: AtomicI64::new(start),
        buf_map: DashMap::new(),
        free_list: Mutex::new(Vec::new()),
        pos_sender,
    };

    let arc_entry_buffer = Arc::new(entry_buffer);

    let entry_buffer_reader = EntryBufferReader {
        entry_buffer: arc_entry_buffer.clone(),
        curr_buf: None,
        scratch_pad: Vec::with_capacity(DEFAULT_ENTRY_SIZE),
        end_pos: start,
        curr_pos: start,
        buf_margin,
        pos_receiver,
    };

    let entry_buffer_writer = EntryBufferWriter {
        entry_buffer: arc_entry_buffer.clone(),
        curr_buf: Some(new_big_buf_boxed()),
    };

    (entry_buffer_writer, entry_buffer_reader)
}

impl EntryBuffer {
    fn end_block(
        &self,
        compact_done_pos: i64,
        compact_done_sn: u64,
        sn_end: u64,
        curr_buf: Box<BigBuf>,
    ) {
        let end = self.end.load(Ordering::SeqCst);
        self.buf_map.insert(end / BIG_BUF_SIZE_I64, curr_buf);
        self.pos_sender.send(end).unwrap();
        self.pos_sender.send(i64::MIN).unwrap();
        self.pos_sender.send(compact_done_pos).unwrap();
        self.pos_sender.send(compact_done_sn as i64).unwrap();
        self.pos_sender.send(sn_end as i64).unwrap();
    }

    fn allocate_big_buf(&self) -> Box<BigBuf> {
        let mut free_list = self.free_list.lock();
        if !free_list.is_empty() {
            return free_list.pop().unwrap();
        }
        new_big_buf_boxed()
    }

    fn append(
        &self,
        entry: &Entry,
        deactived_serial_num_list: &[u64],
        mut curr_buf: Box<BigBuf>,
    ) -> (i64, Box<BigBuf>) {
        let size = entry.get_serialized_len(deactived_serial_num_list.len());
        if size > BIG_BUF_SIZE {
            panic!("Entry too large {size} vs {BIG_BUF_SIZE}");
        }
        let file_pos = self.end.load(Ordering::SeqCst);
        let idx = file_pos / BIG_BUF_SIZE_I64;
        let offset = file_pos % BIG_BUF_SIZE_I64;
        if offset + (size as i64) < BIG_BUF_SIZE_I64 {
            // curr_buf is large enough
            entry.dump(&mut curr_buf[offset as usize..], deactived_serial_num_list);
            self.end.fetch_add(size as i64, Ordering::SeqCst);
            return (file_pos, curr_buf);
        }
        let mut new_buf = self.allocate_big_buf();
        let total_length = entry.dump(&mut new_buf[..], deactived_serial_num_list);
        let copied_length = BIG_BUF_SIZE - offset as usize;
        curr_buf[offset as usize..].copy_from_slice(&new_buf[..copied_length]);
        new_buf.copy_within(copied_length..total_length, 0);
        self.buf_map.insert(idx, curr_buf);
        self.end.fetch_add(size as i64, Ordering::SeqCst);
        self.pos_sender.send(file_pos).unwrap();
        (file_pos, new_buf)
    }

    fn get_and_remove_buf(&self, idx: i64, remove_idx: i64) -> Box<BigBuf> {
        let res = self.buf_map.get(&idx).unwrap().clone();
        if remove_idx >= 0 {
            let new_start = (remove_idx + 1) * BIG_BUF_SIZE as i64;
            let old_start = self.start.load(Ordering::SeqCst);
            if old_start < new_start {
                self.start.store(new_start, Ordering::SeqCst);
            }
            if let Some((_, buf)) = self.buf_map.remove(&remove_idx) {
                self.free_list.lock().push(buf);
            }
        }
        res
    }

    pub fn get_entry_bz(&self, file_pos: i64, buf: &mut ReadBuf) -> bool {
        let (in_disk, _) = self.get_entry_bz_at(file_pos, Option::None, buf);
        in_disk
    }

    fn get_entry_bz_at(
        &self,
        file_pos: i64,
        curr_buf: Option<Box<BigBuf>>,
        buf: &mut ReadBuf,
    ) -> (bool, Option<Box<BigBuf>>) {
        assert!(buf.is_empty(), "buf must be empty");
        let (idx, offset) = (file_pos / BIG_BUF_SIZE_I64, file_pos % BIG_BUF_SIZE_I64);
        let start = self.start.load(Ordering::SeqCst);
        let end = self.end.load(Ordering::SeqCst);
        if file_pos < start {
            return (true /*inDisk*/, curr_buf);
        }
        if file_pos >= end {
            panic!("Read past end");
        }
        let curr_buf_start = (end / BIG_BUF_SIZE_I64) * BIG_BUF_SIZE_I64; //round-down
        if file_pos >= curr_buf_start && curr_buf.is_none() {
            return (false /*inDisk*/, None);
        }
        if file_pos >= curr_buf_start && curr_buf.is_some() {
            // reading currBuf is enough
            let curr_buf = curr_buf.unwrap();
            let len_bytes = &curr_buf[offset as usize..offset as usize + 5];
            let size = EntryBz::get_entry_len(len_bytes) as i64;
            buf.initialize_from(&curr_buf[offset as usize..(offset + size) as usize]);
            return (false /*inDisk*/, Some(curr_buf));
        }
        let target = self.buf_map.get(&idx);
        if target.is_none() {
            // check if start has just been changed
            let start = self.start.load(Ordering::SeqCst);
            if file_pos < start {
                return (true /*inDisk*/, curr_buf);
            }
        }
        let target = target.unwrap();
        let len_bytes = &target[offset as usize..offset as usize + 5];
        let size = EntryBz::get_entry_len(len_bytes) as i64;
        if offset + size <= BIG_BUF_SIZE as i64 {
            // only need to read target
            buf.initialize_from(&target[offset as usize..(offset + size) as usize]);
            return (false /*inDisk*/, curr_buf);
        }
        if file_pos + size > curr_buf_start && curr_buf.is_none() {
            // need to read curr_buf but curr_buf is not provided
            return (false /*inDisk*/, curr_buf);
        }
        let copied_length = BIG_BUF_SIZE - offset as usize;
        buf.extend_from_slice(&target[offset as usize..]);
        if file_pos + size > curr_buf_start && curr_buf.is_some() {
            let curr_buf = curr_buf.as_ref().unwrap();
            buf.extend_from_slice(&curr_buf[..(size as usize - copied_length)]);
        } else {
            let next_buf = self.buf_map.get(&(idx + 1)).unwrap();
            buf.extend_from_slice(&next_buf[..(size as usize - copied_length)]);
        }
        (false /*inDisk*/, curr_buf)
    }
}

pub struct EntryBufferReader {
    entry_buffer: Arc<EntryBuffer>,
    curr_buf: Option<Box<BigBuf>>,
    scratch_pad: Vec<u8>,
    end_pos: i64,
    curr_pos: i64,
    buf_margin: usize,
    pos_receiver: crossbeam::channel::Receiver<i64>,
}

impl EntryBufferReader {
    pub fn read_next_entry<F>(&mut self, mut access: F) -> (/*endOfBlock*/ bool, /*file_pos*/ i64)
    where
        F: FnMut(EntryBz),
    {
        let file_pos = self.curr_pos;
        // At the beginning/ending of blocks, 'recv' may return duplicated pos, so we
        // need a while-loop to make sure 'curr_pos' increases
        while self.curr_pos >= self.end_pos {
            let pos = self.pos_receiver.recv().unwrap();
            if pos == i64::MIN {
                self.curr_buf = None; // clear the partial buffer
                return (true, file_pos);
            }
            self.end_pos = pos;
            if self.curr_buf.is_none() {
                // re-fetch currBuf because it was cleared
                let buf = self
                    .entry_buffer
                    .get_and_remove_buf(self.curr_pos / BIG_BUF_SIZE_I64, -1);
                self.curr_buf = Some(buf);
            }
        }
        let (idx, offset) = (
            self.curr_pos / BIG_BUF_SIZE_I64,
            self.curr_pos % BIG_BUF_SIZE_I64,
        );
        let curr_buf = self.curr_buf.as_ref().unwrap();
        let size = EntryBz::get_entry_len(&curr_buf[offset as usize..]) as i64;
        self.curr_pos += size;
        if offset + size < BIG_BUF_SIZE_I64 {
            let entry_bz = EntryBz {
                bz: &curr_buf[offset as usize..(offset + size) as usize],
            };
            access(entry_bz);
            return (false, file_pos);
        }
        let remove_idx = idx - (self.buf_margin / BIG_BUF_SIZE) as i64 - 1;
        let next_buf = self.entry_buffer.get_and_remove_buf(idx + 1, remove_idx);
        self.scratch_pad.clear();
        let copied_length = BIG_BUF_SIZE - offset as usize;
        self.scratch_pad
            .extend_from_slice(&curr_buf[offset as usize..]);
        self.scratch_pad
            .extend_from_slice(&next_buf[..(size as usize - copied_length)]);
        self.curr_buf = Some(next_buf);
        let entry_bz = EntryBz {
            bz: &self.scratch_pad[..],
        };
        access(entry_bz);
        (false, file_pos)
    }

    pub fn read_extra_info(&self) -> (i64, u64, u64) {
        let compact_done_pos = self.pos_receiver.recv().unwrap();
        let compact_done_sn = self.pos_receiver.recv().unwrap() as u64;
        let sn_end = self.pos_receiver.recv().unwrap() as u64;
        (compact_done_pos, compact_done_sn, sn_end)
    }
}

#[cfg(test)]
mod test_entry_buffer {
    use qmdb_common::{
        def::BIG_BUF_SIZE,
        entryfile::{
            entry::{entry_to_bytes, Entry},
            readbuf::ReadBuf,
        },
    };
    use std::sync::atomic::Ordering;

    use crate::entrybuffer::{new, BIG_BUF_SIZE_I64};

    #[cfg(feature = "tee_cipher")]
    #[test]
    #[should_panic(expected = "Entry too large 100072 vs 16384")]
    fn test_append_panic() {
        let (mut writer, _reader) = new(0, 3 * BIG_BUF_SIZE);
        writer.append(
            &Entry {
                key: "key".as_bytes(),
                value: vec!["value"; 20000].concat().as_bytes(),
                next_key_hash: &[0xab; 32],
                version: 12345,
                serial_number: 99999,
            },
            &[],
        );
    }

    #[cfg(not(feature = "tee_cipher"))]
    #[test]
    #[should_panic(expected = "Entry too large 100056 vs 16384")]
    fn test_append_panic() {
        let (mut writer, _reader) = new(0, 3 * BIG_BUF_SIZE);
        writer.append(
            &Entry {
                key: "key".as_bytes(),
                value: vec!["value"; 20000].concat().as_bytes(),
                next_key_hash: &[0xab; 32],
                version: 12345,
                serial_number: 99999,
            },
            &[],
        );
    }

    #[test]
    fn test_append() {
        let (mut writer, reader) = new(0, 3 * BIG_BUF_SIZE);
        let value = vec!["value"; 2000].concat();
        let entry = Entry {
            key: "key".as_bytes(),
            value: value.as_bytes(),
            next_key_hash: &[0xab; 32],
            version: 12345,
            serial_number: 99999,
        };
        let deactived_sn_list = [1];

        let file_pos = writer.append(&entry, &deactived_sn_list);
        assert_eq!(0, file_pos);
        let total_size = entry.get_serialized_len(deactived_sn_list.len());
        assert_eq!(
            total_size,
            writer.entry_buffer.end.load(Ordering::SeqCst) as usize
        );

        let file_pos = writer.append(&entry, &deactived_sn_list);
        assert_eq!(total_size, file_pos as usize);
        assert_eq!(
            total_size * 2,
            writer.entry_buffer.end.load(Ordering::SeqCst) as usize
        );
        assert_eq!(reader.pos_receiver.recv().unwrap(), file_pos);
        assert_eq!(writer.entry_buffer.buf_map.len(), 1);
    }

    #[test]
    fn test_get_entry_bz_at() {
        let (mut writer, _reader) = new(0, 3 * BIG_BUF_SIZE);

        let mut buf = ReadBuf::new();
        // ---  if file_pos < self.start.load(Ordering::SeqCst) {
        let in_disk = writer.get_entry_bz_at(-1, &mut buf);
        assert!(in_disk);
        assert!(buf.is_empty());

        let value = vec!["value"; 2500].concat();
        let entry = Entry {
            key: "key".as_bytes(),
            value: value.as_bytes(),
            next_key_hash: &[0xab; 32],
            version: 12345,
            serial_number: 99999,
        };
        let deactived_sn_list = [1];
        let total_size: usize = entry.get_serialized_len(deactived_sn_list.len());
        let mut bz = vec![0u8; total_size];
        let entry_bytes = entry_to_bytes(&entry, &deactived_sn_list, &mut bz).bz;

        writer.append(&entry, &deactived_sn_list);

        // ---file_pos >= curr_buf_start && curr_buf.is_some()
        buf.clear();
        let in_disk = writer.get_entry_bz_at(0, &mut buf);
        assert!(!in_disk);
        assert_eq!(buf.as_entry_bz().bz, entry_bytes);

        writer.append(&entry, &deactived_sn_list);
        //----  if offset + size <= BIG_BUF_SIZE as i64
        buf.clear();
        let in_disk = writer.get_entry_bz_at(0, &mut buf);
        assert!(!in_disk);
        assert_eq!(buf.as_entry_bz().bz, entry_bytes);

        // --- last casew3
        writer.append(&entry, &deactived_sn_list);
        buf.clear();
        let in_disk = writer.get_entry_bz_at(total_size as i64, &mut buf);
        assert!(!in_disk);
        assert_eq!(buf.as_entry_bz().bz, entry_bytes);
    }

    #[test]
    fn test_end_block_and_read_extra_info() {
        let (mut writer, reader) = new(0, 3 * BIG_BUF_SIZE);
        let value = vec!["value"; 2500].concat();
        let entry = Entry {
            key: "key".as_bytes(),
            value: value.as_bytes(),
            next_key_hash: &[0xab; 32],
            version: 12345,
            serial_number: 99999,
        };
        let deactived_sn_list = [1];

        writer.append(&entry, &deactived_sn_list);
        writer.end_block(1, 2, 3);

        assert_eq!(writer.entry_buffer.buf_map.len(), 1);

        assert_eq!(
            reader.pos_receiver.recv().unwrap(),
            (entry.get_serialized_len(deactived_sn_list.len()) as i64)
        );
        assert_eq!(reader.pos_receiver.recv().unwrap(), i64::MIN);

        let (compact_done_pos, compact_done_sn, sn_end) = reader.read_extra_info();
        assert_eq!(compact_done_pos, 1);
        assert_eq!(compact_done_sn, 2);
        assert_eq!(sn_end, 3);
    }

    #[test]
    fn test_read_next_entry() {
        let (mut writer, mut reader) = new(0, BIG_BUF_SIZE);
        let value = vec!["value"; 2500].concat();
        let entry = Entry {
            key: "key".as_bytes(),
            value: value.as_bytes(),
            next_key_hash: &[0xab; 32],
            version: 12345,
            serial_number: 99999,
        };
        let deactived_sn_list = [1];
        let total_size: usize = entry.get_serialized_len(deactived_sn_list.len());
        let mut bz = vec![0u8; total_size];
        let entry_bytes = entry_to_bytes(&entry, &deactived_sn_list, &mut bz).bz;

        writer.append(&entry, &deactived_sn_list);
        writer.end_block(1, 2, 3);

        // ----  offset + size <= BIG_BUF_SIZE_I64
        let (end_of_block, file_pos) = reader.read_next_entry(|entry_bz| {
            assert_eq!(entry_bytes, entry_bz.bz);
        });
        assert!(!end_of_block);
        assert_eq!(file_pos, 0);
        assert_eq!(reader.entry_buffer.buf_map.len(), 1);

        // --- if pos == i64::MIN
        let (end_of_block, file_pos) = reader.read_next_entry(|_| {});
        assert!(end_of_block);
        assert_eq!(file_pos, total_size as i64);
        assert_eq!(reader.entry_buffer.buf_map.len(), 1);

        reader.read_extra_info();

        // ---- last case
        writer.append(&entry, &deactived_sn_list);
        writer.append(&entry, &deactived_sn_list);
        writer.append(&entry, &deactived_sn_list);
        writer.end_block(1, 2, 3);

        let (end_of_block, file_pos) = reader.read_next_entry(|entry_bz| {
            assert_eq!(entry_bytes, entry_bz.bz);
        });
        assert!(!end_of_block);
        assert_eq!(file_pos, total_size as i64);
        assert_eq!(reader.entry_buffer.buf_map.len(), 4);

        let (end_of_block, file_pos) = reader.read_next_entry(|entry_bz| {
            assert_eq!(entry_bytes, entry_bz.bz);
        });
        assert!(!end_of_block);
        assert_eq!(file_pos, (total_size * 2) as i64);
        assert_eq!(reader.entry_buffer.buf_map.len(), 4);

        let (end_of_block, file_pos) = reader.read_next_entry(|entry_bz| {
            assert_eq!(entry_bytes, entry_bz.bz);
        });

        // remove buf_map[0]
        assert!(!end_of_block);
        assert_eq!(file_pos, (total_size * 3) as i64);
        assert_eq!(reader.entry_buffer.buf_map.len(), 3);
    }

    #[test]
    fn test_threshold_size() {
        let start = BIG_BUF_SIZE_I64 - 12518; // total_size
        let (mut writer, mut reader) = new(start, BIG_BUF_SIZE);
        let value = vec!["value"; 2500].concat();
        let entry = Entry {
            key: "key".as_bytes(),
            value: value.as_bytes(),
            next_key_hash: &[0xab; 32],
            version: 12345,
            serial_number: 99999,
        };
        let deactived_sn_list = [1];
        let total_size: usize = entry.get_serialized_len(deactived_sn_list.len());
        let mut bz = vec![0u8; total_size];
        let entry_bytes = entry_to_bytes(&entry, &deactived_sn_list, &mut bz).bz;

        writer.append(&entry, &deactived_sn_list);
        writer.append(&entry, &deactived_sn_list);

        for i in 0..2 {
            let mut buf = ReadBuf::new();
            writer.get_entry_bz_at(start + i as i64 * total_size as i64, &mut buf);
            assert_eq!(buf.as_entry_bz().bz, entry_bytes);
        }

        writer.end_block(1, 2, 3);

        for _ in 0..2 {
            reader.read_next_entry(|entry_bz| {
                assert_eq!(entry_bytes, entry_bz.bz);
            });
        }
    }

    #[test]
    fn test_free_list() {
        let start = BIG_BUF_SIZE_I64 - 12518; // total_size
        let (mut writer, mut reader) = new(start, BIG_BUF_SIZE);
        let value = vec!["value"; 2500].concat();
        let entry = Entry {
            key: "key".as_bytes(),
            value: value.as_bytes(),
            next_key_hash: &[0xab; 32],
            version: 12345,
            serial_number: 99999,
        };
        let deactived_sn_list = [1];
        let total_size: usize = entry.get_serialized_len(deactived_sn_list.len());
        let mut bz = vec![0u8; total_size];
        let entry_bytes = entry_to_bytes(&entry, &deactived_sn_list, &mut bz).bz;

        for _ in 0..10 {
            writer.append(&entry, &deactived_sn_list);
        }

        writer.end_block(1, 2, 3);

        for _ in 0..10 {
            reader.read_next_entry(|entry_bz| {
                assert_eq!(entry_bytes, entry_bz.bz);
            });
        }

        let free_list = reader.entry_buffer.free_list.lock();
        assert_eq!(free_list.len(), 5);
    }
}
