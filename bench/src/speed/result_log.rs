//! Centralized "result log" + monitor-disk resolution for the benchmark.
//!
//! - KEY result metrics (per-phase throughput, OS physical R/W, MEM/SSD/CPU
//!   summary, final summary) are written via [`write_line`] to a dedicated
//!   metrics-log file selected by `--result-log`. If no file is set they fall
//!   back to stdout. Progress / process noise keeps using plain `println!`.
//! - The block device used for OS physical R/W is derived ONCE from the db dir
//!   (so a single `--db-dir` drives both the DB location AND the I/O counters),
//!   with an optional `--monitor-disk` override.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

static RESULT_LOG: OnceLock<Mutex<Option<File>>> = OnceLock::new();
static MONITOR_DISK: OnceLock<String> = OnceLock::new();

/// Initialize the metrics-log sink. Empty path => metrics fall back to stdout.
pub fn init_result_log(path: &str) {
    let file = if path.trim().is_empty() {
        None
    } else {
        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("[result_log] cannot open '{path}': {e}; metrics -> stdout");
                None
            }
        }
    };
    let _ = RESULT_LOG.set(Mutex::new(file));
}

/// Write one line of KEY result data to the metrics log (or stdout if unset).
pub fn write_line(line: &str) {
    if let Some(lock) = RESULT_LOG.get() {
        if let Ok(mut guard) = lock.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = writeln!(f, "{line}");
                let _ = f.flush();
                return;
            }
        }
    }
    println!("{line}");
}

/// Resolve, once, the /proc/diskstats device name for OS physical R/W.
/// Prefers an explicit override; otherwise derives it from `db_dir`'s mount.
pub fn init_monitor_disk(override_dev: &str, db_dir: &str) {
    let dev = if !override_dev.trim().is_empty() {
        override_dev.trim().to_string()
    } else {
        derive_disk_from_dir(db_dir)
    };
    if dev.is_empty() {
        eprintln!("[result_log] could not resolve monitor disk from '{db_dir}'; OS physical R/W = 0");
    } else {
        eprintln!("[result_log] monitor disk = {dev} (from {db_dir})");
    }
    let _ = MONITOR_DISK.set(dev);
}

/// The resolved diskstats device name (empty string if unresolved).
pub fn monitor_disk() -> &'static str {
    MONITOR_DISK.get().map(|s| s.as_str()).unwrap_or("")
}

/// `findmnt -n -o SOURCE --target <dir>` => "/dev/md0" => "md0".
fn derive_disk_from_dir(dir: &str) -> String {
    let out = std::process::Command::new("findmnt")
        .args(["-n", "-o", "SOURCE", "--target", dir])
        .output();
    if let Ok(o) = out {
        if let Ok(s) = String::from_utf8(o.stdout) {
            // SOURCE may be "/dev/md0" or "/dev/sda[/subvol]"; take the device basename.
            let dev = s.trim().split('[').next().unwrap_or("").trim();
            if let Some(name) = dev.rsplit('/').next() {
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    String::new()
}
