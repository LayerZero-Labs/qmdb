use std::ops::Range;
use std::sync::Arc;
use threadpool::ThreadPool;

use crate::io::{IoCommand, IoHandle, IoKind, PagePool};
use crate::{
    beatree::{
        allocator::{PageNumber, SyncAllocator},
        branch::node::BranchNode,
        index::Index,
        Key,
    },
    task::{join_task, spawn_task},
};

use super::branch_updater::{BaseBranch, BranchUpdater, DigestResult as BranchDigestResult};
use super::extend_range_protocol::{
    request_range_extension, try_answer_left_neighbor, LeftNeighbor, RightNeighbor, SeparatorRange,
    WorkerParams,
};

/// Tracker of all changes that happen to branch nodes during an update
pub type BranchesTracker = super::NodesTracker<BranchNode>;

/// Data, including buffers, which may be dropped only once I/O has certainly concluded.
#[derive(Default)]
pub struct PostIoDrop {
    deferred_drop_pages: Vec<Vec<Arc<BranchNode>>>,
}

/// Outputs of the branch stage.
#[derive(Default)]
pub struct BranchStageOutput {
    /// The page numbers of all freed pages.
    pub freed_pages: Vec<PageNumber>,
    /// The number of submitted I/Os.
    pub submitted_io: usize,
    /// Data which should be dropped after all submitted I/Os have concluded.
    pub post_io_drop: PostIoDrop,
}

/// Change the btree's branch nodes in the specified way.
pub fn run(
    bbn_index: &mut Index,
    bbn_writer: SyncAllocator,
    page_pool: PagePool,
    io_handle: IoHandle,
    changeset: Vec<(Key, Option<PageNumber>)>,
    thread_pool: ThreadPool,
    num_workers: usize,
) -> std::io::Result<BranchStageOutput> {
    if changeset.is_empty() {
        return Ok(BranchStageOutput::default());
    }

    assert!(num_workers >= 1);
    let workers = prepare_workers(&*bbn_index, &changeset, num_workers);
    assert!(!workers.is_empty());

    let changeset = Arc::new(changeset);

    let num_workers = workers.len();
    let (worker_result_tx, worker_result_rx) = crossbeam_channel::bounded(num_workers);

    for worker_params in workers.into_iter() {
        let bbn_index = bbn_index.clone();
        let bbn_writer = bbn_writer.clone();
        let page_pool = page_pool.clone();
        let io_handle = io_handle.clone();
        let changeset = changeset.clone();

        let worker_result_tx = worker_result_tx.clone();

        let branch_stage_worker_task = move || {
            // passing the large `Arc` values by reference ensures that they are dropped at the
            // end of this scope, not the end of `run_worker`.
            run_worker(
                bbn_index,
                bbn_writer,
                page_pool,
                io_handle,
                &*changeset,
                worker_params,
            )
        };
        spawn_task(
            &thread_pool,
            branch_stage_worker_task,
            worker_result_tx.clone(),
        );
    }

    // we don't want to block other sync steps on deallocating these memory regions.
    drop(changeset);

    let mut output = BranchStageOutput::default();
    let mut branch_changeset: Vec<(Key, Option<Arc<BranchNode>>)> = vec![];

    for _ in 0..num_workers {
        let worker_output = join_task(&worker_result_rx)?;
        apply_bbn_changes(&mut output, &mut branch_changeset, worker_output);
    }

    filter_branch_changeset(&mut branch_changeset);
    apply_changes_to_index(bbn_index, branch_changeset);

    Ok(output)
}

fn apply_bbn_changes(
    output: &mut BranchStageOutput,
    branch_changeset: &mut Vec<(Key, Option<Arc<BranchNode>>)>,
    mut worker_output: BranchWorkerOutput,
) {
    for (key, changed_branch) in worker_output.branches_tracker.inner {
        // Discard entries that have both insert and delete equal to None.
        // This could happen when a worker receives a created page from the right
        // worker, but it has been merged into a previous one.
        if changed_branch.inserted.is_none() && changed_branch.deleted.is_none() {
            continue;
        }

        if changed_branch.inserted.is_some() {
            output.submitted_io += 1;
        }

        if let Some(deleted_pn) = changed_branch.deleted {
            output.freed_pages.push(deleted_pn);
        }

        branch_changeset.push((key.clone(), changed_branch.inserted.map(|(node, _)| node)));
    }

    output.submitted_io += worker_output.branches_tracker.extra_freed.len();
    output
        .freed_pages
        .extend(worker_output.branches_tracker.extra_freed.drain(..));
    output.post_io_drop.deferred_drop_pages.push(std::mem::take(
        &mut worker_output.branches_tracker.deferred_drop_pages,
    ));
}

// Branch changeset is created by aggregating the outcomes of multiple workers,
// who share the pages they have worked on with the extension range protocol.
// This function filters some work to avoid the incorrect elimination of pages.
fn filter_branch_changeset(branch_changeset: &mut Vec<(Key, Option<Arc<BranchNode>>)>) {
    branch_changeset.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));
    let mut to_remove = vec![];
    // If there are two changes with the same separator produced
    // by two workers, one is expected to be deleted and the other
    // inserted.
    //
    // The deleted page was already inserted in the freed_pages, thus,
    // now only the inserted page must be kept, resulting in a standard
    // modification of the same page, but made by two workers.
    for i in 0..branch_changeset.len() - 1 {
        if branch_changeset[i].0 == branch_changeset[i + 1].0 {
            // PANICS: If two changesets refer to the same page, they
            // are expected to be treated as a single one, with one
            // change deleting it and the other inserting it.
            if branch_changeset[i].1.is_some() {
                assert!(branch_changeset[i + 1].1.is_none());
                to_remove.push(i + 1);
            }

            if branch_changeset[i].1.is_none() {
                assert!(branch_changeset[i + 1].1.is_some());
                to_remove.push(i);
            }
        }
    }

    for idx in to_remove.into_iter().rev() {
        branch_changeset.remove(idx);
    }
}

fn apply_changes_to_index(
    bbn_index: &mut Index,
    branch_changeset: Vec<(Key, Option<Arc<BranchNode>>)>,
) {
    for (key, maybe_inserted) in branch_changeset {
        match maybe_inserted {
            Some(node) => {
                bbn_index.insert(key, node);
            }
            None => {
                bbn_index.remove(&key);
            }
        }
    }
}

fn prepare_workers(
    bbn_index: &Index,
    changeset: &[(Key, Option<PageNumber>)],
    worker_count: usize,
) -> Vec<WorkerParams<BranchNode>> {
    let mut remaining_workers = worker_count;
    let mut changeset_remaining = changeset;

    let mut workers = Vec::with_capacity(worker_count);

    // first worker covers everything. it'll be adjusted in the next iteration of the loop,
    // if possible.
    workers.push(WorkerParams {
        left_neighbor: None,
        right_neighbor: None,
        range: SeparatorRange {
            low: None,
            high: None,
        },
        op_range: Range {
            start: 0,
            end: changeset.len(),
        },
    });
    remaining_workers -= 1;

    // iter: find endpoint for previous worker and push next worker.
    // look up the branch for the (ops/N)th operation, find out how many ops it covers,
    // and set the end range accordingly.
    while remaining_workers > 0 && changeset_remaining.len() > 0 {
        let pivot_idx = changeset_remaining.len() / (remaining_workers + 1);

        // If pivot_idx == 0 the number of changeset_remaining
        // is less than the number of remaining workers.
        // Everything remaining will be covered by the previous worker.
        if pivot_idx == 0 {
            break;
        }

        // UNWRAP: first worker is pushed at the beginning of the range.
        let prev_worker = workers.last_mut().unwrap();

        let Some((_, branch)) = bbn_index.lookup(changeset_remaining[pivot_idx].0) else {
            // This could happen only on empty db, let one worker handle everything
            break;
        };

        let separator = super::get_key(&branch, 0);

        // link this worker with the previous one.
        let (tx, rx) = crossbeam_channel::unbounded();

        // have previous worker cover everything, only up to the separator of this node.
        let prev_worker_ops = pivot_idx
            - changeset_remaining[..pivot_idx]
                .iter()
                .rev()
                .take_while(|(k, _)| k >= &separator)
                .count();

        if prev_worker_ops == 0 {
            changeset_remaining = &changeset_remaining[pivot_idx..];
            continue;
        }

        let op_partition_index = (changeset.len() - changeset_remaining.len()) + prev_worker_ops;

        // previous worker now owns all nodes up to this one.
        prev_worker.range.high = Some(separator);
        prev_worker.right_neighbor = Some(RightNeighbor { tx });
        prev_worker.op_range.end = op_partition_index;

        workers.push(WorkerParams {
            left_neighbor: Some(LeftNeighbor { rx }),
            right_neighbor: None,
            range: SeparatorRange {
                low: Some(separator),
                high: None,
            },
            op_range: Range {
                start: op_partition_index,
                end: changeset.len(),
            },
        });
        remaining_workers -= 1;
        changeset_remaining = &changeset_remaining[prev_worker_ops..];
    }

    workers
}

fn reset_branch_base(
    bbn_index: &Index,
    branches_tracker: &mut BranchesTracker,
    branch_updater: &mut BranchUpdater,
    has_extended_range: bool,
    mut key: Key,
) {
    if !has_extended_range {
        reset_branch_base_fresh(bbn_index, branches_tracker, branch_updater, key);
        return;
    }

    if let Some((_, node, next_separator)) = branches_tracker.pending_base.take() {
        let base = BaseBranch::new(node);
        branch_updater.reset_base(Some(base), next_separator);
    } else {
        if let Some(separator) = branches_tracker
            .inner
            .last_key_value()
            .and_then(|(_, entry)| entry.next_separator)
        {
            if separator > key {
                key = separator;
            }
            reset_branch_base_fresh(bbn_index, branches_tracker, branch_updater, key);
        } else {
            // special case: all rightward workers deleted every last one of their nodes after the last one
            // we received from a range extension. We are now writing the new rightmost node, which
            // is permitted to be underfull.
            branch_updater.remove_cutoff();
        }
    }
}

fn reset_branch_base_fresh(
    bbn_index: &Index,
    branches_tracker: &mut BranchesTracker,
    branch_updater: &mut BranchUpdater,
    key: Key,
) {
    let Some((separator, branch)) = bbn_index.lookup(key) else {
        return;
    };

    let cutoff = bbn_index.next_key(key);

    branches_tracker.delete(separator, branch.bbn_pn().into(), cutoff);

    let base = BaseBranch::new(branch);
    branch_updater.reset_base(Some(base), cutoff);
}

struct BranchWorkerOutput {
    branches_tracker: BranchesTracker,
}

fn run_worker(
    bbn_index: Index,
    bbn_writer: SyncAllocator,
    page_pool: PagePool,
    io_handle: IoHandle,
    changeset: &[(Key, Option<PageNumber>)],
    mut worker_params: WorkerParams<BranchNode>,
) -> std::io::Result<BranchWorkerOutput> {
    let mut branch_updater = BranchUpdater::new(page_pool, None, None);
    let mut pending_left_request = None;
    let mut has_extended_range = false;
    let mut has_finished_workload = false;

    let mut new_branch_state = NewBranchHandler {
        bbn_writer,
        branches_tracker: BranchesTracker::new(),
        io_handle,
    };

    // point branch updater at first branch.
    reset_branch_base(
        &bbn_index,
        &mut new_branch_state.branches_tracker,
        &mut branch_updater,
        has_extended_range,
        changeset[worker_params.op_range.start].0,
    );

    for (key, op) in &changeset[worker_params.op_range.clone()] {
        // ensure key is in scope for branch updater. if not, digest it. merge rightwards until
        // done _or_ key is in scope.
        while !branch_updater.is_in_scope(&key) {
            // After `digest`, some changed items could have been produced and they could be used
            // to respond to the left neighbor. However, we should only respond if we are sure that
            // we will not introduce changed items with smaller separators later on.
            let k = if let BranchDigestResult::NeedsMerge(cutoff) =
                branch_updater.digest(&mut new_branch_state)?
            {
                // If we are dealing with a NeedsMerge, there is a high probability that the `branch_updater`
                // has a new pending branch which still needs to be constructed with a separator smaller
                // than the last entry in the `leaves_tracker`.
                cutoff
            } else {
                // If the `branch_updater` has finished the last digest, we are safe to try to respond.
                try_answer_left_neighbor(
                    &mut pending_left_request,
                    &mut worker_params,
                    &mut new_branch_state.branches_tracker,
                    has_finished_workload,
                );

                *key
            };

            has_extended_range = false;
            if worker_params.range.high.map_or(false, |high| k >= high) {
                has_extended_range = true;
                request_range_extension(&mut worker_params, &mut new_branch_state.branches_tracker);
            }

            reset_branch_base(
                &bbn_index,
                &mut new_branch_state.branches_tracker,
                &mut branch_updater,
                has_extended_range,
                k,
            );
        }

        branch_updater.ingest(*key, *op);
    }

    while let BranchDigestResult::NeedsMerge(cutoff) =
        branch_updater.digest(&mut new_branch_state)?
    {
        has_extended_range = false;
        if worker_params
            .range
            .high
            .map_or(false, |high| cutoff >= high)
        {
            has_extended_range = true;
            request_range_extension(&mut worker_params, &mut new_branch_state.branches_tracker);
        }

        reset_branch_base(
            &bbn_index,
            &mut new_branch_state.branches_tracker,
            &mut branch_updater,
            has_extended_range,
            cutoff,
        );
    }

    // Now we are safe to send over our right neighbor to the left one
    // because we're sure to have finished interacting with it
    has_finished_workload = true;

    // wait until left worker concludes or exhausts our range.
    while worker_params.left_neighbor.is_some() {
        // answer any outstanding pending request
        try_answer_left_neighbor(
            &mut pending_left_request,
            &mut worker_params,
            &mut new_branch_state.branches_tracker,
            has_finished_workload,
        );
        // we're done with the right worker so we're sure
        // we are able to respond to each request
        assert!(pending_left_request.is_none());

        match worker_params.left_neighbor.as_ref().map(|l| l.rx.recv()) {
            None => continue,
            Some(Ok(item)) => {
                pending_left_request = Some(item);
            }
            Some(Err(_)) => {
                worker_params.left_neighbor = None;
            }
        }
    }

    Ok(BranchWorkerOutput {
        branches_tracker: new_branch_state.branches_tracker,
    })
}

struct NewBranchHandler {
    bbn_writer: SyncAllocator,
    branches_tracker: BranchesTracker,
    io_handle: IoHandle,
}

impl super::branch_updater::HandleNewBranch for NewBranchHandler {
    fn handle_new_branch(
        &mut self,
        key: Key,
        mut bbn: BranchNode,
        cutoff: Option<Key>,
    ) -> std::io::Result<()> {
        let fd = self.bbn_writer.store_fd();

        let page_number = self.bbn_writer.allocate()?;

        bbn.set_bbn_pn(page_number.0);
        let bbn = Arc::new(bbn);

        let page = bbn.page();
        self.io_handle
            .send(IoCommand {
                kind: IoKind::WriteRaw(fd, page_number.0 as u64, page),
                user_data: 0,
            })
            .expect("I/O Pool Down");

        self.branches_tracker.insert(key, bbn, cutoff, page_number);
        Ok(())
    }
}
