//! The writeout logic for beatree.

// As part of beatree writeout, we need to write BBN and LN files, resizing them to the correct
// size beforehand. After the writes are completed (fsync'd), we wait for the MANIFEST to be
// updated and then perform some cleanup.

use super::allocator::{PageNumber, Store};
use crate::io::{FatPage, IoHandle};

pub fn submit_freelist_write(
    io_handle: &IoHandle,
    store: &Store,
    free_list_pages: Vec<(PageNumber, FatPage)>,
) {
    for (pn, page) in free_list_pages {
        io_handle
            .send(crate::io::IoCommand {
                kind: crate::io::IoKind::Write(store.store_fd(), pn.0 as u64, page),
                user_data: 0,
            })
            .unwrap();
    }
}
