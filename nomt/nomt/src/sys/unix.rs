//! Common Unix definitions.

use std::{fs::File, os::fd::AsRawFd as _};

pub fn try_lock_exclusive(file: &File) -> std::io::Result<()> {
    cvt_r(|| unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }).map(drop)
}

pub fn unlock(file: &File) -> std::io::Result<()> {
    unsafe { cvt_r(|| libc::flock(file.as_raw_fd(), libc::LOCK_UN)).map(drop) }
}

pub(super) fn cvt_r<F>(mut f: F) -> std::io::Result<i32>
where
    F: FnMut() -> i32,
{
    fn cvt(res: i32) -> std::io::Result<i32> {
        if res == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }

    loop {
        match cvt(f()) {
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => (),
            other => break other,
        }
    }
}
