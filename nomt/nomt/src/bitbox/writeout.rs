//! The writeout logic for bitbox.

// The logic for writeout is split into three parts:
// - first we write out the wal blob to the WAL file and wait for the MANIFEST to be synced.
// - then we write out the metabits and bucket pages to the HT file.
// - finally, we truncate the WAL file.

use std::{
    fs::File,
    io::{Seek as _, SeekFrom, Write},
    os::fd::AsRawFd as _,
    sync::Arc,
};

use crate::io::{FatPage, IoCommand, IoHandle, IoKind};

pub(super) fn write_wal(mut wal_fd: &File, wal_blob: &[u8]) -> std::io::Result<()> {
    wal_fd.set_len(0)?;
    wal_fd.seek(SeekFrom::Start(0))?;
    wal_fd.write_all(wal_blob)?;
    wal_fd.sync_all()?;
    Ok(())
}

/// Truncates the WAL file to zero length.
///
/// Conditionally syncs the file to disk.
pub(super) fn truncate_wal(mut wal_fd: &File, do_sync: bool) -> std::io::Result<()> {
    wal_fd.set_len(0)?;
    wal_fd.seek(SeekFrom::Start(0))?;
    if do_sync {
        wal_fd.sync_all()?;
    }
    Ok(())
}

pub(super) fn write_ht(
    io_handle: IoHandle,
    ht_fd: &File,
    mut ht: Vec<(u64, Arc<FatPage>)>,
) -> std::io::Result<()> {
    let mut sent = 0;

    ht.sort_unstable_by_key(|item| item.0);
    for (pn, page) in ht {
        io_handle
            .send(IoCommand {
                kind: IoKind::WriteArc(ht_fd.as_raw_fd(), pn, page),
                user_data: 0,
            })
            .unwrap();
        sent += 1;
    }

    while sent > 0 {
        io_handle.recv().unwrap();
        sent -= 1;
    }

    ht_fd.sync_all()?;

    Ok(())
}
