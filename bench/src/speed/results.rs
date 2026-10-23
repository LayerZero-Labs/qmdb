use crate::cli::Cli;
use crate::speed::db_backend;
use memory_stats::memory_stats;
#[cfg(target_os = "linux")]
use procfs::Current;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub struct Update {
    pub count: u64,
    pub elapsed: f64,
    pub throughput: f64,
}

impl Update {
    pub fn new(count: u64, elapsed: f64, throughput: f64) -> Self {
        Self {
            count,
            elapsed,
            throughput,
        }
    }
}

impl std::fmt::Display for Update {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} in {:.2}s ({:.2}/s)",
            self.count, self.elapsed, self.throughput
        )
    }
}

#[derive(Debug, Default)]
pub struct Throughput {
    pub count: u64,
    pub elapsed: f64,
    pub throughput: f64,
    /// Throughput of the most recent record() call.
    pub last_throughput: f64,
}

impl Throughput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, count: u64, elapsed: Duration) -> Update {
        let elapsed_secs = elapsed.as_secs_f64();
        let throughput = count as f64 / elapsed_secs;
        self.count += count;
        self.elapsed += elapsed_secs;
        self.throughput = self.count as f64 / self.elapsed;
        self.last_throughput = throughput;
        Update::new(count, elapsed_secs, throughput)
    }

    pub fn record_as_total(&mut self, new_total: u64, elapsed: Duration) -> Update {
        self.record(new_total - self.count, elapsed)
    }
}

impl std::fmt::Display for Throughput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Update::new(self.count, self.elapsed, self.throughput).fmt(f)
    }
}

pub struct BenchmarkResults {
    pub entry_count: u64,
    pub db_dir: String,
    pub entries_in_db: u64,
    pub tputs: HashMap<String, Throughput>,
    pub start_time: Instant,
}

impl BenchmarkResults {
    pub fn new(config: &Cli) -> Self {
        Self {
            entry_count: config.entry_count,
            db_dir: config.db_dir.clone(),
            entries_in_db: 0,
            tputs: HashMap::from([
                ("updates".to_string(), Throughput::new()),
                ("inserts".to_string(), Throughput::new()),
                ("deletes".to_string(), Throughput::new()),
                ("reads".to_string(), Throughput::new()),
                ("block_population".to_string(), Throughput::new()),
            ]),
            start_time: Instant::now(),
        }
    }

    pub fn log_system_stats(&mut self) {
        self.log_memory_usage();
        self.log_db_space_usage();
        #[cfg(target_os = "linux")]
        self.log_cpu_stats();
    }

    pub fn log_memory_usage(&mut self) {
        if let Some(usage) = memory_stats() {
            println!(
                "MEM(entries={}) Memory usage: physical={:.2}GB virtual={:.2}GB; physical/entry={:.2}bytes",
                self.entries_in_db,
                usage.physical_mem as f64 / 1024.0 / 1024.0 / 1024.0,
                usage.virtual_mem as f64 / 1024.0 / 1024.0 / 1024.0,
                usage.physical_mem as f64 / self.entries_in_db as f64
            );
        } else {
            println!("Couldn't get the current memory usage :(");
        }
    }

    pub fn log_db_space_usage(&mut self) {
        // Use du to get disk usage if available
        let output = match std::process::Command::new("du")
            .arg("-s")
            .arg(self.db_dir.as_str())
            .output()
        {
            Ok(output) => output.stdout,
            Err(e) => {
                eprintln!("Failed to run 'du' command: {e}");
                return;
            }
        };

        // Expected output: "6708	/mnt/nvme/QMDB/ADS"
        let usage: u64 = String::from_utf8(output)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();

        println!(
            "SSD(entries={}) Usage: {:.2}GB, {:.2}bytes/entry",
            self.entries_in_db,
            usage as f64 / 1024.0 / 1024.0,
            usage as f64 / self.entries_in_db as f64
        );
    }

    #[cfg(target_os = "linux")]
    pub fn log_cpu_stats(&mut self) {
        // Using procfs, log CPU time used by process since start (user, system, total).
        // Then calculate CPU utilization percentage using time elapsed since start, and log that.
        let pid = std::process::id();
        let process = match procfs::process::Process::new(pid as i32) {
            Ok(proc) => proc,
            Err(e) => {
                eprintln!("Failed to get process info: {e}");
                return;
            }
        };

        let stat = match process.stat() {
            Ok(stat) => stat,
            Err(e) => {
                eprintln!("Failed to get process stat: {e}");
                return;
            }
        };

        let user_time = stat.utime as f64 / procfs::ticks_per_second() as f64;
        let system_time = stat.stime as f64 / procfs::ticks_per_second() as f64;
        let total_time = user_time + system_time;

        let elapsed = self.start_time.elapsed().as_secs_f64();
        let num_cores = procfs::CpuInfo::current().unwrap().num_cores();
        let cpu_utilization = (total_time / elapsed) * 100.0 / num_cores as f64;

        println!(
            "CPU(entries={}) User time: {:.2}s, System time: {:.2}s, Total time: {:.2}s, Utilization: {:.2}%",
            self.entries_in_db, user_time, system_time, total_time, cpu_utilization
        );
    }

    pub fn update_entries_in_db(&mut self, entries: u64) {
        self.entries_in_db = entries;
    }

    pub fn record_throughput(&mut self, operation_name: &str, count: u64, duration: Duration) {
        // Register phase names that are not predefined (the rw_read* mixed read/write sweep).
        let record = self
            .tputs
            .entry(operation_name.to_string())
            .or_insert_with(Throughput::new);
        let update = record.record(count, duration);
        println!(
            "{}@{} entries: {}, total: {}",
            operation_name, self.entries_in_db, update, record
        );
    }

    pub fn record_throughput_as_total(
        &mut self,
        operation_name: &str,
        new_total: u64,
        duration: Duration,
    ) {
        if let Some(record) = self.tputs.get_mut(operation_name) {
            let update = record.record_as_total(new_total, duration);
            println!(
                "{}@{} entries: {}, total: {}",
                operation_name, self.entries_in_db, update, record
            );
        }
    }

    pub fn print_summary(&self) {
        let elapsed = self.start_time.elapsed();
        println!(
            "Benchmarking summary ({:.1} seconds so far)",
            elapsed.as_secs_f64()
        );
        for name in ["block_population", "updates", "inserts", "deletes", "reads"] {
            let msg: String = if name == "block_population" {
                "populate DB with entries".to_string()
            } else {
                format!("benchmark {name}")
            };
            println!(
                " - Time to {}: {:.3?}",
                msg,
                Duration::from_secs_f64(self.tputs[name].elapsed)
            );
        }
        println!("Results summary");
        println!("\tentry_count: {}", self.entry_count);
        println!("\tdb_backend: {}", db_backend::NAME);
        println!("\telapsed_s: {:.3}", elapsed.as_secs_f64());
        println!("\telapsed: {elapsed:.3?}");
        let mut names: Vec<&String> = self.tputs.keys().collect();
        names.sort();
        for key in names {
            println!("\t{}: {:.3?}/s", key, self.tputs[key].last_throughput);
        }
    }
}
