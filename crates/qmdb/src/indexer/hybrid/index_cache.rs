use crate::utils::shortlist::ShortList;
use lazy_static::lazy_static;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const KEEP_RANGE: i64 = 2;

lazy_static! {
    pub static ref COUNTERS: Counters = Counters::default();
}

#[derive(Default)]
pub struct Counters {
    hit_wi: AtomicU64,
    miss_wi: AtomicU64,
    none_wi: AtomicU64,
    hit_wo: AtomicU64,
    miss_wo: AtomicU64,
    none_wo: AtomicU64,
    page_num: AtomicU64,
    wr_bytes: AtomicUsize,
}

impl Counters {
    pub fn print(&self) {
        if cfg!(not(feature = "profiling_hybridindexer")) {
            return;
        }
        // Profiling counters: no synchronization semantics required.
        let h_wi = self.hit_wi.load(Ordering::Relaxed);
        let m_wi = self.miss_wi.load(Ordering::Relaxed);
        let n_wi = self.none_wi.load(Ordering::Relaxed);
        let h_wo = self.hit_wo.load(Ordering::Relaxed);
        let m_wo = self.miss_wo.load(Ordering::Relaxed);
        let n_wo = self.none_wo.load(Ordering::Relaxed);
        let p_num = self.page_num.load(Ordering::Relaxed);
        let wr_bytes = self.wr_bytes.load(Ordering::Relaxed);
        println!(
            "hit_wi={h_wi} miss_wi={m_wi} none_wi={n_wi} hit_wo={h_wo} miss_wo={m_wo} none_wo={n_wo} page_num={p_num} wr={wr_bytes}",
        );
    }

    pub fn incr_page_num(&self) {
        if cfg!(feature = "profiling_hybridindexer") {
            self.page_num.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn incr_wr_bytes(&self, n: usize) {
        if cfg!(feature = "profiling_hybridindexer") {
            self.wr_bytes.fetch_add(n, Ordering::Relaxed);
        }
    }

    fn incr_hit_wi(&self, nop: bool) {
        if cfg!(feature = "profiling_hybridindexer") && !nop {
            self.hit_wi.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn incr_miss_wi(&self, nop: bool) {
        if cfg!(feature = "profiling_hybridindexer") && !nop {
            self.miss_wi.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn incr_none_wi(&self, nop: bool) {
        if cfg!(feature = "profiling_hybridindexer") && !nop {
            self.none_wi.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn incr_hit_wo(&self, nop: bool) {
        if cfg!(feature = "profiling_hybridindexer") && !nop {
            self.hit_wo.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn incr_miss_wo(&self, nop: bool) {
        if cfg!(feature = "profiling_hybridindexer") && !nop {
            self.miss_wo.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn incr_none_wo(&self, nop: bool) {
        if cfg!(feature = "profiling_hybridindexer") && !nop {
            self.none_wo.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Default)]
pub struct IndexCacheMap {
    m: HashMap<i64, IndexCache>,
}

impl IndexCacheMap {
    pub fn clear(&mut self) {
        self.m.clear();
    }

    pub fn add(&mut self, height: i64, k: u64, prev_k: u64, list: &ShortList) {
        if height < 0 {
            return;
        }
        if let Some(cache) = self.m.get_mut(&height) {
            return cache.add(k, prev_k, list);
        }
        // remove an old cache
        let mut old_height = -1;
        for (h, _) in self.m.iter() {
            if *h < height - KEEP_RANGE {
                old_height = *h;
                break;
            }
        }
        if old_height > 0 {
            self.m.remove(&old_height);
        }
        // insert a new cache
        let mut cache = IndexCache::default();
        cache.add(k, prev_k, list);
        self.m.insert(height, cache);
    }

    pub fn lookup_without_prev(&self, height: i64, warmup: bool, k: u64) -> Option<ShortList> {
        if height < 0 {
            return None;
        }
        if let Some(cache) = self.m.get(&height) {
            return cache.lookup_without_prev(warmup, k);
        }
        COUNTERS.incr_none_wo(warmup);
        None
    }

    pub fn lookup_with_prev(
        &mut self,
        height: i64,
        warmup: bool,
        k: u64,
    ) -> Option<(u64, ShortList)> {
        if height < 0 {
            return None;
        }
        if let Some(cache) = self.m.get_mut(&height) {
            return cache.lookup_with_prev(warmup, k);
        }
        COUNTERS.incr_none_wi(warmup);
        None
    }
}

#[derive(Default)]
pub struct IndexCache {
    // without_prev: mapping u64-key to i64-pos
    // pos < 0 means no position (no record on disk for k)
    // pos >= 0 means unique position
    without_prev: HashMap<u64, i64>,
    // with_prev: mapping u64-key to u64-prev-key and adjacent-pos-list
    // In ShortList bit#49 means prev bucket (1) or current bucket (0)
    with_prev: HashMap<u64, (u64, ShortList)>,
}

impl IndexCache {
    pub fn clear(&mut self) {
        self.without_prev.clear();
        self.with_prev.clear();
    }

    pub fn add(&mut self, k: u64, prev_k: u64, list: &ShortList) {
        if list.is_empty() {
            // pos < 0 means cache-hit with no position
            self.without_prev.insert(k, -1);
            return;
        } else if list.len() == 1 {
            let pos = list.get(0);
            // bit#49 != 0 means prev bucket, not current
            if (pos >> 48) == 0 {
                self.without_prev.insert(k, pos);
                return;
            }
        }
        self.with_prev.insert(k, (prev_k, list.clone()));
    }

    pub fn lookup_without_prev(&self, warmup: bool, k: u64) -> Option<ShortList> {
        let mut res = ShortList::new();
        if let Some(&pos) = self.without_prev.get(&k) {
            COUNTERS.incr_hit_wo(warmup);
            // pos < 0 means cache-hit with no position
            if pos >= 0 {
                res.append(pos);
            }
            Some(res) //cache-hit
        } else if let Some((_, list)) = self.with_prev.get(&k) {
            // now without_prev has a cache-miss, let's check with_prev
            // with_prev may also contain the result for current bucket
            COUNTERS.incr_hit_wo(warmup);
            for (_, pos) in list.enumerate() {
                // bit#49 != 0 means prev bucket, not current
                if (pos >> 48) == 0 {
                    res.append(pos);
                }
            }
            Some(res) //cache-hit
        } else {
            COUNTERS.incr_miss_wo(warmup);
            None //cache-miss
        }
    }

    pub fn lookup_with_prev(&mut self, warmup: bool, k: u64) -> Option<(u64, ShortList)> {
        if let Some((prev_k, list)) = self.with_prev.get(&k) {
            COUNTERS.incr_hit_wi(warmup);
            Some((*prev_k, list.clone())) //cache-hit
        } else {
            COUNTERS.incr_miss_wi(warmup);
            None //cache-miss
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_index_cache() {
        let cache = IndexCache::default();
        assert!(cache.without_prev.is_empty());
        assert!(cache.with_prev.is_empty());
    }

    #[test]
    fn test_clear() {
        let mut cache = IndexCache::default();
        let mut list = ShortList::new();
        list.append(100);
        cache.add(1, 0, &list);
        cache.add(2, 3, &ShortList::new());
        cache.clear();
        assert!(cache.without_prev.is_empty());
        assert!(cache.with_prev.is_empty());
    }

    #[test]
    fn test_add_and_lookup_without_prev() {
        let mut cache = IndexCache::default();
        let mut list = ShortList::new();
        list.append(100);
        cache.add(1, 0, &list);

        let result = cache.lookup_without_prev(false, 1);
        assert!(result.is_some());
        assert_eq!(result.unwrap().get(0), 100);

        assert!(cache.lookup_without_prev(false, 2).is_none());
    }

    #[test]
    fn test_add_and_lookup_with_prev() {
        let mut cache = IndexCache::default();
        let mut short_list = ShortList::new();
        short_list.append(200);
        short_list.append(201);
        cache.add(1, 2, &short_list);

        let result = cache.lookup_with_prev(false, 1);
        assert!(result.is_some());
        let (prev_k, list) = result.unwrap();
        assert_eq!(prev_k, 2);
        assert_eq!(list.get(0), 200);
    }

    #[test]
    fn test_lookup_without_prev_from_with_prev() {
        let mut cache = IndexCache::default();
        let mut short_list = ShortList::new();
        short_list.append(100);
        short_list.append((1 << 48) | 200); // This should be ignored in wo_prev lookup
        cache.add(1, 2, &short_list);

        let result = cache.lookup_without_prev(false, 1);
        assert!(result.is_some());
        let list = result.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list.get(0), 100);
    }
}
