//! This module provides a cross-platform advisory lock on a directory.

use std::{
    fs::{File, OpenOptions},
    path::Path,
};

/// Represents a cross-platform advisory lock on a directory.
pub struct Flock {
    lock_fd: File,
}

impl Flock {
    pub fn lock(db_dir: &Path, lock_filename: &str) -> anyhow::Result<Self> {
        let lock_path = db_dir.join(lock_filename);

        let lock_fd = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(lock_path)?;

        match crate::sys::unix::try_lock_exclusive(&lock_fd) {
            Ok(_) => Ok(Self { lock_fd }),
            Err(e) => {
                anyhow::bail!("Failed to lock directory: {e}");
            }
        }
    }
}

impl Drop for Flock {
    fn drop(&mut self) {
        if let Err(e) = crate::sys::unix::unlock(&self.lock_fd) {
            eprintln!("Failed to unlock directory lock: {e}");
        }
    }
}
