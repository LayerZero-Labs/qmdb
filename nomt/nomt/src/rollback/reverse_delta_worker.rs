use std::{collections::HashMap, sync::Arc};

use crossbeam::channel::{Receiver, RecvError, Sender};
use dashmap::DashMap;
use nomt_core::trie::KeyPath;
use threadpool::ThreadPool;

use crate::{
    beatree::{self, AsyncLookup, OverflowPageInfo, ReadTransaction},
    io::{CompleteIo, FatPage, IoHandle},
    overlay::LiveOverlay,
    task::{spawn_task, TaskResult},
};

/// A trait for asynchronously loading values from the store.
///
/// This family of traits allows us to mock the store in tests.
///
/// Dropping a value implementing this trait should end its corresponding completion stream.
pub(super) trait LoadValueAsync: Send + Sync + Sized + 'static {
    type Pending: AsyncPending<Store = Self>;
    type Completion: Send;
    type RawCompletion;

    /// Start a load. This may complete eagerly or return a pending result.
    fn start_load(
        &self,
        key_path: KeyPath,
        user_data: u64,
    ) -> Result<Option<Vec<u8>>, Self::Pending>;

    /// Get a receiver of completions.
    fn raw_completion_stream(&self) -> Receiver<Self::RawCompletion>;

    /// Create the completion function.
    fn process_raw_completion(
        complete_io: Self::RawCompletion,
    ) -> (u64, anyhow::Result<Self::Completion>);
}

/// A trait for a pending asynchronous value load.
///
/// This family of traits allows us to mock the store in tests.
pub(super) trait AsyncPending {
    type Store: LoadValueAsync;
    type OverflowPageInfo;

    fn submit(&mut self, store: &Self::Store, user_data: u64) -> Option<Self::OverflowPageInfo>;
    fn try_complete(
        &mut self,
        completion: <Self::Store as LoadValueAsync>::Completion,
        meta: Option<Self::OverflowPageInfo>,
    ) -> Option<Option<Vec<u8>>>;
}

pub(super) enum DeltaBuilderCommand {
    /// Start lookup on a key path.
    Lookup(KeyPath),
    /// Join all outstanding lookups, then send a signal over the channel.
    /// Replace the priors with the given map.
    Join(Sender<()>, Arc<DashMap<KeyPath, Option<Vec<u8>>>>),
}

/// Start the reverse delta builder. The thread pool must have at least 2 threads or else the worker
/// will never conclude.
pub(super) fn start(
    store: impl LoadValueAsync,
    tp: &ThreadPool,
    priors: Arc<DashMap<KeyPath, Option<Vec<u8>>>>,
) -> (Sender<DeltaBuilderCommand>, Receiver<TaskResult<()>>) {
    let (command_tx, command_rx) = crossbeam::channel::unbounded();
    let (worker_result_tx, worker_result_rx) = crossbeam::channel::unbounded();

    let worker_task = {
        let priors = priors.clone();
        move || {
            let worker = ReverseDeltaWorker {
                store: Some(store),
                requests: HashMap::new(),
                priors,
                shutdown: false,
                request_index: 0,
                overflow_request_index: u64::MAX,
                dormant_request_count: 0,
            };

            run(worker, command_rx);
        }
    };

    spawn_task(&tp, worker_task, worker_result_tx);

    (command_tx, worker_result_rx)
}

const TARGET_OVERFLOW_REQUESTS: usize = 128;

enum LiveRequest<Req: AsyncPending> {
    Main(Req, KeyPath),
    OverflowPage(u64, Req::OverflowPageInfo),
}

struct ReverseDeltaWorker<Store: LoadValueAsync> {
    // INVARIANT: store is always `Some` until `joining` is true and requests are empty.
    store: Option<Store>,
    requests: HashMap<u64, LiveRequest<Store::Pending>>,
    priors: Arc<DashMap<KeyPath, Option<Vec<u8>>>>,
    shutdown: bool,
    // regular requests start at 0 and move upwards.
    request_index: u64,
    // overflow requests start at max and move downwards.
    overflow_request_index: u64,
    // the number of requests which are "dormant", i.e. they are used only for dispatching
    // overflow requests.
    dormant_request_count: usize,
}

impl<Store: LoadValueAsync> ReverseDeltaWorker<Store> {
    fn handle_lookup(&mut self, key_path: KeyPath) {
        // UNWRAP: `Load` commands never come after finish.
        let store = self.store.as_ref().unwrap();
        match store.start_load(key_path, self.request_index) {
            Ok(val) => {
                self.priors.insert(key_path, val);
            }
            Err(pending) => {
                self.requests
                    .insert(self.request_index, LiveRequest::Main(pending, key_path));
                self.request_index += 1;
            }
        }
    }

    fn handle_completion(
        &mut self,
        user_data: u64,
        maybe_completion: anyhow::Result<Store::Completion>,
    ) {
        // TODO handle error properly
        let completion: Store::Completion = maybe_completion.unwrap();

        // UNWRAP: completions come for occupied entries only.
        let request = self.requests.remove(&user_data).unwrap();
        let resubmit = match request {
            LiveRequest::Main(mut request, key_path) => {
                match request.try_complete(completion, None) {
                    Some(value) => {
                        self.priors.insert(key_path, value);
                        None
                    }
                    None => {
                        // this is now an overflow request. count it as dormant and insert
                        // back.
                        self.dormant_request_count += 1;
                        self.requests
                            .insert(user_data, LiveRequest::Main(request, key_path));
                        Some(user_data)
                    }
                }
            }
            LiveRequest::OverflowPage(request_id, overflow_meta) => {
                // UNWRAP: initial request always exists
                let overflow_request = self.requests.get_mut(&request_id).unwrap();
                // PANIC: ...and has kind initial.
                let LiveRequest::Main(ref mut pending, ref key_path) = overflow_request else {
                    unreachable!()
                };

                // If we get a value, insert it in priors and stop tracking the request.
                if let Some(value) = pending.try_complete(completion, Some(overflow_meta)) {
                    self.dormant_request_count -= 1;
                    self.priors.insert(*key_path, value);
                    self.requests.remove(&request_id);
                    None
                } else {
                    Some(request_id)
                }
            }
        };

        if self.shutdown && self.requests.is_empty() {
            self.store = None;
        } else if let Some(resubmit_id) = resubmit {
            self.resubmit_overflow(resubmit_id);
        }
    }

    fn resubmit_overflow(&mut self, resubmit_id: u64) {
        let submitted = {
            let non_dormant_count = self.requests.len() - self.dormant_request_count;

            // UNWRAP: resubmit request always exists.
            let overflow_request = self.requests.get_mut(&resubmit_id).unwrap();
            // PANIC: ...and has kind initial.
            let LiveRequest::Main(ref mut pending, _) = overflow_request else {
                unreachable!()
            };
            // UNWRAP: ...and the `Store` must be live.
            let store = self.store.as_ref().unwrap();

            let mut submitted = Vec::new();

            // Always submit at least one, but never more than the maximum.
            let submit_count = std::cmp::max(
                1,
                TARGET_OVERFLOW_REQUESTS.saturating_sub(non_dormant_count),
            );
            for _ in 0..submit_count {
                let Some(meta) = pending.submit(store, self.overflow_request_index) else {
                    break;
                };
                submitted.push((
                    self.overflow_request_index,
                    LiveRequest::OverflowPage(resubmit_id, meta),
                ));
                self.overflow_request_index -= 1;
            }

            submitted
        };

        self.requests.extend(submitted);
    }

    fn start_shutdown(&mut self) {
        self.shutdown = true;
        self.try_finish_shutdown();
    }

    fn is_shut_down(&self) -> bool {
        self.shutdown && self.requests.is_empty()
    }

    fn try_finish_shutdown(&mut self) {
        if self.is_shut_down() {
            self.store = None;
        }
    }

    fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }

    fn replace_priors(&mut self, new_priors: Arc<DashMap<KeyPath, Option<Vec<u8>>>>) {
        self.priors = new_priors;
    }
}

fn run<Store: LoadValueAsync>(
    mut worker: ReverseDeltaWorker<Store>,
    command_rx: Receiver<DeltaBuilderCommand>,
) {
    // UNWRAP: the store is always live until shutdown.
    let raw_completion_rx = worker.store.as_ref().unwrap().raw_completion_stream();

    loop {
        let join = crossbeam::select! {
            recv(command_rx) -> command => match command {
                Err(RecvError) => {
                    worker.start_shutdown();
                    break
                }
                Ok(DeltaBuilderCommand::Lookup(key_path)) => {
                    worker.handle_lookup(key_path);
                    None
                },
                Ok(DeltaBuilderCommand::Join(done_joining, new_priors)) => Some((done_joining, new_priors)),
            },
            recv(raw_completion_rx) -> raw_completion => match raw_completion {
                // PANIC: completions never disconnect before commands.
                Err(RecvError) => unreachable!(),
                Ok(raw_completion) => {
                    let (user_data, completion) = Store::process_raw_completion(raw_completion);
                    worker.handle_completion(user_data, completion);
                    None
                }
            },
        };

        if let Some((done_joining, new_priors)) = join {
            while !worker.is_empty() {
                // UNWRAP: completions never disconnect before `commands` does.
                let raw_completion = raw_completion_rx.recv().unwrap();
                let (user_data, completion) = Store::process_raw_completion(raw_completion);

                worker.handle_completion(user_data, completion);
            }

            worker.replace_priors(new_priors);
            let _ = done_joining.send(());
        }
    }

    // wait on outstanding completions.
    for raw_completion in raw_completion_rx {
        let (user_data, completion) = Store::process_raw_completion(raw_completion);
        worker.handle_completion(user_data, completion);
    }

    assert!(worker.is_shut_down());
}

/// The implementation of [`LoadValueAsync`] for the real store.
pub(super) struct StoreLoadValueAsync {
    read_tx: ReadTransaction,
    io_handle: IoHandle,
    overlay: LiveOverlay,
}

impl StoreLoadValueAsync {
    /// Create a new asynchronous value loader from the store.
    pub fn new(store: &crate::store::Store, overlay: LiveOverlay) -> Self {
        let read_tx = store.read_transaction();
        let io_handle = store.io_pool().make_handle();

        StoreLoadValueAsync {
            read_tx,
            io_handle,
            overlay,
        }
    }
}

impl AsyncPending for AsyncLookup {
    type Store = StoreLoadValueAsync;
    type OverflowPageInfo = beatree::OverflowPageInfo;

    fn submit(&mut self, store: &StoreLoadValueAsync, user_data: u64) -> Option<OverflowPageInfo> {
        AsyncLookup::submit(self, &store.io_handle, user_data)
    }

    fn try_complete(
        &mut self,
        completion: FatPage,
        overflow_meta: Option<OverflowPageInfo>,
    ) -> Option<Option<Vec<u8>>> {
        self.try_finish(completion, overflow_meta)
    }
}

impl LoadValueAsync for StoreLoadValueAsync {
    type Completion = FatPage;
    type Pending = AsyncLookup;
    type RawCompletion = CompleteIo;

    fn start_load(
        &self,
        key_path: KeyPath,
        user_data: u64,
    ) -> Result<Option<Vec<u8>>, Self::Pending> {
        if let Some(change) = self.overlay.value(&key_path) {
            return Ok(change.as_option().map(|v| v.to_vec()));
        }

        self.read_tx
            .lookup_async(key_path, &self.io_handle, user_data)
    }

    fn raw_completion_stream(&self) -> Receiver<Self::RawCompletion> {
        // using just the receiver ensures that when `self` is dropped, the completion stream ends.
        self.io_handle.receiver().clone()
    }

    fn process_raw_completion(
        complete_io: Self::RawCompletion,
    ) -> (u64, anyhow::Result<Self::Completion>) {
        let user_data = complete_io.command.user_data;
        let res = complete_io
            .result
            .map(|()| complete_io.command.kind.unwrap_buf());

        (user_data, res.map_err(Into::into))
    }
}
