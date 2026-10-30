//! The read-path for the WAL.

use super::{WAL_ENTRY_TAG_CLEAR, WAL_ENTRY_TAG_END, WAL_ENTRY_TAG_START, WAL_ENTRY_TAG_UPDATE};
use crate::{
    io::{self, PagePool, PAGE_SIZE},
    merkle::ElidedChildren,
    page_diff::PageDiff,
};
use anyhow::bail;
use std::{fs::File, io::Seek};

#[derive(Debug, PartialEq, Eq)]
pub enum WalEntry {
    Update {
        /// The unique identifier of the page being updated.
        page_id: [u8; 32],
        /// A bitmap where each bit indicates whether the node at the corresponding index was
        /// changed by this update.
        page_diff: PageDiff,
        /// Nodes that were changed by this update. The length of this array must be consistent with
        /// the number of ones in `page_diff`.
        changed_nodes: Vec<[u8; 32]>,
        /// Bitfield representing which child page has been elided.
        elided_children: ElidedChildren,
        /// The bucket index which is being updated.
        bucket: u64,
    },
    Clear {
        /// The bucket index which is being cleared.
        bucket: u64,
    },
}

pub struct WalBlobReader {
    wal: Vec<u8>,
    offset: usize,
    sync_seqn: u32,
}

impl WalBlobReader {
    /// Creates a new WAL blob reader.
    ///
    /// The `wal_fd` is expected to be positioned at the start of the WAL file. The file must be
    /// a multiple of the page size.
    pub fn new(page_pool: &PagePool, mut wal_fd: &File) -> anyhow::Result<Self> {
        let stat = wal_fd.metadata()?;
        let file_size = stat.len() as usize;
        if file_size % PAGE_SIZE != 0 {
            anyhow::bail!("WAL file size is not a multiple of the page size");
        }

        wal_fd.seek(std::io::SeekFrom::Start(0))?;

        // Read the entire WAL file into memory. We do it page-by-page because WAL fd is opened
        // with O_DIRECT flag, and that means we need to provide aligned buffers.
        let mut wal = Vec::with_capacity(file_size);
        let mut pn = 0;
        loop {
            let page = match io::read_page(page_pool, wal_fd, pn) {
                Ok(page) => page,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            };
            pn += 1;
            wal.extend_from_slice(&*page);
        }

        let mut reader = Self {
            wal,
            offset: 0,
            sync_seqn: 0,
        };
        reader.read_start()?;

        Ok(reader)
    }

    /// Get the sync sequence number of the WAL file.
    pub fn sync_seqn(&self) -> u32 {
        self.sync_seqn
    }

    /// Reads the next entry from the WAL file.
    ///
    /// Returns `None` if the end of the file is reached.
    pub fn read_entry(&mut self) -> anyhow::Result<Option<WalEntry>> {
        let entry_tag = self.read_byte()?;
        match entry_tag {
            WAL_ENTRY_TAG_END => Ok(None),
            WAL_ENTRY_TAG_CLEAR => {
                let bucket = self.read_u64()?;
                Ok(Some(WalEntry::Clear { bucket }))
            }
            WAL_ENTRY_TAG_UPDATE => {
                let page_id: [u8; 32] = self.read_buf()?;
                let page_diff: [u8; 16] = self.read_buf()?;
                let page_diff = PageDiff::from_bytes(page_diff)
                    .ok_or_else(|| anyhow::anyhow!("Invalid page diff"))?;

                let changed_count = page_diff.count();
                let mut changed_nodes = Vec::with_capacity(changed_count);
                for _ in 0..changed_count {
                    let node = self.read_buf::<32>()?;
                    changed_nodes.push(node);
                }

                let elided_children: [u8; 8] = self.read_buf()?;
                let elided_children = ElidedChildren::from_bytes(elided_children);
                let bucket = self.read_u64()?;

                Ok(Some(WalEntry::Update {
                    page_id,
                    page_diff,
                    changed_nodes,
                    elided_children,
                    bucket,
                }))
            }
            _ => bail!("unknown WAL entry tag: {entry_tag}"),
        }
    }

    fn read_start(&mut self) -> anyhow::Result<()> {
        let entry_tag = self.read_byte()?;
        if entry_tag == WAL_ENTRY_TAG_START {
            self.sync_seqn = self.read_u32()?;

            Ok(())
        } else {
            bail!("unexpected WAL entry tag at start: {entry_tag}");
        }
    }

    /// Reads a single byte from the WAL file.
    fn read_byte(&mut self) -> anyhow::Result<u8> {
        if self.offset >= self.wal.len() {
            bail!("Unexpected end of WAL file");
        }
        let byte = self.wal[self.offset];
        self.offset += 1;
        Ok(byte)
    }

    /// Reads a [u8; N] array from the WAL file.
    fn read_buf<const N: usize>(&mut self) -> anyhow::Result<[u8; N]> {
        if self.offset + N > self.wal.len() {
            bail!("Unexpected end of WAL file");
        }
        let array = self.wal[self.offset..self.offset + N]
            .try_into()
            .map_err(|_| anyhow::anyhow!("Failed to read [u8; {N}] from WAL file"))?;
        self.offset += N;
        Ok(array)
    }

    /// Reads a u64 from the WAL file in little-endian format.
    fn read_u64(&mut self) -> anyhow::Result<u64> {
        let buf = self.read_buf::<8>()?;
        Ok(u64::from_le_bytes(buf))
    }

    /// Reads a u32 from the WAL file in little-endian format.
    fn read_u32(&mut self) -> anyhow::Result<u32> {
        let buf = self.read_buf::<4>()?;
        Ok(u32::from_le_bytes(buf))
    }
}
