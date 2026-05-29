use aes_gcm::aead::AeadInPlace;
use aes_gcm::Aes256Gcm;
use byteorder::{ByteOrder, LittleEndian};
use parking_lot::RwLock;
use std::sync::Arc;

use super::def::{L, PAGE_BYTES, PAGE_SIZE, WR_BUF_SIZE};
use super::index_cache::COUNTERS;
use super::tempfile::TempFile;
use qmdb_common::def::{NONCE_SIZE, TAG_SIZE};

pub struct FileWriter {
    file: Arc<RwLock<TempFile>>,
    buf: Vec<u8>,
    record_count: usize,
    file_size: usize,
    cipher: Arc<Option<Aes256Gcm>>,
}

impl FileWriter {
    pub fn new(file: Arc<RwLock<TempFile>>, cipher: Arc<Option<Aes256Gcm>>) -> Self {
        Self {
            file,
            buf: Vec::with_capacity(WR_BUF_SIZE),
            record_count: 0,
            file_size: 0,
            cipher,
        }
    }

    pub fn get_name(&self) -> String {
        let guard = self.file.read();
        guard.get_name()
    }

    pub fn get_file_size(&self) -> usize {
        self.file_size
    }

    pub fn get_tmp_file(&self) -> Arc<RwLock<TempFile>> {
        self.file.clone()
    }

    // load a new file and start writing to it
    pub fn load_file(&mut self, file: Arc<RwLock<TempFile>>) {
        self.file = file;
        self.file_size = 0;
    }

    pub fn write(&mut self, e: &[u8; L], first_k_list: &mut (Vec<u64>, Vec<u32>)) {
        if self.buf.len() == WR_BUF_SIZE {
            self._flush();
        }
        self.buf.extend_from_slice(&e[..]);
        self.file_size += L;
        if self.buf.len() % PAGE_BYTES == L * PAGE_SIZE {
            self.encrypt_page();
        }
        if self.record_count % PAGE_SIZE == 0 {
            let k = LittleEndian::read_u64(&e[..8]);
            let n = self.record_count / PAGE_SIZE;
            if n % 4 == 0 {
                first_k_list.0.push(k);
            } else {
                first_k_list.1.push((k >> 32) as u32);
            }
        }
        self.record_count += 1;
    }

    // flush buf and stop writing to the current file
    pub fn flush(&mut self) {
        self.encrypt_page();
        self._flush();
        self.record_count = 0;
    }

    fn _flush(&mut self) {
        let mut guard = self.file.write_arc();
        guard.write(&self.buf[..]);
        COUNTERS.incr_wr_bytes(self.buf.len());
        self.buf.clear();
    }

    pub fn encrypt_page(&mut self) {
        if cfg!(not(feature = "tee_cipher")) {
            return;
        }
        let cipher = (*self.cipher).as_ref().unwrap();

        let len = self.buf.len();
        let page_start = (len / PAGE_BYTES) * PAGE_BYTES;
        if page_start == len {
            return;
        }
        let off_in_file = self.file_size - len + page_start;
        let mut nonce_arr = [0u8; NONCE_SIZE];
        LittleEndian::write_u64(&mut nonce_arr[..8], off_in_file as u64);

        let bz = &mut self.buf[page_start..];
        match cipher.encrypt_in_place_detached(&nonce_arr.into(), b"", bz) {
            Err(err) => panic!("{}", err),
            Ok(tag) => {
                self.buf.extend_from_slice(&tag[..]);
                self.file_size += TAG_SIZE;
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_temp_file(name: &str) -> Arc<RwLock<TempFile>> {
        Arc::new(RwLock::new(TempFile::new(name.to_string())))
    }

    fn read_all(file: Arc<RwLock<TempFile>>) -> Vec<u8> {
        let mut buf = vec![0u8; 1024];
        loop {
            let n = file.read_arc().read_at(&mut buf, 0);
            if n < buf.len() {
                buf.truncate(n);
                break;
            } else {
                buf.resize(buf.len() * 2, 0);
            }
        }
        buf
    }

    #[test]
    fn test_new_file_writer() {
        let file = create_temp_file("test_new_file_writer.tmp");
        let writer = FileWriter::new(file.clone(), Arc::new(None));

        assert_eq!(writer.buf.capacity(), WR_BUF_SIZE);
        assert_eq!(writer.record_count, 0);
        assert_eq!(writer.file_size, 0);
    }

    #[test]
    fn test_load_file() {
        let file1 = create_temp_file("test_load_file1.tmp");
        let mut writer = FileWriter::new(file1, Arc::new(None));

        let file2 = create_temp_file("test_load_file2.tmp");
        writer.load_file(file2);

        assert_eq!(writer.file_size, 0);
    }

    #[cfg(not(feature = "tee_cipher"))]
    #[test]
    fn test_write_and_flush() {
        let file = create_temp_file("test_write_and_flush.tmp");
        let mut writer = FileWriter::new(file.clone(), Arc::new(None));
        let mut first_k_list = (Vec::new(), Vec::new());

        let e1 = [1u8; L];
        let e2 = [2u8; L];

        writer.write(&e1, &mut first_k_list);
        writer.write(&e2, &mut first_k_list);
        assert_eq!("", hex::encode(read_all(file.clone())));
        assert_eq!(writer.file_size, 2 * L);
        assert_eq!(writer.record_count, 2);

        writer.flush();
        assert_eq!(
            format!("{}{}", hex::encode(e1), hex::encode(e2)),
            hex::encode(read_all(file.clone()))
        );
        assert_eq!(writer.file_size, 2 * L);
        assert_eq!(writer.record_count, 0);
    }

    #[cfg(not(feature = "tee_cipher"))]
    #[test]
    fn test_write_multiple_pages() {
        let file = create_temp_file("test_write_multiple_pages.tmp");
        let mut writer = FileWriter::new(file.clone(), Arc::new(None));
        let mut first_k_list = (Vec::new(), Vec::new());

        for i in 0..(PAGE_SIZE * 4 + 1) {
            let mut e = [0u8; L];
            LittleEndian::write_u32(&mut e[..4], i as u32 * 256);
            LittleEndian::write_u32(&mut e[4..], i as u32);
            writer.write(&e, &mut first_k_list);
        }
        writer.flush();

        assert_eq!(first_k_list.0.len(), 2);
        assert_eq!(first_k_list.1.len(), 3);
        assert_eq!(first_k_list.0[0], 0);
        assert_eq!(first_k_list.0[1], (128u64 << 32) + 128u64 * 256);
        assert_eq!(first_k_list.1[0], 32u32);
        assert_eq!(first_k_list.1[1], 64u32);
        assert_eq!(first_k_list.1[2], 96u32);
    }

    #[cfg(not(feature = "tee_cipher"))]
    #[test]
    fn test_write_buffer_overflow() {
        let file = create_temp_file("test_write_buffer_overflow.tmp");
        let mut writer = FileWriter::new(file.clone(), Arc::new(None));
        let mut first_k_list = (Vec::new(), Vec::new());

        let e = [1u8; L];
        for _ in 0..WR_BUF_SIZE / L + 1 {
            writer.write(&e, &mut first_k_list);
        }

        let content = read_all(file.clone());
        assert_eq!(content.len(), WR_BUF_SIZE);

        writer.flush();
        let content = read_all(file.clone());
        assert_eq!(content.len(), WR_BUF_SIZE + L);
    }
}
