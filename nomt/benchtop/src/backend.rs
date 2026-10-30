use crate::{nomt::NomtDB, timer::Timer, workload::Workload};

#[cfg(feature = "sov-db")]
use crate::sov_db::SovDB;

#[cfg(feature = "sp-trie")]
use crate::sp_trie::SpTrieDB;

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum Backend {
    SovDB,
    Nomt,
    SpTrie,
}

impl Backend {
    pub fn all_backends() -> Vec<Self> {
        vec![Backend::SovDB, Backend::SpTrie, Backend::Nomt]
    }

    // If reset is true, then erase any previous backend's database
    // and restart from an empty database.
    // Otherwise, use the already present database.
    pub fn instantiate(
        &self,
        reset: bool,
        commit_concurrency: usize,
        io_workers: usize,
        hashtable_buckets: Option<u32>,
        page_cache_size: Option<usize>,
        leaf_cache_size: Option<usize>,
        page_cache_upper_levels: usize,
        prepopulate_page_cache: bool,
        overlay_window_length: usize,
    ) -> DB {
        match self {
            Backend::SovDB => {
                #[cfg(not(feature = "sov-db"))]
                panic!("benchtop not compiled with feature sov-db. rebuild");
                #[cfg(feature = "sov-db")]
                DB::Sov(SovDB::open(reset))
            }
            Backend::Nomt => DB::Nomt(NomtDB::open(
                reset,
                commit_concurrency,
                io_workers,
                hashtable_buckets,
                page_cache_size,
                leaf_cache_size,
                page_cache_upper_levels,
                prepopulate_page_cache,
                overlay_window_length,
            )),
            Backend::SpTrie => {
                #[cfg(not(feature = "sp-trie"))]
                panic!("benchtop not compiled with feature sp-trie. rebuild");
                #[cfg(feature = "sp-trie")]
                DB::SpTrie(SpTrieDB::open(reset))
            }
        }
    }
}

/// A transaction over the database which allows reading and writing.
pub trait Transaction {
    /// Read a value from the database. If a value was previously written, return that.
    fn read(&mut self, key: &[u8]) -> Option<Vec<u8>>;

    /// Note that a value was read from a cache, for inclusion in a storage proof.
    fn note_read(&mut self, key: &[u8], value: Option<Vec<u8>>);

    /// Write a value to the database. `None` means to delete the previous value.
    fn write(&mut self, key: &[u8], value: Option<&[u8]>);
}

/// A wrapper around all databases implemented in this tool.
pub enum DB {
    #[cfg(feature = "sov-db")]
    Sov(SovDB),
    #[cfg(feature = "sp-trie")]
    SpTrie(SpTrieDB),
    Nomt(NomtDB),
}

impl DB {
    /// Execute a workload repeatedly until done or a time limit is reached.
    pub fn execute(
        &mut self,
        mut timer: Option<&mut Timer>,
        workload: &mut dyn Workload,
        timeout: Option<std::time::Instant>,
    ) {
        while !workload.is_done() {
            if timeout
                .as_ref()
                .map_or(false, |t| std::time::Instant::now() > *t)
            {
                break;
            }
            let timer = timer.as_deref_mut();
            match self {
                #[cfg(feature = "sov-db")]
                DB::Sov(db) => db.execute(timer, workload),
                #[cfg(feature = "sp-trie")]
                DB::SpTrie(db) => db.execute(timer, workload),
                DB::Nomt(db) => db.execute(timer, workload),
            }
        }
    }

    /// Execute several workloads in parallel, repeatedly, until all done or a time limit is reached.
    ///
    /// Only works with the NOMT backend.
    pub fn parallel_execute(
        &mut self,
        mut timer: Option<&mut Timer>,
        thread_pool: &rayon::ThreadPool,
        workloads: &mut [Box<dyn Workload>],
        timeout: Option<std::time::Instant>,
    ) -> anyhow::Result<()> {
        while workloads.iter().any(|w| !w.is_done()) {
            if timeout
                .as_ref()
                .map_or(false, |t| std::time::Instant::now() > *t)
            {
                break;
            }
            let timer = timer.as_deref_mut();
            match self {
                #[cfg(feature = "sov-db")]
                DB::Sov(_) => {
                    anyhow::bail!("parallel execution is only supported with the NOMT backend.")
                }
                #[cfg(feature = "sp-trie")]
                DB::SpTrie(_) => {
                    anyhow::bail!("parallel execution is only supported with the NOMT backend.")
                }
                DB::Nomt(db) => db.parallel_execute(timer, thread_pool, workloads),
            }
        }

        Ok(())
    }

    /// Print metrics collected by the Backend if it supports metrics collection
    pub fn print_metrics(&self) {
        match self {
            DB::Nomt(db) => db.print_metrics(),
            #[cfg(any(feature = "sp-trie", feature = "sov-db"))]
            _ => (),
        }
    }
}
