// Copyright 2020 Parity Technologies
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use super::{DBOp, DBTransaction, DBValue, MallocSizeOfDerive};
use malloc_size_of::MallocSizeOfOps;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

#[derive(Default, Clone, Copy)]
pub struct RawDbStats {
    pub reads: u64,
    pub writes: u64,
    pub bytes_written: u64,
    pub bytes_read: u64,
    pub transactions: u64,
    pub cache_hit_count: u64,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct RocksDbStatsTimeValue {
    /// 50% percentile
    pub p50: f64,
    /// 95% percentile
    pub p95: f64,
    /// 99% percentile
    pub p99: f64,
    /// 100% percentile
    pub p100: f64,
    pub sum: u64,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct RocksDbStatsValue {
    pub count: u64,
    pub times: Option<RocksDbStatsTimeValue>,
}

#[allow(unused)]
pub fn parse_rocksdb_stats(stats: &str) -> HashMap<String, RocksDbStatsValue> {
    stats
        .lines()
        .map(|line| parse_rocksdb_stats_row(line.splitn(2, ' ')))
        .collect()
}

fn parse_rocksdb_stats_row<'a>(
    mut iter: impl Iterator<Item = &'a str>,
) -> (String, RocksDbStatsValue) {
    const PROOF: &str = "rocksdb statistics format is valid and hasn't changed";
    const SEPARATOR: &str = " : ";
    let key = iter
        .next()
        .expect(PROOF)
        .trim_start_matches("rocksdb.")
        .to_owned();
    let values = iter.next().expect(PROOF);
    let value = if values.starts_with("COUNT") {
        // rocksdb.row.cache.hit COUNT : 0
        RocksDbStatsValue {
            count: u64::from_str(values.rsplit(SEPARATOR).next().expect(PROOF)).expect(PROOF),
            times: None,
        }
    } else {
        // rocksdb.db.get.micros P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
        let values: Vec<&str> = values.split_whitespace().filter(|s| *s != ":").collect();
        let times = RocksDbStatsTimeValue {
            p50: f64::from_str(values.get(1).expect(PROOF)).expect(PROOF),
            p95: f64::from_str(values.get(3).expect(PROOF)).expect(PROOF),
            p99: f64::from_str(values.get(5).expect(PROOF)).expect(PROOF),
            p100: f64::from_str(values.get(7).expect(PROOF)).expect(PROOF),
            sum: u64::from_str(values.get(11).expect(PROOF)).expect(PROOF),
        };
        RocksDbStatsValue {
            count: u64::from_str(values.get(9).expect(PROOF)).expect(PROOF),
            times: Some(times),
        }
    };
    (key, value)
}

impl RawDbStats {
    fn combine(&self, other: &RawDbStats) -> Self {
        RawDbStats {
            reads: self.reads + other.reads,
            writes: self.writes + other.writes,
            bytes_written: self.bytes_written + other.bytes_written,
            bytes_read: self.bytes_read + other.bytes_written,
            transactions: self.transactions + other.transactions,
            cache_hit_count: self.cache_hit_count + other.cache_hit_count,
        }
    }
}

struct OverallDbStats {
    stats: RawDbStats,
    last_taken: Instant,
    started: Instant,
}

impl OverallDbStats {
    fn new() -> Self {
        OverallDbStats {
            stats: RawDbStats::default(),
            last_taken: Instant::now(),
            started: Instant::now(),
        }
    }
}

pub struct RunningDbStats {
    reads: AtomicU64,
    writes: AtomicU64,
    bytes_written: AtomicU64,
    bytes_read: AtomicU64,
    transactions: AtomicU64,
    cache_hit_count: AtomicU64,
    overall: RwLock<OverallDbStats>,
}

pub struct TakenDbStats {
    pub raw: RawDbStats,
    pub started: Instant,
}

impl RunningDbStats {
    pub fn new() -> Self {
        Self {
            reads: 0.into(),
            bytes_read: 0.into(),
            writes: 0.into(),
            bytes_written: 0.into(),
            transactions: 0.into(),
            cache_hit_count: 0.into(),
            overall: OverallDbStats::new().into(),
        }
    }

    pub fn tally_reads(&self, val: u64) {
        self.reads.fetch_add(val, AtomicOrdering::Relaxed);
    }

    pub fn tally_bytes_read(&self, val: u64) {
        self.bytes_read.fetch_add(val, AtomicOrdering::Relaxed);
    }

    pub fn tally_writes(&self, val: u64) {
        self.writes.fetch_add(val, AtomicOrdering::Relaxed);
    }

    pub fn tally_bytes_written(&self, val: u64) {
        self.bytes_written.fetch_add(val, AtomicOrdering::Relaxed);
    }

    pub fn tally_transactions(&self, val: u64) {
        self.transactions.fetch_add(val, AtomicOrdering::Relaxed);
    }

    pub fn tally_cache_hit_count(&self, val: u64) {
        self.cache_hit_count.fetch_add(val, AtomicOrdering::Relaxed);
    }

    fn take_current(&self) -> RawDbStats {
        RawDbStats {
            reads: self.reads.swap(0, AtomicOrdering::Relaxed),
            writes: self.writes.swap(0, AtomicOrdering::Relaxed),
            bytes_written: self.bytes_written.swap(0, AtomicOrdering::Relaxed),
            bytes_read: self.bytes_read.swap(0, AtomicOrdering::Relaxed),
            transactions: self.transactions.swap(0, AtomicOrdering::Relaxed),
            cache_hit_count: self.cache_hit_count.swap(0, AtomicOrdering::Relaxed),
        }
    }

    fn peek_current(&self) -> RawDbStats {
        RawDbStats {
            reads: self.reads.load(AtomicOrdering::Relaxed),
            writes: self.writes.load(AtomicOrdering::Relaxed),
            bytes_written: self.bytes_written.load(AtomicOrdering::Relaxed),
            bytes_read: self.bytes_read.load(AtomicOrdering::Relaxed),
            transactions: self.transactions.load(AtomicOrdering::Relaxed),
            cache_hit_count: self.cache_hit_count.load(AtomicOrdering::Relaxed),
        }
    }

    pub fn since_previous(&self) -> TakenDbStats {
        let mut overall_lock = self.overall.write();

        let current = self.take_current();

        overall_lock.stats = overall_lock.stats.combine(&current);

        let stats = TakenDbStats {
            raw: current,
            started: overall_lock.last_taken,
        };

        overall_lock.last_taken = Instant::now();

        stats
    }

    pub fn overall(&self) -> TakenDbStats {
        let overall_lock = self.overall.read();

        let current = self.peek_current();

        TakenDbStats {
            raw: overall_lock.stats.combine(&current),
            started: overall_lock.started,
        }
    }
}

impl super::MallocSizeOf for RunningDbStats {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        0
    }
}

#[derive(Default, MallocSizeOfDerive)]
pub struct MyStat {
    ops: u64,
    enabled: bool,
    last_seen: HashMap<Vec<u8>, u64>,
    read_size: Vec<u64>,
    read_time: Vec<u64>,
    unread_time: Vec<u64>,
    write_time: Vec<u64>,
    write_size: Vec<u64>,
    seen_stat: Vec<u64>,
}

impl MyStat {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Default::default()
        }
    }

    pub fn record_read(
        &mut self,
        mut key: Vec<u8>,
        value: &Option<DBValue>,
        time: Duration,
        col: u32,
    ) {
        if !self.enabled {
            return;
        }
        self.ops += 1;
        key.extend_from_slice(&col.to_be_bytes());
        let since_last_seen = self
            .last_seen
            .insert(key.clone(), self.ops)
            .map_or(u64::MAX, |x| self.ops - x);
        self.seen_stat.push(since_last_seen);
        if since_last_seen < 1800 {
            // println!("col {}, seen {}", col, since_last_seen);
            // println!("key {:?}", key);
        }
        if value.is_none() {
            self.unread_time.push(time.as_nanos() as u64);
        } else {
            self.read_time.push(time.as_nanos() as u64);
            self.read_size.push(value.as_ref().unwrap().len() as u64);
        }
    }

    pub fn record_write(&mut self, tx: &DBTransaction, time: Duration) {
        if !self.enabled {
            return;
        }
        let DBTransaction { ops } = tx;
        if ops.len() == 0 {
            return;
        }
        let avg_op = (time.as_nanos() as usize / ops.len()) as u64;
        self.write_time.extend_from_slice(&vec![avg_op; ops.len()]);
        for t in ops.iter() {
            self.ops += 1;

            let key = t.key().to_vec();
            // println!("Write {:?}", &key);
            self.last_seen.insert(key, self.ops);
            match t {
                DBOp::Insert { value, .. } => self.write_size.push(value.len() as u64),
                DBOp::Delete { .. } => self.write_size.push(0),
            }
        }
    }

    fn report_vec(vec: &mut Vec<u64>, name: &str) {
        if vec.len() == 0 {
            return;
        }
        const TICKS: usize = 10;
        let mut indexes: Vec<usize> = (1..TICKS).map(|x| x * 100 / TICKS).collect();
        indexes.extend_from_slice(&[95, 98, 99]);
        vec.sort_unstable();
        let times: String = indexes
            .iter()
            .map(|idx| {
                let index = (vec.len() - 1) * idx / 100;
                format!("{:>5}: {:>5}", idx, prettier(vec[index]))
            })
            .collect();
        let avg = if name == "Last seen" {
            u64::MAX
        } else {
            (vec.iter().sum::<u64>() as f64 / vec.len() as f64) as u64
        };
        println!(
            "{} > Cnt {:>8}, Avg {:>8}. {}",
            times,
            vec.len(),
            prettier(avg),
            name
        );
        vec.clear();
    }

    pub fn report(&mut self) {
        if !self.enabled {
            return;
        }
        Self::report_vec(&mut self.read_size, "Non-empty read size");
        Self::report_vec(&mut self.read_time, "Non-empty read");
        Self::report_vec(&mut self.unread_time, "Empty read");
        Self::report_vec(&mut self.write_time, "Write time");
        Self::report_vec(&mut self.write_size, "Write size");
        Self::report_vec(&mut self.seen_stat, "Last seen");
    }
}

fn prettier(data: u64) -> String {
    if data == u64::MAX {
        "none".to_string()
    } else if data < 100_000 {
        format!("{}", data)
    } else if data < 10_000_000 {
        format!("{}k", data / 1000)
    } else {
        format!("{}m", data / 1000000)
    }
}
