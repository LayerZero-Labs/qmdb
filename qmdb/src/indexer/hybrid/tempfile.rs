use std::fs::remove_file;

#[cfg(feature = "in_sp1")]
use hpfile::file::File;
#[cfg(not(feature = "in_sp1"))]
use std::{fs::File, io::Write, os::unix::fs::FileExt};

// #![allow(dead_code)]

pub struct TempFile {
    file: File,
    fname: String,
}

impl TempFile {
    pub fn new(fname: String) -> Self {
        match File::options()
            .create(true)
            .read(true)
            .write(true)
            .open(fname.clone())
        {
            Ok(file) => Self { file, fname },
            Err(_) => panic!("Fail to open file: {}", fname),
        }
    }

    pub fn get_name(&self) -> String {
        self.fname.clone()
    }

    pub fn read_at(&self, buf: &mut [u8], off: u64) -> usize {
        if buf.is_empty() {
            return 0;
        }
        self.file.read_at(buf, off).unwrap()
    }

    pub fn write(&mut self, buf: &[u8]) {
        if buf.is_empty() {
            return;
        }
        self.file.write(buf).unwrap();
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if remove_file(self.fname.clone()).is_err() {
            panic!("Fail to remove file: {}", self.fname);
        }
    }
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    #[test]
    #[serial]
    fn test_tempfile_new() {
        let fname = "./test_file.txt".to_string();
        let temp_file = TempFile::new(fname.clone());
        assert_eq!(temp_file.fname, fname);
    }

    #[test]
    #[serial]
    fn test_tempfile_write_and_read_at() {
        let fname = "./test_file.txt".to_string();
        let mut temp_file = TempFile::new(fname.clone());
        {
            let mut buf = [0u8; 5];
            temp_file.write(b"Hello");
            let bytes_read = temp_file.read_at(&mut buf, 0);
            assert_eq!(bytes_read, 5);
            assert_eq!(&buf, b"Hello");
        }
        {
            let buf = b"World";
            temp_file.write(buf);
            let mut read_buf = [0u8; 5];
            let bytes_read = temp_file.read_at(&mut read_buf, 5);
            assert_eq!(bytes_read, 5);
            assert_eq!(&read_buf, buf);
        }
    }

    #[test]
    #[serial]
    fn test_tempfile_drop() {
        let fname = "./test_file.txt".to_string();
        {
            let _temp_file = TempFile::new(fname.clone());
        }
        assert!(!std::path::Path::new(&fname).exists());
    }
}
