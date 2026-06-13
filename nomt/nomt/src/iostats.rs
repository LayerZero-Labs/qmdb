//! Per-fd write byte counters for instrumented benchmarks.
use std::sync::atomic::{AtomicU64, Ordering};

pub static HT_PWRITE_BYTES: AtomicU64 = AtomicU64::new(0);
pub static LN_PWRITE_BYTES: AtomicU64 = AtomicU64::new(0);
pub static BBN_PWRITE_BYTES: AtomicU64 = AtomicU64::new(0);
pub static WAL_WRITE_BYTES: AtomicU64 = AtomicU64::new(0);
pub static META_WRITE_BYTES: AtomicU64 = AtomicU64::new(0);

pub fn add_ht(n: u64) { HT_PWRITE_BYTES.fetch_add(n, Ordering::Relaxed); }
pub fn add_ln(n: u64) { LN_PWRITE_BYTES.fetch_add(n, Ordering::Relaxed); }
pub fn add_bbn(n: u64) { BBN_PWRITE_BYTES.fetch_add(n, Ordering::Relaxed); }
pub fn add_wal(n: u64) { WAL_WRITE_BYTES.fetch_add(n, Ordering::Relaxed); }
pub fn add_meta(n: u64) { META_WRITE_BYTES.fetch_add(n, Ordering::Relaxed); }

/// (ht, ln, bbn, wal, meta) in bytes, then resets to 0.
pub fn dump_and_reset() -> (u64, u64, u64, u64, u64) {
    (
        HT_PWRITE_BYTES.swap(0, Ordering::Relaxed),
        LN_PWRITE_BYTES.swap(0, Ordering::Relaxed),
        BBN_PWRITE_BYTES.swap(0, Ordering::Relaxed),
        WAL_WRITE_BYTES.swap(0, Ordering::Relaxed),
        META_WRITE_BYTES.swap(0, Ordering::Relaxed),
    )
}
