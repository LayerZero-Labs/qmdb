use nomt_core::page_id::PageId;

use super::{
    meta::{self, Meta},
    DirtyPage, Shared,
};
use crate::{beatree, bitbox, options::PanicOnSyncMode, page_cache::PageCache, rollback};

pub struct Sync {
    pub(crate) sync_seqn: u32,
    pub(crate) bitbox_num_pages: u32,
    pub(crate) bitbox_seed: [u8; 16],
    pub(crate) panic_on_sync: Option<PanicOnSyncMode>,
}

impl Sync {
    pub fn new(
        sync_seqn: u32,
        bitbox_num_pages: u32,
        bitbox_seed: [u8; 16],
        panic_on_sync: Option<PanicOnSyncMode>,
    ) -> Self {
        Self {
            sync_seqn,
            bitbox_num_pages,
            bitbox_seed,
            panic_on_sync,
        }
    }

    pub fn sync(
        &mut self,
        shared: &Shared,
        value_tx: impl IntoIterator<Item = (beatree::Key, beatree::ValueChange)> + Send + 'static,
        bitbox: bitbox::DB,
        beatree: beatree::Tree,
        rollback: Option<rollback::Rollback>,
        page_cache: PageCache,
        updated_pages: impl IntoIterator<Item = (PageId, DirtyPage)> + Send + 'static,
    ) -> anyhow::Result<()> {
        let sync_seqn = self.sync_seqn + 1;

        let mut bitbox_sync = bitbox.sync();
        let mut beatree_sync = beatree.sync();
        let mut rollback_sync = rollback.map(|rollback| rollback.sync());

        bitbox_sync.begin_sync(sync_seqn, page_cache, updated_pages);
        beatree_sync.begin_sync(value_tx);
        let (rollback_start_live, rollback_end_live) = match rollback_sync {
            Some(ref mut rollback) => rollback.begin_sync(),
            None => (0, 0),
        };

        bitbox_sync.wait_pre_meta()?;
        let beatree_meta_wd = beatree_sync.wait_pre_meta()?;

        if let Some(PanicOnSyncMode::PostWal) = self.panic_on_sync {
            panic!("panic_on_sync is true (post-wal)")
        }

        let new_meta = Meta {
            magic: meta::MAGIC,
            version: meta::VERSION,
            ln_freelist_pn: beatree_meta_wd.ln_freelist_pn,
            ln_bump: beatree_meta_wd.ln_bump,
            bbn_freelist_pn: beatree_meta_wd.bbn_freelist_pn,
            bbn_bump: beatree_meta_wd.bbn_bump,
            sync_seqn,
            bitbox_num_pages: self.bitbox_num_pages,
            bitbox_seed: self.bitbox_seed,
            rollback_start_live,
            rollback_end_live,
        };
        Meta::write(&shared.io_pool.page_pool(), &shared.meta_fd, &new_meta)?;
        self.sync_seqn += 1;

        if let Some(PanicOnSyncMode::PostMeta) = self.panic_on_sync {
            panic!("panic_on_sync is true (post-meta)");
        }

        if let Some(ref mut rollback) = rollback_sync {
            rollback.post_meta();
        }

        bitbox_sync.post_meta(shared.io_pool.make_handle())?;
        beatree_sync.post_meta();

        if let Some(ref rollback) = rollback_sync {
            rollback.wait_post_meta()?;
        }
        Ok(())
    }
}
