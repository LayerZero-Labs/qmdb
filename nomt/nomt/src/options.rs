use std::path::PathBuf;

/// Options when opening a [`crate::Nomt`] instance.
#[derive(Debug)]
pub struct Options {
    /// The path to the directory where the trie is stored.
    pub(crate) path: PathBuf,
    /// The number of commit workers. Values over 64 will be rounded down to 64.
    pub(crate) commit_concurrency: usize,
    /// The number of io_uring instances, or I/O threads on non-Linux platforms.
    pub(crate) io_workers: usize,
    /// Enable or disable metrics collection.
    pub(crate) metrics: bool,
    pub(crate) bitbox_num_pages: u32,
    pub(crate) bitbox_seed: [u8; 16],
    pub(crate) panic_on_sync: Option<PanicOnSyncMode>,
    pub(crate) rollback: bool,
    /// The maximum number of commits that can be rolled back.
    pub(crate) max_rollback_log_len: u32,
    pub(crate) warm_up: bool,
    /// Whether to preallocate the hashtable file.
    pub(crate) preallocate_ht: bool,
    /// The maximum size of the page cache specified in MiB, rounded down
    /// to the nearest byte multiple of [`crate::io::PAGE_SIZE`].
    pub(crate) page_cache_size: usize,
    /// The maximum size of the leaf cache specified in MiB, rounded down
    /// to the nearest byte multiple of [`crate::io::PAGE_SIZE`].
    pub(crate) leaf_cache_size: usize,
    /// Whether to prepopulate the upper layers of the page cache on startup.
    /// This incurs some I/O on startup but leads to predictable worst-case performance.
    pub(crate) prepopulate_page_cache: bool,
    pub(crate) page_cache_upper_levels: usize,
}

impl Options {
    /// Create a new `Options` instance with the default values and a random bitbox seed.
    pub fn new() -> Self {
        use rand::Rng as _;
        let mut bitbox_seed = [0u8; 16];
        rand::rngs::OsRng.fill(&mut bitbox_seed);

        Self {
            path: PathBuf::from("nomt_db"),
            commit_concurrency: 1,
            io_workers: 3,
            metrics: false,
            bitbox_num_pages: 64_000,
            bitbox_seed,
            panic_on_sync: None,
            rollback: false,
            max_rollback_log_len: 100,
            warm_up: false,
            preallocate_ht: true,
            page_cache_size: 256,
            leaf_cache_size: 256,
            prepopulate_page_cache: false,
            page_cache_upper_levels: 2,
        }
    }

    /// Set the path to the directory where the trie is stored.
    ///
    /// If the directory does not exist or is empty, a new empty instance of the database will be
    /// initialized in this directory.
    pub fn path(&mut self, path: impl Into<PathBuf>) {
        self.path = path.into();
    }

    /// Set the maximum number of concurrent commit workers.
    ///
    /// Values over 64 will be rounded down to 64.
    ///
    /// May not be zero.
    pub fn commit_concurrency(&mut self, commit_concurrency: usize) {
        self.commit_concurrency = commit_concurrency;
    }

    /// Set metrics collection on or off.
    ///
    /// Default: off.
    pub fn metrics(&mut self, metrics: bool) {
        self.metrics = metrics;
    }

    /// Set the number of io_uring instances.
    ///
    /// Must be more than 0
    pub fn io_workers(&mut self, io_workers: usize) {
        assert!(io_workers > 0);
        self.io_workers = io_workers;
    }

    /// Set the number of hashtable buckets to use when creating the database.
    pub fn hashtable_buckets(&mut self, hashtable_buckets: u32) {
        self.bitbox_num_pages = hashtable_buckets;
    }

    /// Set the seed for the hash function used by the bitbox store.
    ///
    /// Useful for reproducibility.
    pub fn bitbox_seed(&mut self, bitbox_seed: [u8; 16]) {
        self.bitbox_seed = bitbox_seed;
    }

    /// Set to `true` to panic during sync at different points.
    ///
    /// Useful to test WAL recovery and will just cause your DB to be unusable otherwise.
    pub fn panic_on_sync(&mut self, mode: PanicOnSyncMode) {
        self.panic_on_sync = Some(mode);
    }

    /// Set to `true` to enable rolling back committed sessions.
    pub fn rollback(&mut self, rollback: bool) {
        self.rollback = rollback;
    }

    /// Set the maximum number of commits that can be rolled back.
    ///
    /// Only relevant if rollback is enabled.
    ///
    /// Default: 100.
    pub fn max_rollback_log_len(&mut self, max_rollback_log_len: u32) {
        self.max_rollback_log_len = max_rollback_log_len;
    }

    /// Configure whether merkle page fetches should be warmed up while sessions are ongoing.
    ///
    /// Enabling this feature can pessimize performance.
    pub fn warm_up(&mut self, warm_up: bool) {
        self.warm_up = warm_up;
    }

    /// Sets whether to preallocate the hashtable file.
    ///
    /// Many filesystems don't handle sparse files well. If the `preallocate_ht` option is set to
    /// `true`, NOMT will try to make sure that the file is fully allocated.
    ///
    /// If set to `false` this won't allocate the disk space for the hashtable file upfront, but can
    /// lead to fragmentation later.
    ///
    /// Default: `true`.
    pub fn preallocate_ht(&mut self, preallocate_ht: bool) {
        self.preallocate_ht = preallocate_ht;
    }

    /// Sets the size of the page cache in MiB.
    ///
    /// This does not count the memory used by the upper levels of the page
    /// cache. See [`Self::page_cache_upper_levels`]
    /// Rounded down to the nearest byte multiple of 4096.
    ///
    /// Default: 256MiB.
    pub fn page_cache_size(&mut self, page_cache_size: usize) {
        self.page_cache_size = page_cache_size;
    }

    /// Sets the size of the leaf cache in MiB.
    ///
    /// Rounded down to the nearest byte multiple of 4096.
    ///
    /// Default: 256MiB.
    pub fn leaf_cache_size(&mut self, leaf_cache_size: usize) {
        self.leaf_cache_size = leaf_cache_size;
    }

    /// Sets whether to prepopulate the upper levels of the page cache on startup.
    /// Has no effect if [`Options::page_cache_upper_levels`] is set to 0.
    ///
    /// This incurs some I/O on startup but leads to predictable worst-case performance.
    /// Default: false
    pub fn prepopulate_page_cache(&mut self, prepopulate: bool) {
        self.prepopulate_page_cache = prepopulate;
    }

    /// Sets the number of upper levels of the page tree to keep permanently
    /// cached.
    ///
    /// Each level adds 64x the RAM burden of the previous.
    /// Level 1 uses ≈256KiB, level 2 ≈16MiB, level 3 ≈1GiB.
    ///
    /// Default: 2
    pub fn page_cache_upper_levels(&mut self, upper_levels: usize) {
        self.page_cache_upper_levels = upper_levels;
    }
}

#[test]
fn page_size_is_4096() {
    // Update the docs above if this fails.
    assert_eq!(crate::io::PAGE_SIZE, 4096);
}

/// Modes for panicking during sync.
#[derive(Clone, Copy, Debug)]
pub enum PanicOnSyncMode {
    /// After the meta has been swapped, but before other points.
    PostMeta,
    /// Before the meta has been swapped, but after the WAL is written.
    PostWal,
}
