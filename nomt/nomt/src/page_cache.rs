use crate::{
    bitbox::BucketIndex,
    io::{page_pool::FatPage, PagePool, PAGE_SIZE},
    merkle::ElidedChildren,
    metrics::{Metric, Metrics},
    page_region::PageRegion,
    rw_pass_cell::{Region, RegionContains, RwPassDomain, WritePass},
    Options,
};
use fxhash::FxBuildHasher;
use lru::LruCache;
use nomt_core::{
    page::DEPTH,
    page_id::{ChildPageIndex, PageId, NUM_CHILDREN, ROOT_PAGE_ID},
    trie::Node,
};
use parking_lot::{Mutex, RwLock};
use std::{
    collections::HashMap,
    fmt,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicU64, Ordering}, // [ADDED]
        Arc,
    },
};

// Total number of nodes stored in one Page. It depends on the `DEPTH`
// of the rootless sub-binary tree stored in a page following this formula:
// (2^(DEPTH + 1)) - 2
pub const NODES_PER_PAGE: usize = (1 << DEPTH + 1) - 2;

fn read_node(data: &FatPage, index: usize) -> Node {
    assert!(index < NODES_PER_PAGE, "index out of bounds");
    let start = index * 32;
    let end = start + 32;
    let mut node = [0; 32];
    node.copy_from_slice(&data[start..end]);
    node
}

fn set_node(data: &mut FatPage, index: usize, node: Node) {
    assert!(index < NODES_PER_PAGE, "index out of bounds");
    let start = index * 32;
    let end = start + 32;
    data[start..end].copy_from_slice(&node);
}

fn read_elided_children(data: &FatPage) -> ElidedChildren {
    // UNWRAP: elided_children slice is 8 bytes long.
    ElidedChildren::from_bytes(data[PAGE_SIZE - 32 - 8..PAGE_SIZE - 32].try_into().unwrap())
}

/// A mutable page.
pub struct PageMut {
    inner: FatPage,
}

impl PageMut {
    /// Freeze the page.
    pub fn freeze(self) -> Page {
        Page {
            inner: Arc::new(self.inner),
        }
    }

    /// Create a pristine (i.e. logically blank) `PageMut`.
    ///
    /// Note that this is not actually guaranteed to be blank, because pages in the pool are not
    /// zeroed prior to use. However, within the context of the trie, no search should require
    /// reading from a blank page.
    pub fn pristine_empty(page_pool: &PagePool, page_id: &PageId) -> PageMut {
        let mut page = PageMut {
            inner: page_pool.alloc_fat_page(),
        };
        page.inner[PAGE_SIZE - 32..].copy_from_slice(&page_id.encode());
        // No garbage in the elided children bitfield.
        page.set_elided_children(&ElidedChildren::new());
        page
    }

    /// Create a mutable page from raw page data.
    pub fn pristine_with_data(data: FatPage) -> PageMut {
        PageMut { inner: data }
    }

    /// Read out the node at the given index.
    pub fn node(&self, index: usize) -> Node {
        read_node(&self.inner, index)
    }

    /// Write the node at the given index.
    pub fn set_node(&mut self, index: usize, node: Node) {
        set_node(&mut self.inner, index, node)
    }

    /// Read out the bitfield representing which child page has been elided.
    pub fn elided_children(&self) -> ElidedChildren {
        read_elided_children(&self.inner)
    }

    /// Write the bitfield representing which child page has been elided.
    pub fn set_elided_children(&mut self, elided_children: &ElidedChildren) {
        self.inner[PAGE_SIZE - 32 - 8..PAGE_SIZE - 32].copy_from_slice(&elided_children.to_bytes());
    }
}

impl From<FatPage> for PageMut {
    fn from(data: FatPage) -> PageMut {
        Self::pristine_with_data(data)
    }
}

/// A handle to the page.
///
/// Can be cloned cheaply.
#[derive(Clone)]
pub struct Page {
    inner: Arc<FatPage>,
}

impl Page {
    /// Read out the node at the given index.
    pub fn node(&self, index: usize) -> Node {
        read_node(&self.inner, index)
    }

    /// Read out the bitfield representing which child page has been elided.
    pub fn elided_children(&self) -> ElidedChildren {
        read_elided_children(&self.inner)
    }

    /// Create a mutable deep copy of this page.
    pub fn deep_copy(&self) -> PageMut {
        PageMut {
            inner: FatPage::clone(&self.inner),
        }
    }

    /// Get a reference to the underlying page data.
    pub fn page_data(&self) -> &FatPage {
        &self.inner
    }

    /// Transform this into the underlying page.
    pub fn into_inner(self) -> Arc<FatPage> {
        self.inner
    }
}

impl fmt::Debug for Page {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Page").finish()
    }
}

struct CacheEntry {
    page_data: Arc<FatPage>,
    bucket_index: BucketIndex,
}

impl CacheEntry {
    fn init(page_data: Arc<FatPage>, bucket_index: BucketIndex) -> Self {
        CacheEntry {
            page_data,
            bucket_index,
        }
    }
}

// Each shard has its own domain and handles a sub-tree of the page tree, defined by a
// continuous set of children of the root page.
struct CacheShard {
    region: PageRegion,
    locked: Mutex<CacheShardLocked>,
    page_limit: NonZeroUsize,
}

struct CacheShardLocked {
    // storage for pages in the levels of the tree which we always cache.
    fixed_level_cache: HashMap<PageId, CacheEntry, FxBuildHasher>,
    cached: LruCache<PageId, CacheEntry, FxBuildHasher>,
}

impl CacheShardLocked {
    fn get(&mut self, fixed_levels: usize, page_id: &PageId) -> Option<&CacheEntry> {
        if page_id.depth() <= fixed_levels {
            self.fixed_level_cache.get(page_id)
        } else {
            self.cached.get(page_id)
        }
    }

    fn get_or_insert(
        &mut self,
        fixed_levels: usize,
        page_id: PageId,
        entry: impl FnOnce() -> CacheEntry,
    ) -> &CacheEntry {
        if page_id.depth() <= fixed_levels {
            &*self.fixed_level_cache.entry(page_id).or_insert_with(entry)
        } else {
            self.cached.get_or_insert(page_id, entry)
        }
    }

    fn insert(&mut self, fixed_levels: usize, page_id: PageId, entry: CacheEntry) {
        if page_id.depth() <= fixed_levels {
            self.fixed_level_cache.insert(page_id, entry);
        } else {
            self.cached.put(page_id, entry);
        }
    }

    fn remove(&mut self, fixed_levels: usize, page_id: &PageId) {
        if page_id.depth() <= fixed_levels {
            self.fixed_level_cache.remove(page_id);
        } else {
            self.cached.pop(page_id);
        }
    }

    fn evict(&mut self, limit: NonZeroUsize) {
        // preserve everything in the fixed level cache, removing only the variable cache.
        while self.cached.len() > limit.get() {
            let _ = self.cached.pop_lru();
        }
    }
}

struct Shared {
    shards: Vec<CacheShard>,
    root_page: RwLock<Option<CacheEntry>>,
    page_rw_pass_domain: RwPassDomain,
    fixed_levels: usize,
    metrics: Metrics,
    stats_hits: AtomicU64,
    stats_total: AtomicU64,
}

fn shard_regions(num_shards: usize) -> Vec<(PageRegion, usize)> {
    // We apply a simple strategy that assumes keys are uniformly distributed, and give
    // each shard an approximately even number of root child pages. This scales well up to
    // 64 shards.
    // The first `remainder` shards get `part + 1` children and the rest get `part`.
    let part = NUM_CHILDREN / num_shards;
    let remainder = NUM_CHILDREN % num_shards;

    let mut regions = Vec::with_capacity(num_shards);
    for shard_index in 0..num_shards {
        let (start, count) = if shard_index >= remainder {
            (part * shard_index + remainder, part)
        } else {
            (part * shard_index + shard_index, part + 1)
        };

        // UNWRAP: start / start + count are both less than the number of children.
        let start_child = ChildPageIndex::new(start as u8).unwrap();
        let end_child = ChildPageIndex::new((start + count - 1) as u8).unwrap();
        let region = PageRegion::from_page_id_descendants(ROOT_PAGE_ID, start_child, end_child);
        regions.push((region, count));
    }

    regions
}

// returns the shard index, according to the allocation in `shard_regions`.
fn shard_index_for(num_shards: usize, first_ancestor: usize) -> usize {
    let part = NUM_CHILDREN / num_shards;
    let remainder = NUM_CHILDREN % num_shards;

    if (part + 1) * remainder > first_ancestor {
        // in a 'remainder' shard
        first_ancestor / (part + 1)
    } else {
        // in a non-remainder shard - take all the remainder shards out, then divide by part,
        // and add back the remainder to get the final index
        ((first_ancestor - (part + 1) * remainder) / part) + remainder
    }
}

fn make_shards(num_shards: usize, page_cache_size: usize) -> Vec<CacheShard> {
    // page_cache_size is measured in MiB
    let cache_page_limit = (page_cache_size * 1024 * 1024) / PAGE_SIZE;
    let page_limit_per_root_child = cache_page_limit / 64;

    assert!(num_shards > 0);
    shard_regions(num_shards)
        .into_iter()
        .map(|(region, count)| CacheShard {
            region,
            locked: Mutex::new(CacheShardLocked {
                fixed_level_cache: HashMap::with_hasher(FxBuildHasher::default()),
                cached: LruCache::unbounded_with_hasher(FxBuildHasher::default()),
            }),
            // UNWRAP: both factors are non-zero
            page_limit: NonZeroUsize::new(page_limit_per_root_child * count).unwrap(),
        })
        .collect()
}

/// The index of a shard of a page cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardIndex {
    /// The "shard" containing nothing but the root page.
    Root,
    /// The shard with the given index.
    Shard(usize),
}

/// The page-cache stores full pages and can be shared between threads.
///
/// It has a sharded representation for efficient concurrent access.
#[derive(Clone)]
pub struct PageCache {
    shared: Arc<Shared>,
}

impl PageCache {
    /// Create a new `PageCache`.
    pub fn new(
        root_page_data: Option<(FatPage, BucketIndex)>,
        o: &Options,
        metrics: impl Into<Option<Metrics>>,
    ) -> Self {
        let domain = RwPassDomain::new();

        let root_page_entry =
            root_page_data.map(|(page, bucket)| CacheEntry::init(Arc::new(page), bucket));

        Self {
            shared: Arc::new(Shared {
                shards: make_shards(o.commit_concurrency, o.page_cache_size),
                root_page: RwLock::new(root_page_entry),
                page_rw_pass_domain: domain,
                metrics: metrics.into().unwrap_or(Metrics::new(false)),
                fixed_levels: o.page_cache_upper_levels,
                // [ADDED] Initialize stats
                stats_hits: AtomicU64::new(0),
                stats_total: AtomicU64::new(0),
            }),
        }
    }

    fn shard_index_for(&self, page_id: &PageId) -> Option<usize> {
        if page_id == &ROOT_PAGE_ID {
            None
        } else {
            let first_ancestor = page_id.child_index_at_level(0).to_u8() as usize;
            let shard_index = shard_index_for(self.shared.shards.len(), first_ancestor);
            debug_assert!(self.shared.shards[shard_index]
                .region
                .contains_exclusive(page_id));
            Some(shard_index)
        }
    }

    /// Query the cache for the page data at the given [`PageId`].
    ///
    /// Returns `None` if not in the cache.
    pub fn get(&self, page_id: PageId) -> Option<(Page, BucketIndex)> {
        self.shared.metrics.count(Metric::PageRequests);
        let total = self.shared.stats_total.fetch_add(1, Ordering::Relaxed) + 1;
        let result = self.get_inner(page_id);
        
        if result.is_some() {
             self.shared.stats_hits.fetch_add(1, Ordering::Relaxed);
        } else {
             self.shared.metrics.count(Metric::PageCacheMisses);
        }

        // [ADDED] Log Rate every 100 requests
        if total % 100000 == 0 {
            let hits = self.shared.stats_hits.load(Ordering::Relaxed);
            let rate = (hits as f64 / total as f64) * 100.0;
            eprintln!(
                "[PageCache] Stats: Total={} Hits={} Rate={:.2}%",
                total, hits, rate
            );
            self.log_usage_stats();
        }
        result
    }

    fn get_inner(&self, page_id: PageId) -> Option<(Page, BucketIndex)> {
        let shard_index = match self.shard_index_for(&page_id) {
            None => {
                let cache_item = self.shared.root_page.read();
                let cache_item_ref = cache_item.as_ref();
                let cache_item = cache_item_ref?;
                return Some((
                    Page {
                        inner: cache_item.page_data.clone(),
                    },
                    cache_item.bucket_index,
                ));
            }
            Some(i) => i,
        };

        let mut shard = self.shard(shard_index).locked.lock();
        shard.get(self.shared.fixed_levels, &page_id).map(|cache_item| {
             (
                Page {
                    inner: cache_item.page_data.clone(),
                },
                cache_item.bucket_index,
            )
        })
    }

    /// Acquire a write pass for all pages in the cache.
    pub fn new_write_pass(&self) -> WritePass<ShardIndex> {
        self.shared
            .page_rw_pass_domain
            .new_write_pass()
            .with_region(ShardIndex::Root)
    }

    /// Get the number of shards in this page region.
    pub fn shard_count(&self) -> usize {
        self.shared.shards.len()
    }

    /// Get the page-region associated with a shard.
    ///
    /// # Panics
    ///
    /// Panics if shard index is out of range.
    pub fn shard_region(&self, shard_index: usize) -> PageRegion {
        self.shard(shard_index).region.clone()
    }

    /// Insert a page into the cache by its data. Provide the bucket index where the
    /// page is stored if this was loaded from the disk.
    ///
    /// This ignores the inputs if the page was already present, and returns that.
    pub fn insert(&self, page_id: PageId, page: Page, bucket_index: BucketIndex) -> Page {
        let shard_index = match self.shard_index_for(&page_id) {
            None => {
                let mut cache_item = self.shared.root_page.write();
                if let Some(root_page) = cache_item.as_ref() {
                    return Page {
                        inner: root_page.page_data.clone(),
                    };
                } else {
                    *cache_item = Some(CacheEntry::init(page.inner.clone(), bucket_index));
                    return page;
                }
            }
            Some(i) => i,
        };

        let mut shard = self.shard(shard_index).locked.lock();
        let cache_entry = shard.get_or_insert(self.shared.fixed_levels, page_id, || {
            CacheEntry::init(page.inner, bucket_index)
        });

        Page {
            inner: cache_entry.page_data.clone(),
        }
    }

    /// Absorb a set of altered pages into the cache.
    ///
    /// A `None` value for a page indicates that it should be removed from the cache.
    pub fn batch_update(&self, updated_pages: Vec<(PageId, Option<(Page, BucketIndex)>)>) {
        let mut shard_guards = self
            .shared
            .shards
            .iter()
            .map(|s| s.locked.lock())
            .collect::<Vec<_>>();

        for (page_id, maybe_page) in updated_pages {
            if page_id == ROOT_PAGE_ID {
                let mut root_page = self.shared.root_page.write();
                *root_page = maybe_page
                    .map(|(page, bucket_index)| CacheEntry::init(page.inner, bucket_index));

                continue;
            }

            // UNWRAP: all pages which are not the root page are in a shard.
            let shard_index = self.shard_index_for(&page_id).unwrap();

            if let Some((page, bucket_index)) = maybe_page {
                shard_guards[shard_index].insert(
                    self.shared.fixed_levels,
                    page_id.clone(),
                    CacheEntry::init(page.inner, bucket_index),
                );
            } else {
                shard_guards[shard_index].remove(self.shared.fixed_levels, &page_id)
            }
        }
    }

    /// Evict stale pages for the cache. This should only be used after all dirty pages have been
    /// prepared for writeout with `prepare_transaction`.
    pub fn evict(&self) {
        let shard_guards = self
            .shared
            .shards
            .iter()
            .map(|s| s.locked.lock())
            .collect::<Vec<_>>();

        for (shard, mut guard) in self.shared.shards.iter().zip(shard_guards) {
            guard.evict(shard.page_limit);
        }
    }

    fn shard(&self, index: usize) -> &CacheShard {
        &self.shared.shards[index]
    }

        /// 打印当前 PageCache 的详细内存占用统计
/// 打印当前 PageCache 的详细内存占用统计
    pub fn log_usage_stats(&self) {
        let mut total_fixed_pages = 0;
        let mut total_lru_pages = 0;
        
        // 1. 统计 Root Page
        let root_occupied = if self.shared.root_page.read().is_some() { 1 } else { 0 };
        
        // 2. 统计所有 Shards
        for shard in &self.shared.shards {
            let guard = shard.locked.lock();
            total_fixed_pages += guard.fixed_level_cache.len();
            total_lru_pages += guard.cached.len();
        }

        let total_pages = root_occupied + total_fixed_pages + total_lru_pages;
        
        // 计算预估内存占用
        let approx_bytes_per_entry = PAGE_SIZE + 200; 
        let total_size_bytes = total_pages * approx_bytes_per_entry;
        let total_size_mb = total_size_bytes as f64 / 1024.0 / 1024.0;

        // 获取当前配置的固定层级
        let current_fixed_level = self.shared.fixed_levels;

        eprintln!(
            "[PageCache Stats] \n\
             \tConfigured Fixed Levels: {}  <-- (Depth <= {} are NEVER evicted)\n\
             \tTotal Cached Pages: {}\n\
             \t  - Root: {}\n\
             \t  - Fixed Levels (Top of Tree): {}\n\
             \t  - LRU (Bottom of Tree): {}\n\
             \tApprox Memory Usage: {:.2} MB (Config Limit: 8 MB)",
            current_fixed_level,      // 打印配置值
            current_fixed_level,      // 再打印一次方便阅读含义
            total_pages,
            root_occupied,
            total_fixed_pages,
            total_lru_pages,
            total_size_mb
        );
    }
}

impl Region for ShardIndex {
    fn encompasses(&self, other: &Self) -> bool {
        match (self, other) {
            (ShardIndex::Root, _) => true,
            (ShardIndex::Shard(a), ShardIndex::Shard(b)) => a == b,
            _ => false,
        }
    }

    fn excludes_unique(&self, other: &Self) -> bool {
        match (self, other) {
            (ShardIndex::Shard(a), ShardIndex::Shard(b)) => a != b,
            _ => false,
        }
    }
}

unsafe impl RegionContains<ShardIndex> for ShardIndex {
    fn contains(&self, page_shard: &ShardIndex) -> bool {
        match (self, page_shard) {
            (ShardIndex::Shard(a), ShardIndex::Shard(b)) => a == b,
            (ShardIndex::Root, _) => true,
            (ShardIndex::Shard(_), ShardIndex::Root) => {
                // safe to read the root page from any shard pass.
                true
            }
        }
    }

    fn contains_exclusive(&self, page_shard: &ShardIndex) -> bool {
        match (self, page_shard) {
            (ShardIndex::Shard(a), ShardIndex::Shard(b)) => a == b,
            (ShardIndex::Root, _) => true,
            _ => false, // exclusive access only to pages in this shard.
        }
    }
}

