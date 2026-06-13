use super::{
    resource::ResourceAllocator,
    swarm::{self, SwarmFeatures},
    ResourceExhaustion,
};
use rand::{Rng, RngCore};
use std::sync::{Arc, Mutex};

/// Percentage of the assigned space to the workload that will be
/// used by the hash table stored on disk.
const HASH_TABLE_SIZE_RATIO_DISK: f64 = 0.6;

/// Percentage of the assigned space to the workload that will be
/// used by the hash table stored in memory.
///
/// This is smaller than `HASH_TABLE_SIZE_RATIO_DISK` becaues it accounts
/// also for all the other things that need to be stored in memory,
/// not only the workload directory.
const HASH_TABLE_SIZE_RATIO_MEM: f64 = 0.4;

/// Maximum size of a single commit, combining reads and updates.
const MAX_COMMIT_SIZE: usize = 200_000;

/// Maximum number of commit workers supported by nomt.
const MAX_COMMIT_CONCURRENCY: usize = 64;

/// Maximum number of io workers supported by nomt.
const MAX_IO_WORKERS: usize = 32;

/// Maximum amount of MiB that an in memory cache can occupy.
///
/// It refers to leaf and page cache.
const MAX_IN_MEMORY_CACHE_SIZE: usize = 512;

/// Maximum supported number of page cache upper levels.
const MAX_PAGE_CACHE_UPPER_LEVELS: usize = 3;

/// Maximum size of a value that fits in a leaf,
/// after this threshold, overflow values will be used.
pub const MAX_VALUE_LEN: usize = 1333;

/// Maximum size of an overflow value.
pub const MAX_OVERFLOW_VALUE_LEN: usize = 32 * 1024;

#[derive(Debug)]
pub struct WorkloadConfiguration {
    /// The seed for bitbox generated for this workload.
    pub bitbox_seed: [u8; 16],
    /// How many iterations the workload should perform.
    pub iterations: usize,
    /// The number of commit workers.
    pub commit_concurrency: usize,
    /// The number of io_uring instances.
    pub io_workers: usize,
    /// The number of pages within the ht.
    pub hashtable_buckets: u32,
    /// Whether merkle page fetches should be warmed up while sessions are ongoing.
    pub warm_up: bool,
    /// Whether to preallocate the hashtable file.
    pub preallocate_ht: bool,
    /// The maximum size of the page cache.
    pub page_cache_size: usize,
    /// The maximum size of the leaf cache.
    pub leaf_cache_size: usize,
    /// Whether to prepopulate the upper layers of the page cache on startup.
    pub prepopulate_page_cache: bool,
    /// Number of upper layers contained in the cache.
    pub page_cache_upper_levels: usize,
    /// The average size of a commit, combining reads and updates.
    pub avg_commit_size: usize,
    /// The average size of a generated value.
    pub avg_value_len: usize,
    /// The average size of a generated overflow value.
    pub avg_overflow_value_len: usize,
    /// Percentage of `avg_commit_size` that will be designated for reads,
    /// the rest will be used by the changeset.
    pub reads: f64,
    /// The number of concurrent readers.
    pub read_concurrency: usize,
    /// The probability of reading an already existing key.
    pub read_existing_key: f64,
    /// The weight of generating an update operation when constructing the changeset.
    pub update_key: f64,
    /// The weight of generating a deletion operation when constructing the changeset.
    pub delete_key: f64,
    /// The weight of generating an insertion operation when constructing the changeset.
    pub new_key: f64,
    /// When generating a value, the probability of generating a value that will spill into the
    /// overflow pages.
    pub overflow: f64,
    /// Whether to ensure the correct application of the changeset after every commit.
    pub ensure_changeset: bool,
    /// Whether to randomly sample the state after every crash or rollback.
    pub sample_snapshot: bool,
    /// When executing a commit this is the probability of causing it to crash.
    pub commit_crash: f64,
    /// When executing a workload iteration ,this is the probability of executing a rollback.
    pub rollback: f64,
    /// The max number of commits involved in a rollback.
    pub max_rollback_commits: usize,
    /// When executing a rollback this is the probability of causing it to crash.
    pub rollback_crash: f64,
    /// Whether trickfs will be used or not.
    ///
    /// If false, enospc_on/off and latency_on/off will all be 0.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub trickfs: bool,
    /// The probability of turning on the `ENOSPC` error.
    pub enospc_on: f64,
    /// The probability of turning off the `ENOSPC` error.
    pub enospc_off: f64,
    /// The probability of turning on the latency injector.
    pub latency_on: f64,
    /// The probability of turning off the latency injector.
    pub latency_off: f64,
    // Whether to ensure the correctness of the entire state after every crash or rollback.
    //
    // This is only used when repeating a failed workload.
    ensure_snapshot: bool,
}

impl WorkloadConfiguration {
    fn new_inner(
        rng: &mut rand_pcg::Pcg64,
        avail_bytes: impl Fn(bool) -> Result<u64, ResourceExhaustion>,
    ) -> Result<Self, ResourceExhaustion> {
        let swarm_features = swarm::new_features_set(rng);

        let trickfs = swarm_features
            .iter()
            .find(|feature| {
                matches!(
                    feature,
                    SwarmFeatures::TrickfsENOSPC | SwarmFeatures::TrickfsLatencyInjection
                )
            })
            .is_some();

        let avail_bytes = avail_bytes(trickfs)?;

        let mut bitbox_seed = [0u8; 16];
        rng.fill_bytes(&mut bitbox_seed);

        let mut config = Self {
            read_existing_key: 0.0,
            new_key: 0.0,
            delete_key: 0.0,
            update_key: 0.0,
            overflow: 0.0,
            rollback: 0.0,
            commit_crash: 0.0,
            rollback_crash: 0.0,
            trickfs,
            enospc_on: 0.0,
            enospc_off: 0.0,
            latency_on: 0.0,
            latency_off: 0.0,
            ensure_changeset: false,
            ensure_snapshot: false,
            sample_snapshot: false,
            max_rollback_commits: 0,
            reads: 0.0,
            read_concurrency: 1,
            warm_up: false,
            preallocate_ht: false,
            prepopulate_page_cache: false,
            bitbox_seed,
            avg_commit_size: rng.gen_range(1..=(MAX_COMMIT_SIZE / 2)),
            avg_value_len: rng.gen_range(1..=(MAX_VALUE_LEN / 2)),
            avg_overflow_value_len: rng.gen_range(MAX_VALUE_LEN..=(MAX_OVERFLOW_VALUE_LEN / 2)),
            commit_concurrency: rng.gen_range(1..=MAX_COMMIT_CONCURRENCY),
            io_workers: rng.gen_range(1..=MAX_IO_WORKERS),
            page_cache_size: rng.gen_range(1..=MAX_IN_MEMORY_CACHE_SIZE),
            leaf_cache_size: rng.gen_range(1..=MAX_IN_MEMORY_CACHE_SIZE),
            page_cache_upper_levels: rng.gen_range(0..=MAX_PAGE_CACHE_UPPER_LEVELS),
            // To avoid reaching Bucket Exhaustion, we limit the number of iterations
            // with the worst case scenario of every iteration adding `avg_commit_size` new keys.
            hashtable_buckets: 0,
            iterations: 0,
        };

        // Use only portion of the assigned bytes for the hash table.
        let hashtable_ratio = if trickfs {
            HASH_TABLE_SIZE_RATIO_MEM
        } else {
            HASH_TABLE_SIZE_RATIO_DISK
        };

        let hashtable_size = (avail_bytes as f64 * hashtable_ratio) as usize;
        let hashtable_buckets = (hashtable_size / 4096) as u32;
        config.hashtable_buckets = hashtable_buckets;

        // Do not use the entire space left by the hashtable
        // for the beatree and rollbacks, instead, leave some room for estimation error.
        let bytes_left = (avail_bytes as usize - hashtable_size) as f64 * 0.8;

        // Expected max size occupied by the rollback log.
        let rollback_avg_value_len = (config.avg_value_len as f64 * (1. - config.overflow))
            + (config.avg_overflow_value_len as f64 * config.overflow);
        let rollback_size = config.max_rollback_commits as f64
            * (config.avg_commit_size as f64 * rollback_avg_value_len);

        let avg_leaf_page_usage = 4096. * (2. / 3.);
        // Leaves do not store overflow values, just only store the overflow cell,
        // which can be at most 96 bytes.
        let avg_value_per_leaf: f64 =
            ((1. - config.overflow) * config.avg_value_len as f64) + (config.overflow * 96.);
        // The maximum integer number of items that can fit in a leaf.
        let n_per_leaf: f64 = (avg_leaf_page_usage / (34. + avg_value_per_leaf)).floor();

        // Estimate the number of items that can fit in the space left for leaves and bbns.
        // Derivated from:
        // bn_size = ln_size / 100, which is an overestimate of the size containing bbn nodes,
        // both because we accounted also for pages that contain overflow values
        // (which are not pointed to by bbns) and 100 is an underestimate of a branch node fanout.
        // ln_size = (1 - overflow)*((n_items/n_per_leaf) * 4096) + overflow*(n_items * avg_overflow_value_len)
        // and ln_size + bn_size = bytes_left - rollback_size
        let bytes_per_item = ((((1. - config.overflow) * avg_leaf_page_usage) / n_per_leaf)
            + (config.overflow * config.avg_overflow_value_len as f64))
            * 1.01;
        let mut n_items = ((bytes_left - rollback_size) / bytes_per_item) as u64;

        // Given a uniform distribution and a usage of the hash table of 80%,
        // estimate the number of items that can be inserted into the trie before reaching
        // bucket exhaustion.
        let max_hashtable_usage = hashtable_size as f64 * 0.80;
        let max_item_page_tree =
            64u64.pow((((63. * max_hashtable_usage) / 4096.) + 1.).log(64.).ceil() as u32 - 1);

        // If the number of items that would fit in the beatree is greater
        // than what could fit in the trie, than reduce them to a portion of max_item_page_tree.
        if n_items > max_item_page_tree {
            n_items = (max_item_page_tree as f64 * 0.8) as u64;
        }

        config.iterations = n_items as usize / config.avg_commit_size;

        for swarm_feature in swarm_features {
            config.apply_swarm_feature(rng, swarm_feature);
        }

        Ok(config)
    }

    pub fn is_rollback_enable(&self) -> bool {
        self.rollback > 0.0
    }

    pub fn no_new_keys(&self) -> bool {
        self.new_key == 0.0
    }

    pub fn new(
        rng: &mut rand_pcg::Pcg64,
        workload_id: u64,
        resource_alloc: Arc<Mutex<ResourceAllocator>>,
    ) -> Result<Self, ResourceExhaustion> {
        let avail_bytes = |trickfs: bool| {
            // UNWRAP: The allocator is only used during the creation of the workload or
            // upon completion or failure to free the allocated data.
            let mut allocator = resource_alloc.lock().unwrap();
            if trickfs {
                allocator.alloc_memory(workload_id)?;
                Ok(allocator.assigned_resources(workload_id).memory)
            } else {
                allocator.alloc(workload_id)?;
                Ok(allocator.assigned_resources(workload_id).disk)
            }
        };
        Self::new_inner(rng, avail_bytes)
    }

    pub fn new_with_resources(
        rng: &mut rand_pcg::Pcg64,
        assigned_disk: u64,
        assigned_memory: u64,
    ) -> Self {
        let avail_bytes = |trickfs: bool| {
            if trickfs {
                Ok(assigned_memory)
            } else {
                Ok(assigned_disk)
            }
        };
        Self::new_inner(rng, avail_bytes).unwrap()
    }

    pub fn enable_ensure_snapshot(&mut self) {
        self.ensure_snapshot = true;
    }

    pub fn should_ensure_snapshot(&mut self) -> bool {
        self.ensure_snapshot
    }

    pub fn apply_swarm_feature(&mut self, rng: &mut rand_pcg::Pcg64, feature: SwarmFeatures) {
        match feature {
            SwarmFeatures::TrickfsENOSPC => {
                self.enospc_on = rng.gen_range(0.01..=1.00);
                self.enospc_off = rng.gen_range(0.01..=1.00);
            }
            SwarmFeatures::TrickfsLatencyInjection => {
                self.latency_on = rng.gen_range(0.01..=1.00);
                self.latency_off = rng.gen_range(0.01..=1.00);
            }
            SwarmFeatures::EnsureChangeset => self.ensure_changeset = true,
            SwarmFeatures::SampleSnapshot => self.sample_snapshot = true,
            SwarmFeatures::WarmUp => self.warm_up = true,
            SwarmFeatures::PreallocateHt => self.preallocate_ht = true,
            SwarmFeatures::Read => {
                self.reads = rng.gen_range(0.01..1.00);
                self.read_concurrency = rng.gen_range(1..=64);
                self.read_existing_key = rng.gen_range(0.01..1.00);
            }
            SwarmFeatures::Rollback => {
                self.rollback = rng.gen_range(0.01..1.00);
                // During scheduling of rollbacks the lower bound is 1
                // thus max must be at least 2 to avoid creating an empty range.
                self.max_rollback_commits = rng.gen_range(2..100);
            }
            SwarmFeatures::RollbackCrash => self.rollback_crash = rng.gen_range(0.01..1.00),
            SwarmFeatures::CommitCrash => self.commit_crash = rng.gen_range(0.01..1.00),
            SwarmFeatures::PrepopulatePageCache => self.prepopulate_page_cache = true,
            SwarmFeatures::NewKeys => self.new_key = rng.gen_range(0.01..=1.00),
            SwarmFeatures::DeleteKeys => self.delete_key = rng.gen_range(0.01..=1.00),
            SwarmFeatures::UpdateKeys => self.update_key = rng.gen_range(0.01..=1.00),
            SwarmFeatures::OverflowValues => self.overflow = rng.gen_range(0.01..=1.00),
        }
    }
}
