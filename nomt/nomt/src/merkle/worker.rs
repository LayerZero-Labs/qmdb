//! Worker logic.
//!
//! During the update phase, each worker gets a range of the keys
//! to work on, pre-fetches pages for all keys within that range, and performs
//! page updates.
//!
//! This module also exposes a warm-up worker, which can be used to pre-fetch pages before the
//! update command is issued.
//!
//! Updates are performed while the next fetch is pending, unless all fetches in
//! the range have completed.

use crossbeam::channel::{Receiver, Select, TryRecvError};

use nomt_core::{
    page_id::ROOT_PAGE_ID,
    proof::PathProofTerminal,
    trie::{KeyPath, Node, ValueHash},
};

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use super::{
    page_set::{FrozenSharedPageSet, PageOrigin, PageSet},
    page_walker::{Output, PageSet as _, PageWalker},
    seek::{Seek, Seeker},
    KeyReadWrite, LiveOverlay, RootPagePending, UpdateCommand, UpdateShared, WarmUpCommand,
    WarmUpOutput, WorkerOutput,
};

use crate::{
    io::PagePool,
    page_cache::{PageCache, ShardIndex},
    page_region::PageRegion,
    rw_pass_cell::WritePass,
    store::Store,
    HashAlgorithm, PathProof, WitnessedPath,
};

pub(super) struct UpdateParams {
    pub page_cache: PageCache,
    pub page_pool: PagePool,
    pub store: Store,
    pub root: Node,
    pub warm_ups: Arc<HashMap<KeyPath, Seek>>,
    pub warm_page_set: Option<FrozenSharedPageSet>,
    pub command: UpdateCommand,
}

pub(super) struct WarmUpParams {
    pub page_cache: PageCache,
    pub overlay: LiveOverlay,
    pub store: Store,
    pub root: Node,
}

pub(super) fn run_warm_up<H: HashAlgorithm>(
    params: WarmUpParams,
    warmup_rx: Receiver<WarmUpCommand>,
    finish_rx: Receiver<()>,
) -> std::io::Result<WarmUpOutput> {
    let page_loader = params.store.page_loader();
    let io_handle = params.store.io_pool().make_handle();
    let page_pool = params.store.io_pool().page_pool().clone();
    let page_io_receiver = io_handle.receiver().clone();

    // We always run with `WithoutDependents` here, and the mode is adjusted later, during `update`.
    let page_set = PageSet::new(page_pool, None);

    let seeker = Seeker::<H>::new(
        params.root,
        params.store.read_transaction(),
        params.page_cache,
        params.overlay,
        io_handle,
        page_loader,
        true,
    );

    warm_up_phase(page_io_receiver, seeker, page_set, warmup_rx, finish_rx)
}

pub(super) fn run_update<H: HashAlgorithm>(params: UpdateParams) -> std::io::Result<WorkerOutput> {
    let UpdateParams {
        page_cache,
        page_pool,
        store,
        root,
        warm_ups,
        warm_page_set,
        command,
        ..
    } = params;

    let seeker = Seeker::<H>::new(
        root,
        store.read_transaction(),
        page_cache.clone(),
        command.shared.overlay.clone(),
        store.io_pool().make_handle(),
        store.page_loader(),
        command.shared.witness,
    );

    update::<H>(
        root,
        page_cache,
        page_pool,
        seeker,
        command,
        warm_ups,
        warm_page_set,
    )
}

fn warm_up_phase<H: HashAlgorithm>(
    page_io_receiver: Receiver<crate::io::CompleteIo>,
    mut seeker: Seeker<H>,
    mut page_set: PageSet,
    warmup_rx: Receiver<WarmUpCommand>,
    finish_rx: Receiver<()>,
) -> std::io::Result<WarmUpOutput> {
    let mut select_all = Select::new();
    let warmup_idx = select_all.recv(&warmup_rx);
    let finish_idx = select_all.recv(&finish_rx);
    let page_idx = select_all.recv(&page_io_receiver);

    let mut select_no_work = Select::new();
    let finish_no_work_idx = select_no_work.recv(&finish_rx);
    let page_no_work_idx = select_no_work.recv(&page_io_receiver);

    let mut warm_ups = HashMap::new();

    loop {
        if let Some(result) = seeker.take_completion() {
            warm_ups.insert(result.key, result);
            continue;
        }

        seeker.submit_all(&mut page_set);
        if !seeker.has_room() {
            // block on interrupt or next page ready.
            let index = select_no_work.ready();
            if index == finish_no_work_idx {
                match finish_rx.try_recv() {
                    Err(TryRecvError::Empty) => continue,
                    Err(_) => panic!("Warm-Up worker, unexpected failure of the finish channel",),
                    Ok(()) => break,
                }
            } else if index == page_no_work_idx {
                seeker.try_recv_page(&mut page_set)?;
            } else {
                unreachable!()
            }
        } else {
            // has room. select on everything, pushing new work as available.
            let index = select_all.ready();
            if index == finish_idx {
                match finish_rx.try_recv() {
                    Err(TryRecvError::Empty) => continue,
                    Err(_) => panic!("Warm-Up worker, unexpected failure of the finish channel",),
                    Ok(()) => break,
                }
            } else if index == warmup_idx {
                let warm_up_command = match warmup_rx.try_recv() {
                    Ok(command) => command,
                    Err(TryRecvError::Empty) => continue,
                    Err(_) => panic!("Warm-Up worker, unexpected failure of the warmup channel"),
                };

                seeker.push(warm_up_command.key_path);
            } else if index == page_idx {
                seeker.try_recv_page(&mut page_set)?;
            } else {
                unreachable!()
            }
        }
    }

    while !seeker.is_empty() {
        if let Some(result) = seeker.take_completion() {
            warm_ups.insert(result.key, result);
            continue;
        }
        seeker.submit_all(&mut page_set);
        if seeker.has_live_requests() {
            seeker.recv_page(&mut page_set)?;
        }
    }

    Ok(WarmUpOutput {
        pages: page_set.freeze(),
        paths: warm_ups,
    })
}

fn update<H: HashAlgorithm>(
    root: Node,
    page_cache: PageCache,
    page_pool: PagePool,
    mut seeker: Seeker<H>,
    command: UpdateCommand,
    warm_ups: Arc<HashMap<KeyPath, Seek>>,
    warm_page_set: Option<FrozenSharedPageSet>,
) -> std::io::Result<WorkerOutput> {
    let UpdateCommand { shared, write_pass } = command;
    let write_pass = write_pass.into_inner();

    let mut output = WorkerOutput::new(shared.witness);

    let mut page_set = PageSet::new(page_pool, warm_page_set);

    let updater = RangeUpdater::<H>::new(root, shared.clone(), write_pass, &page_cache);

    // one lucky thread gets the master write pass.
    match updater.update(&mut seeker, &mut output, &mut page_set, warm_ups)? {
        None => return Ok(output),
        Some(write_pass) => write_pass,
    };

    let pending_ops = shared.take_root_pending();
    let mut root_page_updater = PageWalker::<H>::new(root, None);

    // Ensure the root page updater holds the root page. It is possible that this worker did not
    // seek any keys, and therefore the root page would not have been populated yet.
    if let Some((root_page, root_page_bucket)) =
        super::get_in_memory_page(&shared.overlay, &page_cache, &ROOT_PAGE_ID)
    {
        page_set.insert(
            ROOT_PAGE_ID,
            root_page,
            PageOrigin::Persisted(root_page_bucket),
        );
    }

    for (trie_pos, pending_op) in pending_ops {
        match pending_op {
            RootPagePending::Node(node) => {
                root_page_updater.advance_and_place_node(&page_set, trie_pos.clone(), node)
            }
            RootPagePending::SubTrie {
                range_start,
                range_end,
                prev_terminal,
            } => {
                let ops = subtrie_ops(&shared.read_write[range_start..range_end]);
                let ops = nomt_core::update::leaf_ops_spliced(prev_terminal, &ops);
                root_page_updater.advance_and_replace(&page_set, trie_pos.clone(), ops.clone());
            }
        }
    }

    // PANIC: output is always root when no parent page is specified.
    match root_page_updater.conclude() {
        Output::Root(new_root, updates) => {
            output.updated_pages.extend(updates);
            output.root = Some(new_root);
        }
        Output::ChildPageRoots(_, _) => unreachable!(),
    };

    Ok(output)
}

// helper for iterating all paths in the range and performing
// updates.
//
// anything that touches the root page is deferred via `shared.pending`.
struct RangeUpdater<H> {
    shared: Arc<UpdateShared>,
    write_pass: WritePass<ShardIndex>,
    region: PageRegion,
    page_walker: PageWalker<H>,
    range_start: usize,
    range_end: usize,
}

impl<H: HashAlgorithm> RangeUpdater<H> {
    fn new(
        root: Node,
        shared: Arc<UpdateShared>,
        write_pass: WritePass<ShardIndex>,
        page_cache: &PageCache,
    ) -> Self {
        let region = match write_pass.region() {
            ShardIndex::Root => PageRegion::universe(),
            ShardIndex::Shard(i) => page_cache.shard_region(*i),
        };
        let key_range_start = region.exclusive_min().min_key_path();
        let key_range_end = region.exclusive_max().max_key_path();

        let range_start = shared
            .read_write
            .binary_search_by_key(&key_range_start, |x| x.0)
            .unwrap_or_else(|i| i);

        let range_end = shared
            .read_write
            .binary_search_by_key(&key_range_end, |x| x.0)
            .unwrap_or_else(|i| i);

        RangeUpdater {
            shared,
            write_pass,
            region,
            page_walker: PageWalker::<H>::new(root, Some(ROOT_PAGE_ID)),
            range_start,
            range_end,
        }
    }

    // returns the end index of the batch. returns the end index of the batches covered by
    // this terminal.
    fn handle_completion(
        &mut self,
        output: &mut WorkerOutput,
        page_set: &PageSet,
        start_index: usize,
        seek_result: Seek,
    ) -> usize {
        // note that this is only true when the seek result is in the shared area - so multiple
        // workers will encounter it. we use this to defer to the first worker.
        let batch_starts_in_our_range = start_index != self.range_start
            || self.range_start == 0
            || !seek_result
                .position
                .subtrie_contains(&self.shared.read_write[start_index - 1].0);

        let (batch_size, has_writes) = {
            let mut batch_size = 0;
            let mut has_writes = false;

            // find batch size.
            for (k, v) in &self.shared.read_write[start_index..] {
                if !seek_result.position.subtrie_contains(&k) {
                    break;
                }
                batch_size += 1;
                has_writes |= v.is_write();
            }

            (batch_size, has_writes)
        };

        let next_index = start_index + batch_size;

        // witness / pushing pending responsibility falls on the worker whose range this falls
        // inside.
        if !batch_starts_in_our_range {
            return next_index;
        }

        let is_non_exclusive = seek_result
            .page_id
            .as_ref()
            .map_or(true, |p_id| !self.region.contains_exclusive(p_id));

        if is_non_exclusive {
            self.shared.push_pending_subtrie(
                seek_result.position.clone(),
                start_index,
                next_index,
                seek_result.terminal.clone(),
            );

            if let Some(ref mut witnessed_paths) = output.witnessed_paths {
                let path = WitnessedPath {
                    inner: PathProof {
                        // if the terminal lands in the non-exclusive area, then the path to it is
                        // guaranteed not to have been altered by anything we've done so far.
                        siblings: seek_result.siblings,
                        terminal: match seek_result.terminal.clone() {
                            Some(leaf_data) => PathProofTerminal::Leaf(leaf_data),
                            None => PathProofTerminal::Terminator(seek_result.position.clone()),
                        },
                    },
                    path: seek_result.position,
                };
                witnessed_paths.push((path, seek_result.terminal, batch_size));
            }

            return next_index;
        }

        // attempt to advance the trie walker. if it fails, pocket away for later.
        let ops = if has_writes {
            Some(subtrie_ops(
                &self.shared.read_write[start_index..next_index],
            ))
        } else {
            None
        };
        self.attempt_advance(output, page_set, seek_result, ops, batch_size);

        next_index
    }

    // attempt to advance the trie walker. if this fails, it submits a special page request to
    // the seeker and stores the `SavedAdvance`. if this succeeds, it updates the output.
    fn attempt_advance(
        &mut self,
        output: &mut WorkerOutput,
        page_set: &PageSet,
        seek_result: Seek,
        ops: Option<Vec<(KeyPath, Option<ValueHash>)>>,
        batch_size: usize,
    ) {
        match ops {
            None => self.page_walker.advance(seek_result.position.clone()),
            Some(ref ops) => {
                let ops = nomt_core::update::leaf_ops_spliced(seek_result.terminal.clone(), &ops);
                self.page_walker
                    .advance_and_replace(page_set, seek_result.position.clone(), ops)
            }
        };

        if let Some(ref mut witnessed_paths) = output.witnessed_paths {
            let siblings = {
                // nodes may have been altered prior to seeking - the page walker tracks which ones.
                let mut siblings = seek_result.siblings;
                for (actual_sibling, depth) in self.page_walker.siblings() {
                    siblings[*depth - 1] = *actual_sibling;
                }
                siblings
            };

            let path = WitnessedPath {
                inner: PathProof {
                    siblings,
                    terminal: match seek_result.terminal.clone() {
                        Some(leaf_data) => PathProofTerminal::Leaf(leaf_data),
                        None => PathProofTerminal::Terminator(seek_result.position.clone()),
                    },
                },
                path: seek_result.position,
            };
            witnessed_paths.push((path, seek_result.terminal, batch_size));
        }
    }

    fn update(
        mut self,
        seeker: &mut Seeker<H>,
        output: &mut WorkerOutput,
        page_set: &mut PageSet,
        warm_ups: Arc<HashMap<KeyPath, Seek>>,
    ) -> std::io::Result<Option<WritePass<ShardIndex>>> {
        let mut start_index = self.range_start;
        let mut pushes = 0;
        let mut skips = 0;

        let mut warmed_up: VecDeque<Seek> = VecDeque::new();

        // 1. drive until work is done.
        while start_index < self.range_end || !seeker.is_empty() {
            let completion = if warmed_up
                .front()
                .map_or(false, |res| match seeker.first_key() {
                    None => true,
                    Some(k) => &res.key < k,
                }) {
                // take a "completion" from our warm-ups instead.
                // UNWRAP: checked front exists above.
                Some(warmed_up.pop_front().unwrap())
            } else {
                seeker.take_completion()
            };

            // handle a single completion (only when blocked / at max capacity)
            match completion {
                None => {}
                Some(seek_result) => {
                    // skip completions until we're past the end of the last batch.
                    if skips > 0 {
                        skips -= 1;
                    } else {
                        let end_index =
                            self.handle_completion(output, page_set, start_index, seek_result);

                        // account for stuff we pushed that was already covered by the terminal
                        // we just popped off.
                        let batch_size = end_index - start_index;
                        // note: pushes and batch size are both at least 1.
                        skips = std::cmp::min(pushes, batch_size) - 1;
                        pushes = pushes.saturating_sub(batch_size);

                        start_index = end_index;
                    }
                }
            }

            seeker.submit_all(page_set);
            if !seeker.has_room() && seeker.has_live_requests() {
                // no way to push work until at least one page fetch has concluded.
                seeker.recv_page(page_set)?;
                continue;
            }

            // push work until out of work.
            while seeker.has_room() && start_index + pushes < self.range_end {
                let next_push = start_index + pushes;
                pushes += 1;

                if let Some(result) = warm_ups.get(&self.shared.read_write[next_push].0) {
                    warmed_up.push_back(result.clone());
                    if warmed_up.len() >= 512 {
                        break;
                    }
                } else {
                    seeker.push(self.shared.read_write[next_push].0);
                    seeker.submit_all(page_set);
                }
            }

            seeker.try_recv_page(page_set)?;
        }

        // 2. conclude.
        // PANIC: walker was configured with a parent page.
        let (new_nodes, updates) = match self.page_walker.conclude() {
            Output::ChildPageRoots(new_nodes, output_pages) => (new_nodes, output_pages),
            Output::Root(_, _) => unreachable!(),
        };

        debug_assert!(!updates.iter().any(|item| item.page_id == ROOT_PAGE_ID));
        output.updated_pages = updates;

        self.shared.push_pending_root_nodes(new_nodes);

        return Ok(self.write_pass.consume());
    }
}

fn subtrie_ops(read_write: &[(KeyPath, KeyReadWrite)]) -> Vec<(KeyPath, Option<ValueHash>)> {
    read_write
        .iter()
        .filter_map(|(key, read_write)| match read_write {
            KeyReadWrite::Write(val) | KeyReadWrite::ReadThenWrite(val) => {
                Some((key.clone(), val.clone()))
            }
            KeyReadWrite::Read => None,
        })
        .collect::<Vec<_>>()
}
