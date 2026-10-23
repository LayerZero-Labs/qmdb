use crossbeam_channel::Receiver;
use imbl::OrdMap;
use threadpool::ThreadPool;

use std::{collections::BTreeMap, sync::Arc};

use crate::beatree::{
    allocator::{PageNumber, Store, StoreReader},
    branch::BRANCH_NODE_BODY_SIZE,
    index::Index,
    leaf::node::LEAF_NODE_BODY_SIZE,
    leaf_cache::LeafCache,
    ops::get_key,
    Key, SyncData, ValueChange,
};
use crate::io::{IoHandle, PagePool};
use crate::task::{spawn_task, TaskResult};

mod branch_ops;
mod branch_stage;
mod branch_updater;
mod extend_range_protocol;
mod leaf_stage;
mod leaf_updater;

#[cfg(test)]
mod tests;

// All nodes less than this body size will be merged with a neighboring node.
const BRANCH_MERGE_THRESHOLD: usize = BRANCH_NODE_BODY_SIZE / 2;

// At 180% of the branch size, we perform a 'bulk split' which follows a different algorithm
// than a simple split. Bulk splits are encountered when there are a large number of insertions
// on a single node, typically when inserting into a fresh database.
const BRANCH_BULK_SPLIT_THRESHOLD: usize = (BRANCH_NODE_BODY_SIZE * 9) / 5;
// When performing a bulk split, we target 75% fullness for all of the nodes we create except the
// last.
const BRANCH_BULK_SPLIT_TARGET: usize = (BRANCH_NODE_BODY_SIZE * 3) / 4;

const LEAF_MERGE_THRESHOLD: usize = LEAF_NODE_BODY_SIZE / 2;
const LEAF_BULK_SPLIT_THRESHOLD: usize = (LEAF_NODE_BODY_SIZE * 9) / 5;
const LEAF_BULK_SPLIT_TARGET: usize = (LEAF_NODE_BODY_SIZE * 3) / 4;

/// Change the btree in the specified way. Updates the branch index in-place.
///
/// The changeset is a list of key value pairs to be added or removed from the btree.
pub fn update(
    changeset: OrdMap<Key, ValueChange>,
    mut bbn_index: Index,
    leaf_cache: LeafCache,
    leaf_store: Store,
    bbn_store: Store,
    page_pool: PagePool,
    io_handle: IoHandle,
    thread_pool: ThreadPool,
    workers: usize,
) -> std::io::Result<(SyncData, Index, Receiver<TaskResult<()>>)> {
    let leaf_reader = StoreReader::new(leaf_store.clone(), page_pool.clone());
    let (leaf_writer, leaf_finisher) = leaf_store.start_sync();
    let (bbn_writer, bbn_finisher) = bbn_store.start_sync();

    let leaf_stage_outputs = leaf_stage::run(
        &bbn_index,
        leaf_cache.clone(),
        leaf_reader,
        leaf_writer,
        io_handle.clone(),
        changeset,
        thread_pool.clone(),
        workers,
    )?;

    let branch_stage_outputs = branch_stage::run(
        &mut bbn_index,
        bbn_writer,
        page_pool.clone(),
        io_handle.clone(),
        leaf_stage_outputs.leaf_changeset,
        thread_pool.clone(),
        workers,
    )?;

    let (ln_freelist_pages, ln_meta) =
        leaf_finisher.finish(&page_pool, leaf_stage_outputs.freed_pages)?;

    let (bbn_freelist_pages, bbn_meta) =
        bbn_finisher.finish(&page_pool, branch_stage_outputs.freed_pages)?;

    let mut total_io = leaf_stage_outputs.submitted_io + branch_stage_outputs.submitted_io;
    total_io += ln_freelist_pages.len();
    total_io += bbn_freelist_pages.len();

    crate::beatree::writeout::submit_freelist_write(&io_handle, &leaf_store, ln_freelist_pages);
    crate::beatree::writeout::submit_freelist_write(&io_handle, &bbn_store, bbn_freelist_pages);

    // make sure that all write requests succeeded.
    for _ in 0..total_io {
        // UNWRAP: we receive only what we sent. No `RecvErr` expected.
        io_handle.recv().unwrap().result?;
    }

    let (tx, rx) = crossbeam_channel::bounded(1);
    let post_io_task = move || {
        leaf_stage_outputs.post_io_work.run(&leaf_cache);
        drop(branch_stage_outputs.post_io_drop);
        leaf_cache.evict();
    };
    spawn_task(&thread_pool, post_io_task, tx);

    Ok((
        SyncData {
            ln_freelist_pn: ln_meta.freelist_pn,
            ln_bump: ln_meta.bump,
            bbn_freelist_pn: bbn_meta.freelist_pn,
            bbn_bump: bbn_meta.bump,
        },
        bbn_index,
        rx,
    ))
}

/// Container of possible changes made to a node
pub struct ChangedNodeEntry<Node> {
    /// PageNumber of the Node that is being replaced by the current entry
    pub deleted: Option<PageNumber>,
    /// New or modified Node that will be written
    pub inserted: Option<(Arc<Node>, PageNumber)>,
    /// Separator of the next node.
    pub next_separator: Option<Key>,
}

/// Tracker of all changes that happen to the nodes during an update
pub struct NodesTracker<Node> {
    /// Elements being tracked by the NodesTracker, each Separator
    /// is associated with a ChangedNodeEntry
    pub inner: BTreeMap<Key, ChangedNodeEntry<Node>>,
    /// Pending base received from the right worker which will be used as new base
    pub pending_base: Option<(Key, Arc<Node>, Option<Key>)>,
    /// Page numbers which were allocated and discarded entirely during this update.
    pub extra_freed: Vec<PageNumber>,
    /// Pages which are potentially being written and which must be kept alive until I/O
    /// is guaranteed complete.
    pub deferred_drop_pages: Vec<Arc<Node>>,
    pub new_inserted: usize,
}

impl<Node> NodesTracker<Node> {
    /// Create a new NodesTracker
    pub fn new() -> Self {
        Self {
            inner: BTreeMap::new(),
            pending_base: None,
            extra_freed: Vec::new(),
            new_inserted: 0,
            deferred_drop_pages: Vec::new(),
        }
    }

    /// Add or modify a ChangedNodeEntry specifying a deleted PageNumber.
    /// If the entry is already present, it cannot be associated with another deleted PageNumber.
    pub fn delete(&mut self, key: Key, pn: PageNumber, next_separator: Option<Key>) {
        let entry = self.inner.entry(key).or_insert(ChangedNodeEntry {
            deleted: None,
            inserted: None,
            next_separator,
        });

        // we can only delete a node once.
        assert!(entry.deleted.is_none());

        entry.deleted.replace(pn);
        entry.next_separator = next_separator;
    }

    /// Add or modify a ChangedNodeEntry specifying an inserted Node.
    pub fn insert(
        &mut self,
        key: Key,
        node: Arc<Node>,
        next_separator: Option<Key>,
        pn: PageNumber,
    ) {
        let entry = self.inner.entry(key).or_insert(ChangedNodeEntry {
            deleted: None,
            inserted: None,
            next_separator,
        });

        entry.next_separator = next_separator;
        entry.inserted.replace((node, pn));

        self.new_inserted += 1;
    }

    /// Set the new pending base node.
    pub fn set_pending_base(
        &mut self,
        separator: Key,
        node: Arc<Node>,
        cutoff: Option<Key>,
        pn: PageNumber,
    ) {
        self.extra_freed.push(pn);
        self.deferred_drop_pages.push(node.clone());
        self.pending_base = Some((separator, node, cutoff));
    }
}
