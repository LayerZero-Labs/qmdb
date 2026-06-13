//! The store module abstracts the storage layer of Nomt.
//!
//! It provides storage facilities for the binary trie pages based on a hash-table (bitbox) and the
//! b-tree key-value storage (beatree).

use crate::{
    beatree, bitbox,
    io::{self, page_pool::FatPage, IoPool, PagePool},
    page_cache::{Page, PageCache},
    page_diff::PageDiff,
    rollback::Rollback,
    ValueHasher,
};
use flock::Flock;
use meta::Meta;
use nomt_core::{page_id::PageId, trie::KeyPath};
use parking_lot::Mutex;
use std::{
    fs::{File, OpenOptions},
    sync::{atomic::AtomicBool, Arc},
};

#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt as _;

pub use self::page_loader::{PageLoad, PageLoader};
pub use bitbox::{BucketIndex, HashTableUtilization, SharedMaybeBucketIndex};

mod flock;
mod meta;
mod page_loader;
mod sync;

/// This is a lightweight handle and can be cloned cheaply.
#[derive(Clone)]
pub struct Store {
    shared: Arc<Shared>,
    sync: Arc<Mutex<sync::Sync>>,
}

struct Shared {
    values: beatree::Tree,
    pages: bitbox::DB,
    rollback: Option<Rollback>,
    io_pool: IoPool,
    meta_fd: File,
    flock: Option<flock::Flock>,
    poisoned: AtomicBool,

    // Retained for the lifetime of the store.
    _db_dir_fd: Arc<File>,
}

impl Store {
    /// Open the store with the provided `Options`.
    pub fn open(o: &crate::Options, page_pool: PagePool) -> anyhow::Result<Self> {
        let db_dir_fd;
        let flock;

        let should_create = !o.path.exists() || is_directory_empty(o.path.as_path())?;
        if should_create {
            // NB: note TOCTOU here. Deemed acceptable for this case.
            (db_dir_fd, flock) = create(&page_pool, &o)?;
        } else {
            let mut options = OpenOptions::new();
            options.read(true);
            db_dir_fd = options.open(&o.path)?;
            flock = flock::Flock::lock(&o.path, ".lock")?;
        }
        let db_dir_fd = Arc::new(db_dir_fd);

        cfg_if::cfg_if! {
            if #[cfg(target_os = "linux")] {
                // O_DIRECT is not supported on tmpfs.
                let o_direct: bool;
                match crate::sys::linux::fs_check(&db_dir_fd) {
                    Ok(fsck) => {
                        o_direct = !fsck.is_tmpfs();
                    },
                    Err(_) => {
                        o_direct = false;
                    },
                }
            }
        }

        let io_pool = io::start_io_pool(o.io_workers, page_pool.clone());

        let meta_fd = {
            let mut options = OpenOptions::new();
            options.read(true).write(true);
            #[cfg(target_os = "linux")]
            if o_direct {
                options.custom_flags(libc::O_DIRECT);
            }
            options.open(&o.path.join("meta"))?
        };

        let ln_fd = {
            let mut options = OpenOptions::new();
            options.read(true).write(true);
            #[cfg(target_os = "linux")]
            if o_direct {
                options.custom_flags(libc::O_DIRECT);
            }
            Arc::new(options.open(&o.path.join("ln"))?)
        };
        let bbn_fd = {
            let mut options = OpenOptions::new();
            options.read(true).write(true);
            #[cfg(target_os = "linux")]
            if o_direct {
                options.custom_flags(libc::O_DIRECT);
            }
            Arc::new(options.open(&o.path.join("bbn"))?)
        };
        let ht_fd = {
            let mut options = OpenOptions::new();
            options.read(true).write(true);
            #[cfg(target_os = "linux")]
            if o_direct {
                options.custom_flags(libc::O_DIRECT);
            }
            options.open(&o.path.join("ht"))?
        };
        let wal_fd = {
            let options = &mut OpenOptions::new();
            options.read(true).write(true);
            #[cfg(target_os = "linux")]
            if o_direct {
                options.custom_flags(libc::O_DIRECT);
            }
            options.open(&o.path.join("wal"))?
        };

        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd as _;
            unsafe {
                libc::fcntl(meta_fd.as_raw_fd(), libc::F_NOCACHE, 1);
                libc::fcntl(ln_fd.as_raw_fd(), libc::F_NOCACHE, 1);
                libc::fcntl(bbn_fd.as_raw_fd(), libc::F_NOCACHE, 1);
                libc::fcntl(ht_fd.as_raw_fd(), libc::F_NOCACHE, 1);
                libc::fcntl(wal_fd.as_raw_fd(), libc::F_NOCACHE, 1);
            }
        }

        let meta = meta::Meta::read(&page_pool, &meta_fd)?;
        meta.validate()?;
        let values = beatree::Tree::open(
            page_pool.clone(),
            &io_pool,
            meta.ln_freelist_pn,
            meta.bbn_freelist_pn,
            meta.ln_bump,
            meta.bbn_bump,
            bbn_fd,
            ln_fd,
            o.commit_concurrency,
            o.leaf_cache_size,
        )?;
        let pages = bitbox::DB::open(
            meta.sync_seqn,
            meta.bitbox_num_pages,
            meta.bitbox_seed,
            page_pool.clone(),
            ht_fd,
            wal_fd,
        )?;
        let rollback = o
            .rollback
            .then(|| {
                Rollback::read(
                    o.max_rollback_log_len,
                    o.path.clone(),
                    Arc::clone(&db_dir_fd),
                    meta.rollback_start_live,
                    meta.rollback_end_live,
                )
            })
            .transpose()?;
        Ok(Self {
            sync: Arc::new(Mutex::new(sync::Sync::new(
                meta.sync_seqn,
                meta.bitbox_num_pages,
                meta.bitbox_seed,
                o.panic_on_sync,
            ))),
            shared: Arc::new(Shared {
                rollback,
                values,
                pages,
                io_pool,
                _db_dir_fd: db_dir_fd,
                meta_fd,
                flock: Some(flock),
                poisoned: false.into(),
            }),
        })
    }

    pub fn is_poisoned(&self) -> bool {
        self.shared
            .poisoned
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn sync_seqn(&self) -> u32 {
        self.sync.lock().sync_seqn
    }

    /// Returns a handle to the rollback object. `None` if the rollback feature is not enabled.
    pub fn rollback(&self) -> Option<&Rollback> {
        self.shared.rollback.as_ref()
    }

    /// Loads the flat value stored under the given key.
    pub fn load_value(&self, key: KeyPath) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.shared.values.lookup(key))
    }

    /// Loads the given page, blocking the current thread.
    pub fn load_page(&self, page_id: PageId) -> anyhow::Result<Option<(FatPage, BucketIndex)>> {
        let page_loader = self.page_loader();
        let io_handle = self.io_pool().make_handle();
        let mut page_load = page_loader.start_load(page_id);
        loop {
            if !page_loader.probe(&mut page_load, &io_handle, 0) {
                return Ok(None);
            }

            let completion = io_handle.recv()?;
            completion.result?;
            assert_eq!(completion.command.user_data, 0);

            // UNWRAP: page loader always submits a `Read` command that yields a fat page.
            let page = completion.command.kind.unwrap_buf();

            if let Some(res) = page_load.try_complete(page) {
                return Ok(Some(res));
            }
        }
    }

    /// Creates a new [`beatree::ReadTransaction`]. `sync` will be blocked until this is dropped.
    pub fn read_transaction(&self) -> beatree::ReadTransaction {
        self.shared.values.read_transaction()
    }

    /// Creates a new [`PageLoader`].
    pub fn page_loader(&self) -> PageLoader {
        let page_loader = bitbox::PageLoader::new(&self.shared.pages);
        PageLoader { inner: page_loader }
    }

    /// Access the underlying IoPool.
    pub fn io_pool(&self) -> &IoPool {
        &self.shared.io_pool
    }

    /// Get the current hash-table bucket counts.
    pub fn hash_table_utilization(&self) -> HashTableUtilization {
        self.shared.pages.utilization()
    }

    /// Create a new raw value transaction to be applied against this database.
    pub fn new_value_tx(&self) -> ValueTransaction {
        ValueTransaction { batch: Vec::new() }
    }

    /// Atomically apply the given transaction.
    ///
    /// After this function returns, accessor methods such as [`Self::load_page`] will return the
    /// updated values.
    pub fn commit(
        &self,
        value_tx: impl IntoIterator<Item = (beatree::Key, beatree::ValueChange)> + Send + 'static,
        page_cache: PageCache,
        updated_pages: impl IntoIterator<Item = (PageId, DirtyPage)> + Send + 'static,
    ) -> anyhow::Result<()> {
        let mut sync = self.sync.lock();

        if self
            .shared
            .poisoned
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            anyhow::bail!("Store is poisoned due to prior error");
        }

        if let Err(e) = sync.sync(
            &self.shared,
            value_tx,
            self.shared.pages.clone(),
            self.shared.values.clone(),
            self.shared.rollback.clone(),
            page_cache,
            updated_pages,
        ) {
            self.shared
                .poisoned
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return Err(e);
        }
        Ok(())
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // `Shared` is dropped, meaning no more commits are expected. Therefore, we can
        // wait for IO workers to finish their work and then drop the flock. The order is important
        // because we need to ensure that the flock is only dropped after the IO workers are done.
        // Otherwise, these IO workers might still be writing to the files while another process
        // acquired the flock.
        self.io_pool.shutdown();
        drop(self.flock.take());
    }
}

/// An atomic transaction on raw key/value pairs to be applied against the store
/// with [`Store::commit`].
pub struct ValueTransaction {
    batch: Vec<(beatree::Key, beatree::ValueChange)>,
}

impl ValueTransaction {
    /// Write a value to flat storage.
    pub fn write_value<T: ValueHasher>(&mut self, path: beatree::Key, value: Option<Vec<u8>>) {
        self.batch
            .push((path, beatree::ValueChange::from_option::<T>(value)))
    }

    /// Iterate all the changed values.
    pub fn into_iter(self) -> impl Iterator<Item = (beatree::Key, beatree::ValueChange)> {
        self.batch.into_iter()
    }
}

/// Information about the bucket associated with a page.
///
/// This is either a firmly known bucket index or a pending bucket index which will be determined
/// when the overlay is written.
///
/// The only overhead for non-fresh pages is the overhead of an enum. For fresh pages, there is an
/// allocation and atomic overhead.
#[derive(Clone)]
pub enum BucketInfo {
    /// The bucket index is known.
    Known(BucketIndex),
    /// The page is fresh and there are no dependents (i.e. overlays) which would require the result
    /// of the page allocation.
    FreshWithNoDependents,
    /// The bucket index is either fresh or dependent on the bucket allocation of a fresh page
    /// within an earlier overlay.
    ///
    /// This variant is specifically needed for storage overlays.
    /// When a fresh page is first inserted into an overlay, its bucket is pending. This state is
    /// shared with all subsequent overlays. The bucket is determined when the overlay first
    /// containing the fresh page is committed. The allocated page will automatically propagate to
    /// all dependent overlays.
    ///
    /// Without this shared state, it would be possible to lose track of the allocated bucket index
    /// from one overlay to the next.
    FreshOrDependent(SharedMaybeBucketIndex),
}

/// A dirty page to be written to the store.
#[derive(Clone)]
pub struct DirtyPage {
    /// The (frozen) page.
    pub page: Page,
    /// The diff between this page and the last revision.
    pub diff: PageDiff,
    /// The bucket info associated with the page.
    pub bucket: BucketInfo,
}

/// Creates and initializes a new empty database at the specified path.
///
/// This function:
/// - Creates all necessary directories along the path
/// - Locks the directory
/// - Initializes required database files
/// - Returns a file descriptor for the database directory along with a lock handle.
///
/// The database directory must not exist when calling this function.
fn create(page_pool: &PagePool, o: &crate::Options) -> anyhow::Result<(File, Flock)> {
    // Create the directory and its parent directories.
    std::fs::create_dir_all(&o.path)?;
    let db_dir_fd = std::fs::File::open(&o.path)?;

    // It's important that the lock is taken before creating modifying the directory contents.
    // Because otherwise different instances could fight for changes.
    let flock = Flock::lock(&o.path, ".lock")?;

    let meta_fd = std::fs::File::create(o.path.join("meta"))?;
    let meta = Meta::create_new(o.bitbox_seed, o.bitbox_num_pages);
    Meta::write(page_pool, &meta_fd, &meta)?;
    drop(meta_fd);

    bitbox::create(o.path.clone(), o.bitbox_num_pages, o.preallocate_ht)?;
    beatree::create(&o.path)?;

    // As the last step, sync the directory. This makes sure that the directory is properly
    // written to disk.
    db_dir_fd.sync_all()?;
    Ok((db_dir_fd, flock))
}

fn is_directory_empty(path: &std::path::Path) -> std::io::Result<bool> {
    let mut entries = std::fs::read_dir(path)?;
    Ok(entries.next().is_none())
}

#[cfg(test)]
mod tests {
    use super::{PagePool, Store};

    #[test]
    fn can_crate_in_empty_dir() {
        let tempdir = tempfile::tempdir().unwrap();

        let mut options = crate::Options::new();
        options.path(tempdir.path());

        let page_pool = PagePool::new();
        let store = Store::open(&options, page_pool.clone()).unwrap();
        assert!(!store.is_poisoned());
    }
}
