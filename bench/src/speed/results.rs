use crate::cli::Cli;
use crate::speed::db_backend;
use crate::speed::result_log;
use crate::speed::metrics::LatencyStats;
use chrono::{DateTime, Local};
use memory_stats::memory_stats;
#[cfg(target_os = "linux")]
use procfs::Current;
use serde::Serialize;
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

#[derive(Serialize, Debug)]
pub struct Throughput {
    pub history: Vec<(DateTime<Local>, f64, u64, f64, u64)>,
    pub count: u64,
    pub elapsed: f64,
    pub throughput: f64,
}

impl Default for Throughput {
    fn default() -> Self {
        Self::new()
    }
}

impl Throughput {
    pub fn new() -> Self {
        Self {
            history: Vec::new(),
            count: 0,
            elapsed: 0.0,
            throughput: 0.0,
        }
    }

    pub fn record(&mut self, count: u64, elapsed: Duration, db_entries: u64) -> Update {
        let elapsed_secs = elapsed.as_secs_f64();
        let throughput = count as f64 / elapsed_secs;
        self.history
            .push((Local::now(), elapsed_secs, count, throughput, db_entries));
        self.count += count;
        self.elapsed += elapsed_secs;
        self.throughput = self.count as f64 / self.elapsed;
        Update::new(count, elapsed_secs, throughput)
    }

    pub fn record_as_total(
        &mut self,
        new_total: u64,
        elapsed: Duration,
        db_entries: u64,
    ) -> Update {
        self.record(new_total - self.count, elapsed, db_entries)
    }

    pub fn get_last_throughput(&self) -> f64 {
        if let Some(last) = self.history.last() {
            last.3
        } else {
            0.0
        }
    }
}

impl std::fmt::Display for Throughput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Update::new(self.count, self.elapsed, self.throughput).fmt(f)
    }
}

#[derive(Serialize, Debug)]
pub struct Samples {
    pub history: Vec<(DateTime<Local>, u64, f64)>,
    pub values: Vec<f64>,
    pub total: f64,
    pub mean: f64,
}

impl Default for Samples {
    fn default() -> Self {
        Self::new()
    }
}

impl Samples {
    pub fn new() -> Self {
        Self {
            history: Vec::new(),
            values: Vec::new(),
            total: 0.0,
            mean: 0.0,
        }
    }

    pub fn add(&mut self, value: f64, entry_count: u64) {
        self.history.push((Local::now(), entry_count, value));
        self.total += value;
        self.mean = self.total / self.history.len() as f64;
    }
}

impl std::fmt::Display for Samples {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.mean)
    }
}

#[derive(Serialize, Debug)]
pub struct BenchmarkResults {
    pub complete: bool,
    pub success: bool,
    pub elapsed_s: f64,
    pub summary: HashMap<String, f64>,
    pub db_backend: String,
    pub config: Cli,
    pub entries_in_db: u64,
    pub tputs: HashMap<String, Throughput>,
    pub samples: HashMap<String, Samples>,
    pub latencies: HashMap<String, LatencyStats>,
    pub timestamps: HashMap<String, String>,
    pub hardware_setup: HashMap<String, f64>,
    #[serde(skip_serializing)]
    pub start_time: Instant,
}

impl BenchmarkResults {
    pub fn new(config: &Cli) -> Self {
        let window = Duration::from_secs(60); // 60-second sliding window
        #[allow(unused_mut)]
        let mut results = Self {
            complete: false,
            success: false,
            summary: HashMap::new(),
            db_backend: "UNKNOWN".to_string(),
            start_time: Instant::now(),
            elapsed_s: 0.0,
            config: config.clone(),
            hardware_setup: HashMap::new(),
            tputs: HashMap::from([
                ("updates".to_string(), Throughput::new()),
                ("inserts".to_string(), Throughput::new()),
                ("deletes".to_string(), Throughput::new()),
                ("reads".to_string(), Throughput::new()),
                ("transactions".to_string(), Throughput::new()),
                ("block_population".to_string(), Throughput::new()),
            ]),
            samples: HashMap::from([
                ("memory_usage_physical".to_string(), Samples::new()),
                ("memory_usage_virtual".to_string(), Samples::new()),
            ]),
            latencies: HashMap::from([(
                "reads".to_string(),
                LatencyStats::with_duration_window(window),
            )]),
            entries_in_db: 0,
            timestamps: HashMap::new(),
        };
        #[cfg(target_os = "linux")]
        results.log_system_setup();
        results
    }

    pub fn write_to_file(&mut self, filename: &str) {
        self.elapsed_s = self.start_time.elapsed().as_secs_f64();
        self.summary.clear();
        for (key, value) in &self.tputs {
            let k = format!("{key}.throughput_per_sec");
            let v = value.get_last_throughput();

            // ★ 直接打印
            println!("{k}: {v}");

            // ★ 写入 summary JSON
            self.summary.insert(k, v);
        }

        if let Some(stats) = self.latencies.get("reads") {
            if let Some(detailed_stats) = stats.detailed_stats() {
                self.summary.insert(
                    "reads.latency_s".to_string(),
                    detailed_stats.mean.as_secs_f64(),
                );
            } else {
                self.summary.insert("reads.latency_s".to_string(), 0.0);
            }
        } else {
            self.summary.insert("reads.latency_s".to_string(), 0.0);
        }

        self.log_time("results_written");
        match std::fs::File::create(filename) {
            Ok(file) => {
                if let Err(e) = serde_json::to_writer_pretty(file, self) {
                    eprintln!("Failed to write results to file: {e}");
                }
            }
            Err(e) => eprintln!("Failed to create file: {e}"),
        }
    }

    pub fn log_sample(&mut self, sample_name: &str, value: f64) {
        // Create if not exists
        if !self.samples.contains_key(sample_name) {
            self.samples.insert(sample_name.to_string(), Samples::new());
        }
        self.samples
            .get_mut(sample_name)
            .unwrap()
            .add(value, self.entries_in_db);
    }

    #[cfg(target_os = "linux")]
    pub fn log_system_setup(&mut self) {
        // Use procfs to log certain aspects of the system setup, e.g., CPU cores, memory capacity
        let cpu_info = procfs::CpuInfo::current().unwrap();
        let num_cores = cpu_info.num_cores();
        let memory_capacity = procfs::Meminfo::current().unwrap().mem_total;
        self.hardware_setup
            .insert("cpu_cores".to_string(), num_cores as f64);
        self.hardware_setup
            .insert("memory_capacity".to_string(), memory_capacity as f64);
    }

    pub fn log_system_stats(&mut self) {
        // TODO: Handle errors within this function instead.
        self.log_memory_usage();
        self.log_db_space_usage();
        #[cfg(target_os = "linux")]
        {
            self.log_disk_stats();
            self.log_io_stats();
            self.log_cpu_stats();
        }
    }

    pub fn log_memory_usage(&mut self) {
        if let Some(usage) = memory_stats() {
            self.log_sample("memory_usage_physical", usage.physical_mem as f64);
            self.log_sample("memory_usage_virtual", usage.virtual_mem as f64);
            result_log::write_line(&format!(
                "MEM(entries={}) Memory usage: physical={:.2}GB virtual={:.2}GB; physical/entry={:.2}bytes",
                self.entries_in_db,
                usage.physical_mem as f64 / 1024.0 / 1024.0 / 1024.0,
                usage.virtual_mem as f64 / 1024.0 / 1024.0 / 1024.0,
                usage.physical_mem as f64 / self.entries_in_db as f64
            ));
        } else {
            println!("Couldn't get the current memory usage :(");
        }
    }

    pub fn log_db_space_usage(&mut self) {
        let db_dir = self.config.db_dir.as_str();
        // Use du to get disk usage if available
        let output = match std::process::Command::new("du")
            .arg("-s")
            .arg(db_dir)
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

        self.log_sample("db_space_usage", usage as f64);
        result_log::write_line(&format!(
            "SSD(entries={}) Usage: {:.2}GB, {:.2}bytes/entry",
            self.entries_in_db,
            usage as f64 / 1024.0 / 1024.0,
            usage as f64 / self.entries_in_db as f64
        ));
    }

    pub fn log_io_stats(&mut self) {
        // Parse /proc/{pid}/io and add to samples
        // Example:
        // rchar: 17908800645
        // wchar: 3626499201
        // syscr: 50125473
        // syscw: 25824122
        // read_bytes: 4096
        // write_bytes: 3542319104
        // cancelled_write_bytes: 719708160
        let pid: u32 = std::process::id();
        let io_path = format!("/proc/{pid}/io");
        let io_contents = match std::fs::read_to_string(&io_path) {
            Ok(contents) => contents,
            Err(e) => {
                eprintln!("Failed to read {io_path}: {e}");
                return;
            }
        };
        let lines = io_contents.lines();
        for line in lines {
            let parts = line.split_whitespace().collect::<Vec<&str>>();
            let key = parts[0];
            let value = match parts[1].parse::<u64>() {
                Ok(val) => val,
                Err(e) => {
                    eprintln!("Failed to parse value in line '{line}': {e}");
                    continue;
                }
            };
            self.log_sample(&format!("iostats.{key}"), value as f64);
        }
    }

    #[cfg(target_os = "linux")]
    pub fn log_disk_stats(&mut self) {
        // Use procfs to read disk stats
        let diskstats = match procfs::diskstats() {
            Ok(stats) => stats,
            Err(e) => {
                eprintln!("Failed to read disk stats: {e}");
                return;
            }
        };

        for disk in diskstats {
            let read_bytes = disk.sectors_read * 512; // Assuming 512 bytes per sector
            let write_bytes = disk.sectors_written * 512;
            let utilization = (disk.time_in_progress as f64) / 1000.0; // Convert ms to seconds

            self.log_sample(
                &format!("diskstats.{}.read_ios", disk.name),
                disk.reads as f64,
            );
            self.log_sample(
                &format!("diskstats.{}.write_ios", disk.name),
                disk.writes as f64,
            );
            self.log_sample(
                &format!("diskstats.{}.sectors_read", disk.name),
                disk.sectors_read as f64,
            );
            self.log_sample(
                &format!("diskstats.{}.sectors_written", disk.name),
                disk.sectors_written as f64,
            );
            self.log_sample(
                &format!("diskstats.{}.read_bytes", disk.name),
                read_bytes as f64,
            );
            self.log_sample(
                &format!("diskstats.{}.write_bytes", disk.name),
                write_bytes as f64,
            );
            self.log_sample(&format!("diskstats.{}.utilization", disk.name), utilization);
        }
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

        result_log::write_line(&format!(
            "CPU(entries={}) User time: {:.2}s, System time: {:.2}s, Total time: {:.2}s, Utilization: {:.2}%",
            self.entries_in_db, user_time, system_time, total_time, cpu_utilization
        ));

        self.log_sample("cpu_user_time", user_time);
        self.log_sample("cpu_system_time", system_time);
        self.log_sample("cpu_total_time", total_time);
        self.log_sample("cpu_utilization", cpu_utilization);
    }

    pub fn log_time(&mut self, event_name: &str) {
        let now = Local::now().to_rfc3339();
        self.timestamps.insert(event_name.to_string(), now);
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
        let update = record.record(count, duration, self.entries_in_db);
        let line = format!(
            "{}@{} entries: {}, total: {}",
            operation_name, self.entries_in_db, update, record
        );
        // Populate progress -> stdout; hover-phase results -> metrics log.
        if operation_name == "block_population" || operation_name == "fresh_inserts" {
            println!("{line}");
        } else {
            result_log::write_line(&line);
        }
    }

    pub fn record_throughput_as_total(
        &mut self,
        operation_name: &str,
        new_total: u64,
        duration: Duration,
    ) {
        if let Some(record) = self.tputs.get_mut(operation_name) {
            let update = record.record_as_total(new_total, duration, self.entries_in_db);
            let line = format!(
                "{}@{} entries: {}, total: {}",
                operation_name, self.entries_in_db, update, record
            );
            // Populate progress -> stdout; hover-phase results -> metrics log.
            if operation_name == "block_population" || operation_name == "fresh_inserts" {
                println!("{line}");
            } else {
                result_log::write_line(&line);
            }
        }
    }

    pub fn print_summary(&self) {
        result_log::write_line(&format!(
            "Benchmarking summary ({:.1} seconds so far)",
            self.elapsed_s
        ));
        for name in ["block_population", "updates", "inserts", "deletes", "reads"] {
            let msg: String = if name == "block_population" {
                "populate DB with entries".to_string()
            } else {
                format!("benchmark {name}")
            };
            result_log::write_line(&format!(
                " - Time to {}: {:.3?}",
                msg,
                Duration::from_secs_f64(self.tputs[name].elapsed)
            ));
        }
        let latency = self.latencies.get("reads").unwrap();
        let total_samples: usize = latency.get_total_samples();
        let total_latency = latency.get_total_latency();
        result_log::write_line(&format!(
            " - Time taken to benchmark read latency: {total_latency:.3?} ({total_samples} samples)"
        ));

        result_log::write_line("Results summary");
        result_log::write_line(&format!("\tentry_count: {}", self.config.entry_count));
        result_log::write_line(&format!("\tdb_backend: {}", db_backend::NAME));
        result_log::write_line(&format!("\telapsed_s: {:.3}", self.elapsed_s));
        result_log::write_line(&format!(
            "\telapsed: {:.3?}",
            Duration::from_secs_f64(self.elapsed_s)
        ));
        result_log::write_line(&format!("\tsuccess: {}", self.success));
        for (key, value) in &self.tputs {
            if key == "transactions" {
                continue; // TPS benchmark removed
            }
            result_log::write_line(&format!("\t{}: {:.3?}/s", key, value.get_last_throughput()));
        }
        if let Some(stats) = self.latencies.get("reads") {
            if let Some(detailed_stats) = stats.detailed_stats() {
                detailed_stats.print("Read", true);
            } else {
                result_log::write_line("No read latency data available");
            }
        } else {
            result_log::write_line("No read latency data available");
        }
    }
}

