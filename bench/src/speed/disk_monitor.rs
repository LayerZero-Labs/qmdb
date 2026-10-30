//! Resolves the block device whose /proc/diskstats counters give the OS physical
//! read/write bytes of each phase. It is derived once from the database directory's
//! mount, so one `--db-dir` drives both the database location and the I/O counters;
//! `--monitor-disk` overrides it.

use std::sync::OnceLock;

static MONITOR_DISK: OnceLock<String> = OnceLock::new();

/// Resolve, once, the /proc/diskstats device name for OS physical R/W.
/// Prefers an explicit override; otherwise derives it from `db_dir`'s mount.
pub fn init_monitor_disk(override_dev: &str, db_dir: &str) {
    let dev = if !override_dev.trim().is_empty() {
        override_dev.trim().to_string()
    } else {
        derive_disk_from_dir(db_dir)
    };
    if dev.is_empty() {
        eprintln!("[disk_monitor] could not resolve monitor disk from '{db_dir}'; OS physical R/W = 0");
    } else {
        eprintln!("[disk_monitor] monitor disk = {dev} (from {db_dir})");
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
