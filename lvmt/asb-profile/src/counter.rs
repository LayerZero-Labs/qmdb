use super::profiler::Profiler;
use asb_options::Options;
use authdb::AuthDB;

use kvdb::IoStatsKind;
use lazy_static::lazy_static;
use num_format::{Locale, WriteFormatted};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use simple_process_stats::ProcessStats;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::sync::Mutex;
use std::time::Instant;
use tokio::runtime::Runtime;

lazy_static! {
    pub static ref RUNTIME: Mutex<Runtime> = Mutex::new(Runtime::new().unwrap());
}

pub struct Reporter<'a> {
    pub start_time: Instant,
    total_read_count: usize,
    total_write_count: usize,
    log_file: Option<File>,

    round_start_time: Instant,
    round_start_read_count: usize,
    round_start_write_count: usize,

    empty_reads: usize,

    opts: &'a Options,
    counter: Box<dyn CounterTrait>,
}

impl<'a> Reporter<'a> {
    pub fn new(opts: &'a Options) -> Self {
        let log_file = if let Some(ref path) = opts.report_dir {
            let file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path.to_string() + "/timing.log")
                .unwrap();
            Some(file)
        } else {
            None
        };

        Reporter {
            start_time: Instant::now(),
            round_start_time: Instant::now(),
            log_file,
            opts,
            counter: Box::new(Counter::default()),
            empty_reads: 0,
            total_read_count: 0,
            total_write_count: 0,
            round_start_read_count: 0,
            round_start_write_count: 0,
        }
    }

    pub fn set_counter(&mut self, counter: Box<dyn CounterTrait>) {
        self.counter = counter;
    }

    pub fn start(&mut self) {
        self.start_time = Instant::now();
        self.round_start_time = Instant::now();
        self.counter.reset();
    }

    pub fn notify_empty_read(&mut self) {
        self.empty_reads += 1;
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub async fn report_mem() {
        let process_stats = ProcessStats::get().await.unwrap();
        println!(
            "Memory {:>3.3} bytes",
            (process_stats.memory_usage_bytes as f64) / ((1 << 30) as f64)
        );
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    pub async fn report_mem() {}

    pub fn notify_epoch(
        &mut self,
        epoch: usize,
        read_count: usize,
        write_count: usize,
        db: &dyn AuthDB,
        opts: &Options,
    ) {
        fn c(n: u64) -> String {
            let mut ans = String::new();
            ans.write_formatted(&n, &Locale::en).unwrap();
            ans
        }

        self.total_read_count += read_count;
        self.total_write_count += write_count;

        if (epoch + 1) % self.opts.report_epoch != 0 {
            return;
        }

        let read_count = self.total_read_count - self.round_start_read_count;
        let write_count = self.total_write_count - self.round_start_write_count;

        let last = self.round_start_time.elapsed();
        let avg_time = last.as_secs_f64() / (read_count + write_count) as f64;

        let common = format!(
            "{:>6?}: {:>7.3?} s > {:>7} ops, {:>7.3?} us/op, {:>5} empty reads >",
            epoch + 1,
            self.start_time.elapsed().as_secs_f64(),
            c((1f64 / avg_time) as u64),
            avg_time * 1e6,
            self.empty_reads,
        );

        if opts.stat_mem {
            RUNTIME.lock().unwrap().block_on(Self::report_mem());
        }

        // let db_stat = {
        //     let stats = db.backend().io_stats(IoStatsKind::SincePrevious);
        //     let bytes_per_read = (stats.bytes_read as f64) / (stats.reads as f64);
        //     let bytes_per_write = (stats.bytes_written as f64) / (stats.writes as f64);
        //     let cached_rate = (stats.cache_reads as f64) / (stats.reads as f64);
        //     format!(
        //         "{} / {} r ({:.0}% cached) {} w, avg bytes {:.2}, {:.2} >",
        //         c(stats.reads),
        //         c(stats.cache_reads),
        //         cached_rate * 100.0,
        //         c(stats.writes),
        //         bytes_per_read,
        //         bytes_per_write,
        //     )
        // };
        let (stdout, fileout) = {
            if let Some(backend) = db.backend() {
                let stats = backend.io_stats(IoStatsKind::SincePrevious);
                let ra = stats.reads as f64 / (read_count as f64);
                let wa = stats.writes as f64 / (write_count as f64);
                (
                    format!("Read amp {:>6.3}, Write amp {:>6.3} > ", ra, wa),
                    format!("{},{}", ra, wa),
                )
            } else {
                ("".into(), "".into())
            }
        };
        let customized = self.counter.report();
        println!("{} {} {}", common, stdout, customized);

        if let Some(file) = &mut self.log_file {
            let _ = writeln!(
                file,
                "{},{},{:.3?},{}",
                self.opts.settings(),
                (epoch + 1) / self.opts.report_epoch,
                avg_time * 1e6,
                fileout
            );
        }
        self.empty_reads = 0;
        self.round_start_time = Instant::now();
        self.round_start_read_count = self.total_read_count;
        self.round_start_write_count = self.total_write_count;
    }

    pub fn collect_profiling(&self, profiler: Profiler) {
        if self.opts.report_dir.is_none() {
            return;
        }

        let profile_prefix = self.opts.report_dir.as_ref().unwrap().clone()
            + "/"
            + &str::replace(&self.opts.settings(), ",", "_");
        profiler.report_to_file(&profile_prefix)
    }
}

pub trait CounterTrait {
    fn reset(&mut self) {}
    fn report(&mut self) -> String {
        "".to_string()
    }
}

#[derive(Default)]
pub struct Counter;

impl CounterTrait for Counter {}
