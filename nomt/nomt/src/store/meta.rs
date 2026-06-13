/// The utility functions for handling the metadata file.
use anyhow::Result;
use std::fs::File;
use std::os::unix::fs::FileExt as _;

use crate::io::{self, PagePool};

pub(crate) const MAGIC: [u8; 4] = *b"NOMT";
pub(crate) const VERSION: u32 = 1;
pub(crate) const META_SIZE: usize = 64;

/// This data structure describes the state of the btree.
#[derive(Clone, Debug)]
pub struct Meta {
    /// The magic number of the metadata file.
    pub magic: [u8; 4],
    /// The version of the database format.
    pub version: u32,
    /// The page number of the head of the freelist of the leaf storage file. 0 means the freelist
    /// is empty.
    pub ln_freelist_pn: u32,
    /// The next page available for allocation in the LN storage file.
    ///
    /// Since the first page is reserved, this is always more than 1.
    pub ln_bump: u32,
    /// The page number of the head of the freelist of the bbn storage file. 0 means the freelist
    /// is empty.
    pub bbn_freelist_pn: u32,
    /// The next page available for allocation in the BBN storage file.
    ///
    /// Since the first page is reserved, this is always more than 1.
    pub bbn_bump: u32,
    /// The sequence number of the last sync.
    ///
    /// 0 means there were no syncs and the DB is empty.
    pub sync_seqn: u32,
    /// The number of pages in the bitbox store.
    pub bitbox_num_pages: u32,
    /// The random seed used for populating the hash-table in a unique way.
    pub bitbox_seed: [u8; 16],
    /// The first live record ID in the rollback seglog.
    pub rollback_start_live: u64,
    /// The last live record ID in the rollback seglog.
    pub rollback_end_live: u64,
}

impl Meta {
    /// Returns a newly initialized [`Meta`] instance with the given bitbox seed and number of
    /// pages.
    pub fn create_new(bitbox_seed: [u8; 16], bitbox_num_pages: u32) -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
            ln_freelist_pn: 0,
            ln_bump: 1,
            bbn_freelist_pn: 0,
            bbn_bump: 1,
            sync_seqn: 0,
            bitbox_num_pages,
            bitbox_seed,
            rollback_start_live: 0,
            rollback_end_live: 0,
        }
    }

    pub fn encode_to(&self, buf: &mut [u8]) {
        assert!(buf.len() >= META_SIZE);
        buf[0..4].copy_from_slice(&self.magic);
        buf[4..8].copy_from_slice(&self.version.to_le_bytes());
        buf[8..12].copy_from_slice(&self.ln_freelist_pn.to_le_bytes());
        buf[12..16].copy_from_slice(&self.ln_bump.to_le_bytes());
        buf[16..20].copy_from_slice(&self.bbn_freelist_pn.to_le_bytes());
        buf[20..24].copy_from_slice(&self.bbn_bump.to_le_bytes());
        buf[24..28].copy_from_slice(&self.sync_seqn.to_le_bytes());
        buf[28..32].copy_from_slice(&self.bitbox_num_pages.to_le_bytes());
        buf[32..48].copy_from_slice(&self.bitbox_seed);
        buf[48..56].copy_from_slice(&self.rollback_start_live.to_le_bytes());
        buf[56..64].copy_from_slice(&self.rollback_end_live.to_le_bytes());
    }

    pub fn decode(buf: &[u8]) -> Self {
        assert!(buf.len() >= META_SIZE);
        let magic = buf[0..4].try_into().unwrap();
        let version = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let ln_freelist_pn = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        let ln_bump = u32::from_le_bytes(buf[12..16].try_into().unwrap());
        let bbn_freelist_pn = u32::from_le_bytes(buf[16..20].try_into().unwrap());
        let bbn_bump = u32::from_le_bytes(buf[20..24].try_into().unwrap());
        let sync_seqn = u32::from_le_bytes(buf[24..28].try_into().unwrap());
        let bitbox_num_pages = u32::from_le_bytes(buf[28..32].try_into().unwrap());
        let bitbox_seed = buf[32..48].try_into().unwrap();
        let rollback_start_live = u64::from_le_bytes(buf[48..56].try_into().unwrap());
        let rollback_end_live = u64::from_le_bytes(buf[56..64].try_into().unwrap());
        Self {
            magic,
            version,
            ln_freelist_pn,
            ln_bump,
            bbn_freelist_pn,
            bbn_bump,
            sync_seqn,
            bitbox_num_pages,
            bitbox_seed,
            rollback_start_live,
            rollback_end_live,
        }
    }

    pub fn validate(&self) -> Result<()> {
        let mut errors = Vec::new();
        if self.magic != MAGIC {
            errors.push(format!("invalid magic: {:?}", self.magic));
        }
        if self.version < 1 {
            errors.push(format!("invalid version: 0"));
        } else if self.version > VERSION {
            errors.push(format!(
                "DB manifest version ({}) is newer than supported ({})",
                self.version, VERSION
            ));
        }
        let is_rollback_start_live_nil = self.rollback_start_live == 0;
        let is_rollback_end_live_nil = self.rollback_end_live == 0;
        if is_rollback_start_live_nil ^ is_rollback_end_live_nil {
            errors.push(format!(
                "rollback_start_live and rollback_end_live must both be nil or both be non-nil, got start: {}, end: {}",
                self.rollback_start_live,
                self.rollback_end_live,
            ));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            // Collect all the errors and return them in a single anyhow error.
            Err(anyhow::anyhow!(errors.join("\n")))
        }
    }

    pub fn read(page_pool: &PagePool, fd: &File) -> std::io::Result<Self> {
        let page = io::read_page(page_pool, fd, 0)?;
        let meta = Meta::decode(&page[..META_SIZE]);
        Ok(meta)
    }

    pub fn write(page_pool: &PagePool, fd: &File, meta: &Meta) -> std::io::Result<()> {
        let mut page = page_pool.alloc_fat_page();
        meta.encode_to(&mut page.as_mut()[..META_SIZE]);
        fd.write_all_at(&page[..], 0)?;
        fd.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Meta, META_SIZE};
    use quickcheck::quickcheck;

    impl quickcheck::Arbitrary for Meta {
        fn arbitrary(g: &mut quickcheck::Gen) -> Self {
            Meta {
                magic: u32::arbitrary(g).to_le_bytes(),
                version: u32::arbitrary(g),
                ln_freelist_pn: u32::arbitrary(g),
                ln_bump: u32::arbitrary(g),
                bbn_freelist_pn: u32::arbitrary(g),
                bbn_bump: u32::arbitrary(g),
                sync_seqn: u32::arbitrary(g),
                bitbox_num_pages: u32::arbitrary(g),
                bitbox_seed: u128::arbitrary(g).to_le_bytes(),
                rollback_start_live: u64::arbitrary(g),
                rollback_end_live: u64::arbitrary(g),
            }
        }
    }

    quickcheck! {
        fn encode_decode_roundtrip(meta: Meta) -> bool {
            let mut buf = vec![0u8; META_SIZE];
            meta.encode_to(&mut buf);
            let decoded = Meta::decode(&buf);

            meta.magic == decoded.magic &&
            meta.version == decoded.version &&
            meta.ln_freelist_pn == decoded.ln_freelist_pn &&
            meta.ln_bump == decoded.ln_bump &&
            meta.bbn_freelist_pn == decoded.bbn_freelist_pn &&
            meta.bbn_bump == decoded.bbn_bump &&
            meta.sync_seqn == decoded.sync_seqn &&
            meta.bitbox_num_pages == decoded.bitbox_num_pages &&
            meta.bitbox_seed == decoded.bitbox_seed &&
            meta.rollback_start_live == decoded.rollback_start_live &&
            meta.rollback_end_live == decoded.rollback_end_live
        }
    }
}
