//! The leaf cache stores recently accessed leaf nodes.

use crate::{
    beatree::{allocator::PageNumber, leaf::node::LeafNode},
    io::PAGE_SIZE,
};
use lru::LruCache;
use parking_lot::{Mutex, MutexGuard};
use std::{
    collections::hash_map::RandomState,
    hash::BuildHasher,
    sync::{
        atomic::{AtomicU64, Ordering}, // [ADDED] Import atomics
        Arc,
    },
};

/// A cache for leaf nodes.
///
/// This i cheap to clone.
#[derive(Clone)]
pub struct LeafCache {
    inner: Arc<Shared>,
}

impl LeafCache {
    /// Create a new cache with the given number of shards and the maximum number of items
    /// to hold. `shards` must be non-zero.
    pub fn new(shards: usize, leaf_cache_size: usize) -> Self {
        let max_items = (leaf_cache_size * 1024 * 1024) / PAGE_SIZE;
        let items_per_shard = max_items / shards;
        LeafCache {
            inner: Arc::new(Shared {
                shards: (0..shards)
                    .map(|_| Shard {
                        cache: LruCache::unbounded(),
                        max_items: items_per_shard,
                    })
                    .map(Mutex::new)
                    .collect::<Vec<_>>(),
                shard_assigner: RandomState::new(),
                // [ADDED] Init counters
                stats_hits: AtomicU64::new(0),
                stats_total: AtomicU64::new(0),
            }),
        }
    }

    /// Get a cache entry, updating the LRU state.
    pub fn get(&self, page_number: PageNumber) -> Option<Arc<LeafNode>> {
        let mut shard = self.inner.shard_for(page_number);
        let result = shard.cache.get(&page_number).map(|x| x.clone());

        // [MODIFIED] Calculate and Log Hit Rate
        let total = self.inner.stats_total.fetch_add(1, Ordering::Relaxed) + 1;
        let is_hit = result.is_some();
        if is_hit {
            self.inner.stats_hits.fetch_add(1, Ordering::Relaxed);
        }

        // 没必要每次都打印，每 100 次或 1000 次请求打印一次摘要
        if total % 100000 == 0 {
            let hits = self.inner.stats_hits.load(Ordering::Relaxed);
            let rate = (hits as f64 / total as f64) * 100.0;
            eprintln!(
                "[LeafCache] Stats: Total={} Hits={} Rate={:.2}% (Last operation: {})",
                total,
                hits,
                rate,
                if is_hit { "HIT" } else { "MISS" }
            );
        }

        result
    }

    /// Insert a cache entry. Evicts LRU items to maintain max_items limit.
    pub fn insert(&self, page_number: PageNumber, node: Arc<LeafNode>) {
        let mut shard = self.inner.shard_for(page_number);

        shard.cache.put(page_number, node);
        // FIX: enforce max_items on every insert (otherwise reads grow cache unbounded)
        while shard.cache.len() > shard.max_items {
            shard.cache.pop_lru();
        }
    }

    /// Evict all excess items from the cache.
    pub fn evict(&self) {
        for shard in &self.inner.shards {
            let mut shard = shard.lock();
            while shard.cache.len() > shard.max_items {
                let _ = shard.cache.pop_lru();
            }
        }
    }
}

struct Shared {
    shards: Vec<Mutex<Shard>>,
    shard_assigner: RandomState,
    // [ADDED] Global stats for this cache instance
    stats_hits: AtomicU64,
    stats_total: AtomicU64,
}

impl Shared {
    fn shard_for(&self, page_number: PageNumber) -> MutexGuard<'_, Shard> {
        self.shards[self.shard_index_for(page_number)].lock()
    }

    fn shard_index_for(&self, page_number: PageNumber) -> usize {
        (self.shard_assigner.hash_one(page_number.0) as usize) % self.shards.len()
    }
}

struct Shard {
    cache: LruCache<PageNumber, Arc<LeafNode>>,
    max_items: usize,
}
