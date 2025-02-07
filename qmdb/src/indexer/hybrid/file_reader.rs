use aes_gcm::aead::AeadInPlace;
use aes_gcm::Aes256Gcm;
use byteorder::{ByteOrder, LittleEndian};
use parking_lot::RwLock;
use std::sync::Arc;

use super::def::{L, PAGE_BYTES, PAGE_SIZE, RD_BUF_SIZE};
use super::overlay::Overlay;
use super::tempfile::TempFile;
use crate::def::{NONCE_SIZE, TAG_SIZE};

pub struct FileReader {
    end_pos: usize, // stop reading when reaching 'end_pos'
    file: Option<Arc<RwLock<TempFile>>>,
    buf_begin: usize,  // 'buf' caches file content from 'buf_begin'
    buf_offset: usize, // we'll read from buf[buf_offset..] the next time
    buf: Box<[u8]>,
    curr_value: [u8; L], // it equals buf[buf_offset-L..buf_offset]
    is_empty: bool,      // no more can be read
    cipher: Arc<Option<Aes256Gcm>>,
}

impl FileReader {
    pub fn new(cipher: Arc<Option<Aes256Gcm>>) -> Self {
        let buf = vec![0u8; RD_BUF_SIZE].into_boxed_slice();
        Self {
            end_pos: 0,
            file: None,
            buf_begin: 0,
            buf_offset: 0,
            buf,
            curr_value: [0u8; L],
            is_empty: true,
            cipher,
        }
    }

    pub fn load_file(
        &mut self,
        file: Arc<RwLock<TempFile>>,
        start: usize,
        end: usize,
        overlay: &Overlay,
    ) {
        self.file = Some(file);
        self.buf_begin = start;
        self.end_pos = end;
        self.fill_buf();
        self.is_empty = false;
        self._find_not_erased_value(overlay);
    }

    fn fill_buf(&mut self) {
        let size = usize::min(self.buf.len(), self.end_pos - self.buf_begin);
        let f = self.file.as_ref().unwrap().read_arc();
        f.read_at(&mut self.buf[..size], self.buf_begin as u64);
        self.buf_offset = 0;
        self.decrypt_page();
        self.curr_value.copy_from_slice(&self.buf[..L]);
        self.buf_offset = L;
    }

    pub fn cur_record(&self) -> Option<&[u8; L]> {
        if self.buf_begin + self.buf_offset <= self.end_pos && !self.is_empty {
            return Some(&self.curr_value);
        }
        None
    }

    pub fn next(&mut self, overlay: &Overlay) {
        self._next();
        self._find_not_erased_value(overlay);
    }

    // ensure 'cur_record' is not erased
    fn _find_not_erased_value(&mut self, overlay: &Overlay) {
        loop {
            if let Some(e) = self.cur_record() {
                let v = LittleEndian::read_u64(&e[6..]) as i64 >> 16;
                if !overlay.is_erased_value(v) {
                    break; // find it
                }
                self._next();
            } else {
                break; // no valid data
            }
        }
    }

    fn _next(&mut self) {
        let margin = if cfg!(feature = "tee_cipher") {
            TAG_SIZE
        } else {
            0
        };
        if self.buf_begin + self.buf_offset + margin >= self.end_pos {
            self.is_empty = true;
            return; // cannot move next
        }
        if self.buf_offset < self.buf.len() {
            let (s, e) = (self.buf_offset, self.buf_offset + L);
            self.curr_value.copy_from_slice(&self.buf[s..e]);
            self.buf_offset += L;
            if self.buf_offset % PAGE_BYTES == (L * PAGE_SIZE) {
                self.buf_offset += TAG_SIZE;
                self.decrypt_page();
            }
        } else {
            self.buf_begin += self.buf.len();
            self.fill_buf();
        }
    }

    fn decrypt_page(&mut self) {
        let off_in_file = self.buf_begin + self.buf_offset;
        let mut size = self.end_pos - off_in_file;
        size = usize::min(PAGE_BYTES, size);
        let page = &mut self.buf[self.buf_offset..self.buf_offset + size];
        decrypt_page(page, off_in_file, &self.cipher);
    }
}

pub fn decrypt_page(page: &mut [u8], off_in_file: usize, cipher: &Option<Aes256Gcm>) {
    if cfg!(not(feature = "tee_cipher")) {
        return;
    }
    let cipher = (*cipher).as_ref().unwrap();
    let mut nonce_arr = [0u8; NONCE_SIZE];
    LittleEndian::write_u64(&mut nonce_arr[..8], off_in_file as u64);

    let tag_start = page.len() - TAG_SIZE;
    let mut tag = [0u8; TAG_SIZE];
    tag[..].copy_from_slice(&page[tag_start..]);

    let enc = &mut page[..tag_start];
    if let Err(e) = cipher.decrypt_in_place_detached(&nonce_arr.into(), b"", enc, &tag.into()) {
        panic!("{:?}", e)
    };
}

#[cfg(test)]
mod tests {

    use crate::entryfile::helpers::create_cipher;

    use super::*;

    #[cfg(not(feature = "tee_cipher"))]
    #[test]
    fn test_reader() {
        let mut file = TempFile::new("test_file_reader.tmp".to_string());
        let mut data = [0u8; 100];
        for i in 0..100 {
            data[i] = i as u8;
        }
        file.write(&data);
        let file = Arc::new(RwLock::new(file));
        let cipher = create_cipher();

        let mut reader = FileReader::new(Arc::new(cipher));
        let overlay = Overlay::new();
        reader.load_file(file, 10, 60, &overlay);
        assert_eq!(reader.buf_begin, 10);
        assert_eq!(reader.end_pos, 60);
        assert_eq!(reader.buf_offset, L);
        let record = reader.cur_record();
        assert!(record.is_some());
        let record = record.unwrap();
        assert_eq!(record, &data[10..10 + L]);
        let overlay = Overlay::new();
        // next
        reader.next(&overlay);
        let record = reader.cur_record();
        assert!(record.is_some());
        let record = record.unwrap();
        assert_eq!(record, &data[10 + L..10 + 2 * L]);
        assert_eq!(reader.buf_begin, 10);
        assert_eq!(reader.end_pos, 60);
        assert_eq!(reader.buf_offset, 2 * L);
        // next is bigger than current buf
        reader.buf = vec![0u8; 30].into_boxed_slice();
        reader.buf_offset = 31;
        reader.next(&overlay);
        assert_eq!(reader.buf_begin, 10 + 30);
        assert_eq!(reader.end_pos, 60);
        assert_eq!(reader.buf_offset, L);
        let record = reader.cur_record();
        assert!(record.is_some());
        let record = record.unwrap();
        assert_eq!(record, &data[40..40 + L]);
    }
}
