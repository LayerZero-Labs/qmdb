use crate::{def::DEFAULT_ENTRY_SIZE, entryfile::entry::EntryBz};
use smallvec::SmallVec;

type ReadBufBz = SmallVec<[u8; DEFAULT_ENTRY_SIZE]>;

#[derive(Debug)]
#[repr(align(8))]
pub struct ReadBuf(ReadBufBz);

impl ReadBuf {
    pub fn new() -> Self {
        Self(ReadBufBz::new())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.0.as_mut_slice()
    }

    pub fn as_entry_bz(&self) -> EntryBz {
        EntryBz {
            bz: self.as_slice(),
        }
    }

    pub fn extend_from_slice(&mut self, other: &[u8]) {
        assert!(
            !other.is_empty(),
            "ReadBuf::extend_from_slice: other.len()={}",
            other.len()
        );

        self.0.extend_from_slice(other);
    }

    pub fn initialize_from(&mut self, other: &[u8]) {
        assert!(
            self.is_empty(),
            "ReadBuf::initialize_from: self.len()={}",
            self.len()
        );

        self.extend_from_slice(other);
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn resize(&mut self, size: usize) {
        self.0.resize(size, 0);
    }

    fn initialize_within(&mut self, src: usize, len: usize) {
        self.0.copy_within(src..src + len, 0);
        self.0.truncate(len);
    }

    pub fn initialize_from_entry_value(&mut self) {
        let entry_bz = self.as_entry_bz();
        let (start, len) = entry_bz.get_value_start_and_len();
        self.initialize_within(start, len);
    }
}
