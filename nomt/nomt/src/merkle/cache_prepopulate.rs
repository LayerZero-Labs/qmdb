//! Utility for prepopulating the first N layers of the cache.

use std::io;

use crate::{
    io::IoHandle,
    page_cache::{PageCache, PageMut},
    store::{PageLoad, PageLoader, Store},
};

use nomt_core::page_id::{ChildPageIndex, PageId, MAX_PAGE_DEPTH, NUM_CHILDREN, ROOT_PAGE_ID};

/// Prepopulate the given number of levels of the page tree into the page cache.
///
/// This function blocks until the prepopulation has finished.
pub fn prepopulate(
    io_handle: IoHandle,
    page_cache: &PageCache,
    store: &Store,
    levels: usize,
) -> io::Result<()> {
    let page_loader = store.page_loader();
    let mut loads = Vec::new();

    let levels = std::cmp::min(levels, MAX_PAGE_DEPTH);

    // dispatch all page loads recursively.
    dispatch_recursive(ROOT_PAGE_ID, &page_loader, &io_handle, &mut loads, levels)?;

    let mut completed = 0;

    // wait on I/O results.
    while completed < loads.len() {
        // UNWRAP: we don't expect the I/O pool to go down. fatal error.
        let complete_io = io_handle.recv().expect("I/O Pool Down");
        complete_io.result?;
        let load_index = complete_io.command.user_data as usize;
        let load = &mut loads[load_index];

        // UNWRAP: all submitted requests are of kind Read(FatPage).
        if let Some((page, bucket)) = load.try_complete(complete_io.command.kind.unwrap_buf()) {
            completed += 1;
            page_cache.insert(
                load.page_id().clone(),
                PageMut::pristine_with_data(page).freeze(),
                bucket,
            );
        } else {
            // misprobe. try again.
            if !page_loader.probe(load, &io_handle, complete_io.command.user_data) {
                // guaranteed empty.
                completed += 1;
            }
        }
    }

    Ok(())
}

// dispatch page loads for all the children of the given page.
fn dispatch_recursive(
    page_id: PageId,
    page_loader: &PageLoader,
    io_handle: &IoHandle,
    loads: &mut Vec<PageLoad>,
    levels_remaining: usize,
) -> io::Result<()> {
    if levels_remaining == 0 {
        return Ok(());
    }

    for child_index in 0..NUM_CHILDREN {
        // UNWRAP: all indices up to NUM_CHILDREN are allowed.
        let child_index = ChildPageIndex::new(child_index as u8).unwrap();

        // UNWRAP: depth is not out of bounds and child index is valid.
        let child_page_id = page_id.child_page_id(child_index).unwrap();

        let mut page_load = page_loader.start_load(child_page_id.clone());

        let next_index = loads.len() as u64;
        if page_loader.probe(&mut page_load, io_handle, next_index) {
            // probe has been dispatched.
            loads.push(page_load);
            dispatch_recursive(
                child_page_id,
                page_loader,
                io_handle,
                loads,
                levels_remaining - 1,
            )?;
        }
    }

    Ok(())
}
