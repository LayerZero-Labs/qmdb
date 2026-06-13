use crate::beatree::Key;
use core::ops::Range;
use crossbeam_channel::{Receiver, Sender, TryRecvError};

use super::{ChangedNodeEntry, NodesTracker};

/// A half-open range [low, high), where each key corresponds to a known separator of a node.
pub struct SeparatorRange {
    pub low: Option<Key>,
    pub high: Option<Key>,
}

/// Parameters sent to each worker which enables them to work on a portion
/// of the entire changeset that needs to be applied to the beatree
pub struct WorkerParams<Node> {
    pub left_neighbor: Option<LeftNeighbor<Node>>,
    pub right_neighbor: Option<RightNeighbor<Node>>,
    pub range: SeparatorRange,
    pub op_range: Range<usize>,
}

/// LeftNeighbor of a given node, ExtendRangeRequest can only be received from this side.
pub struct LeftNeighbor<Node> {
    pub rx: Receiver<ExtendRangeRequest<Node>>,
}

/// RightNeighbor of a given node, ExtendRangeRequest can only be sent to the neighbor.
pub struct RightNeighbor<Node> {
    pub tx: Sender<ExtendRangeRequest<Node>>,
}

/// A request to extend the range to the next node following the high bound of the range.
pub struct ExtendRangeRequest<Node> {
    tx: Sender<ExtendRangeResponse<Node>>,
}

// The response to an ExtendRangeRequest, it contains all the information
// needed to continue the update or a new right neighbor,
// to which the request needs to be resent.
struct ExtendRangeResponse<Node> {
    changed: Vec<(Key, ChangedNodeEntry<Node>)>,
    new_high_range: Option<Key>,
    new_right_neighbor: Option<Option<RightNeighbor<Node>>>,
}

/// Attempt to reply to a request by the left worker,
/// the request could be put in a pending state until
/// some useful information is ready to be sent
pub fn try_answer_left_neighbor<Node>(
    pending_request: &mut Option<ExtendRangeRequest<Node>>,
    worker_params: &mut WorkerParams<Node>,
    nodes_tracker: &mut NodesTracker<Node>,
    has_finished_workload: bool,
) {
    let Some(ref left_neighbor) = worker_params.left_neighbor else {
        return;
    };

    let request = match pending_request.take() {
        Some(r) => r,
        None => match left_neighbor.rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Disconnected) => {
                worker_params.left_neighbor = None;
                return;
            }
            _ => return,
        },
    };

    // We send the left side information it needs to deduce the nodes to use next.
    // A modified node could be used as a node by the left worker, or we provide a new
    // high range to let the left worker use new fresh nodes
    let mut found_next_node = false;
    // an unchanged range is given by a difference between the next_separator
    // of a changed item the key associated to the next item
    let mut found_unchanged_range = false;
    let mut new_high_range = None;
    let mut separator = worker_params.range.low;

    let mut take = nodes_tracker
        .inner
        .iter()
        .take_while(|(key, entry)| {
            if let Some(low) = separator {
                if low < **key {
                    found_unchanged_range = true;
                    new_high_range = Some(**key);
                    return false;
                }
            }

            if entry.inserted.is_some() {
                found_next_node = true;
                new_high_range = entry.next_separator.clone();
                return false;
            }

            separator = entry.next_separator.clone();

            true
        })
        .count();

    if !(found_next_node || found_unchanged_range) {
        if has_finished_workload {
            // separator is None only if nodes_tracker.inner.is_empty
            // or the last deleted item has next separator = None.
            // If there are some elements in nodes_tracker.inner
            // they will be for sure all changes with a deleted node
            // and they will be passed to the left worker

            if let Some(low) = separator {
                if worker_params.range.high.map_or(true, |high| low < high) {
                    // special case where there is one unchanged range left,
                    // which is the last one, up to the worker.range.high
                    found_unchanged_range = true;
                }
            }

            new_high_range = worker_params.range.high;
        } else {
            // Keep the request pending only if no inserted node has been found or any unchanged range,
            // and if the worker has not finished with the right worker
            *pending_request = Some(request);
            return;
        }
    }

    // `self.low` and `left.high` are kept equal
    worker_params.range.low = new_high_range;

    if found_next_node {
        take += 1;
    }

    let changed = (0..take)
        .map(|_| {
            // UNWRAP: the existance and variant of `take` nodes_tracker items was previously checked
            nodes_tracker.inner.pop_first().unwrap()
        })
        .collect::<Vec<_>>();

    let new_right_neighbor = if !(found_next_node || found_unchanged_range) && has_finished_workload
    {
        // left neighbor consumed our entire range. link them up with our right neighbor.
        worker_params.left_neighbor = None;
        Some(worker_params.right_neighbor.take())
    } else {
        None
    };

    // UNWRAP: neighbor waiting on request will never drop.
    request
        .tx
        .send(ExtendRangeResponse {
            changed,
            new_high_range,
            new_right_neighbor,
        })
        .unwrap();
}

/// Send an ExtendRangeRequest to the right worker and resend it
/// recursively if a new right neighbor is provided
pub fn request_range_extension<Node>(
    worker_params: &mut WorkerParams<Node>,
    nodes_tracker: &mut NodesTracker<Node>,
) {
    // UNWRAP: we should only be requesting a range extension when we have a right neighbor.
    // workers with no right neighbor have no limit to their range.
    let right_neighbor = worker_params.right_neighbor.as_ref().unwrap();

    let (tx, rx) = crossbeam_channel::unbounded();
    let request = ExtendRangeRequest { tx };

    // UNWRAP: right neighbor never drops until left neighbor is done.
    right_neighbor.tx.send(request).unwrap();

    // UNWRAP: right neighbor never drops until left neighbor is done.
    let mut response = rx.recv().unwrap();

    worker_params.range.high = response.new_high_range;

    if let Some((last_key, last_changed_entry)) = response.changed.last_mut() {
        if last_changed_entry.inserted.is_some() {
            // UNWRAP: the entry has just been checked to have an inserted node
            let (node, pn) = last_changed_entry.inserted.take().unwrap();

            nodes_tracker.set_pending_base(*last_key, node, last_changed_entry.next_separator, pn);
        }
    }

    nodes_tracker.inner.extend(response.changed);

    if let Some(new_right_neighbor) = response.new_right_neighbor {
        worker_params.right_neighbor = new_right_neighbor;
        if worker_params.right_neighbor.is_some() {
            // exhausting the range means we didn't get a node to merge with.
            // try again with the next right neighbor.
            request_range_extension(worker_params, nodes_tracker);
        }
    }
}
