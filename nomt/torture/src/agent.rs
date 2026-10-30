use anyhow::{anyhow, bail, Result};
use futures::SinkExt as _;
use nomt::Session;
use nomt::{hasher::Blake3Hasher, Nomt, SessionParams};
use std::future::Future;
use std::path::Path;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::task::JoinSet;
use tokio::{
    io::{BufReader, BufWriter},
    net::{
        unix::{OwnedReadHalf, OwnedWriteHalf},
        UnixStream,
    },
    time::{error::Elapsed, sleep, timeout},
};
use tokio_serde::{formats::SymmetricalBincode, SymmetricallyFramed};
use tokio_stream::StreamExt as _;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::trace;

use crate::message::Key;
use crate::{
    message::{
        self, CommitPayload, Envelope, InitOutcome, KeyValueChange, OpenOutcome, OpenPayload,
        Outcome, RollbackPayload, ToAgent, ToSupervisor, MAX_ENVELOPE_SIZE,
    },
    panic::panic_to_err,
};

/// The entrypoint for the agent.
///
/// `input` is the UnixStream that the agent should use to communicate with its supervisor.
pub async fn run(input: UnixStream) -> Result<()> {
    let pid = std::process::id();
    trace!(pid, "Child process started");

    let mut stream = Stream::new(input);
    let workdir = initialize(&mut stream).await?;
    let mut agent = Agent::new();

    loop {
        // TODO: make the message processing non-blocking.
        //
        // That being said, I should probably note, that it doesn't necessarily mean we should allow
        // concurrent access to NOMT (only one session could be created at once anyway).
        let Envelope { reqno, message } = stream.recv().await?;
        match message {
            ToAgent::Commit(CommitPayload {
                reads,
                read_concurrency,
                changeset,
                should_crash: Some(crash_delay),
            }) => {
                // Ack first.
                stream
                    .send(Envelope {
                        reqno,
                        message: ToSupervisor::Ack,
                    })
                    .await?;

                let task = async move {
                    let start = std::time::Instant::now();
                    agent.begin_session();
                    agent.read(reads, read_concurrency).await;
                    let _ = agent.commit(changeset).await;
                    let elapsed = start.elapsed();
                    tracing::info!("commit took {}ms", elapsed.as_millis());
                };

                crash_task(task, crash_delay, "commit").await;
                unreachable!();
            }
            ToAgent::Commit(CommitPayload {
                reads,
                read_concurrency,
                changeset,
                should_crash: None,
            }) => {
                let start = std::time::Instant::now();
                agent.begin_session();
                agent.read(reads, read_concurrency).await;
                let outcome = agent.commit(changeset).await;
                let elapsed = start.elapsed();
                tracing::info!("commit took {}ms", elapsed.as_millis());
                stream
                    .send(Envelope {
                        reqno,
                        message: ToSupervisor::CommitResponse { elapsed, outcome },
                    })
                    .await?;
            }
            ToAgent::Rollback(RollbackPayload {
                n_commits,
                should_crash: Some(crash_delay),
            }) => {
                // Ack first.
                stream
                    .send(Envelope {
                        reqno,
                        message: ToSupervisor::Ack,
                    })
                    .await?;

                let task = async move {
                    let start = std::time::Instant::now();
                    let _ = agent.rollback(n_commits).await;
                    tracing::info!("rollback took {:?}", start.elapsed());
                };

                crash_task(task, crash_delay, "rollback").await;
                unreachable!();
            }
            ToAgent::Rollback(RollbackPayload {
                n_commits,
                should_crash: None,
            }) => {
                let start = std::time::Instant::now();
                let outcome = agent.rollback(n_commits).await;
                tracing::info!("rollback took {}ms", start.elapsed().as_millis());
                stream
                    .send(Envelope {
                        reqno,
                        message: ToSupervisor::RollbackResponse { outcome },
                    })
                    .await?;
            }
            ToAgent::Query(key) => {
                let value = agent.query(key)?;
                stream
                    .send(Envelope {
                        reqno,
                        message: ToSupervisor::QueryValue(value),
                    })
                    .await?;
            }
            ToAgent::QuerySyncSeqn => {
                let sync_seqn = agent.query_sync_seqn();
                stream
                    .send(Envelope {
                        reqno,
                        message: ToSupervisor::SyncSeqn(sync_seqn),
                    })
                    .await?;
            }
            ToAgent::GracefulShutdown => {
                stream
                    .send(Envelope {
                        reqno,
                        message: ToSupervisor::Ack,
                    })
                    .await?;
                drop(agent);
                break;
            }
            ToAgent::Open(open_params) => {
                tracing::info!("opening the database");
                let outcome = agent.perform_open(&workdir, open_params).await;
                stream
                    .send(Envelope {
                        reqno,
                        message: ToSupervisor::OpenResponse(outcome),
                    })
                    .await?;
            }
            ToAgent::Init(_) => {
                bail!("Unexpected Init message");
            }
        }
    }
    Ok(())
}

/// Execute the provided `task` and make it crash after the specified `crash_delay`.
async fn crash_task(
    task: impl Future<Output = ()> + Send + 'static,
    crash_delay: Duration,
    op: &'static str,
) {
    // TODO: implement this in the future.
    //
    // This seems to be a big feature. As of now, I envision it, as we either:
    //
    // 1. Launch a concurrent process that brings down the process (e.g. via `abort`) at
    //    some non-deterministic time in the future.
    // 2. Adjust the VFS settings so that it will crash at a specific moment. This might
    //    be some specific moment like writing to Meta or maybe writing to some part of HT.
    // 3. Introduce fail point to do the same.

    let barrier_1 = Arc::new(tokio::sync::Barrier::new(2));
    let barrier_2 = barrier_1.clone();
    let task_1 = tokio::spawn(async move {
        barrier_1.wait().await;
        sleep(crash_delay).await;
        tracing::info!("aborting {op} after {}ms", crash_delay.as_millis());
        std::process::abort();
    });
    let task_2 = tokio::spawn(async move {
        barrier_2.wait().await;
        task.await;
        std::process::abort();
    });
    let _ = tokio::join!(task_1, task_2);
    // This is unreachable because either the process will abort due to a timed
    // abort or due to the commit finishing successfully (and then aborting).
    unreachable!();
}

/// Performs the initialization of the agent.
///
/// Receives the [`ToAgent::Init`] message from the supervisor, initializes the logging, confirms
/// the receipt of the message, and returns the path to the working directory.
///
/// # Errors
///
/// Returns an error if the message is not an Init message or if the message is not received
/// within a certain time limit.
async fn initialize(stream: &mut Stream) -> Result<PathBuf> {
    // The supervisor can be busy initializing and handling multiple workloads at the same time.
    const DEADLINE: Duration = Duration::from_secs(3);
    let Envelope { reqno, message } = match timeout(DEADLINE, stream.recv()).await {
        Ok(envelope) => envelope?,
        Err(Elapsed { .. }) => {
            anyhow::bail!("Timed out waiting for Init message");
        }
    };
    let ToAgent::Init(init) = message else {
        anyhow::bail!("Expected Init message");
    };

    crate::logging::init_agent(&init.id, &init.workdir);

    let mut workdir = PathBuf::from(&init.workdir);

    // Add another directory to the workdir if trickfs is specified.
    //
    // This allows to mount trickfs on <workdir>/trickfs and keep all the log
    // and reproducibility data within just <workdir>.
    if init.trickfs {
        workdir = workdir.join("trickfs");
    }

    if !workdir.exists() {
        stream
            .send(Envelope {
                reqno,
                message: ToSupervisor::InitResponse(InitOutcome::WorkdirDoesNotExist),
            })
            .await?;
        bail!("Workdir does not exist");
    } else {
        stream
            .send(Envelope {
                reqno,
                message: ToSupervisor::InitResponse(InitOutcome::Success),
            })
            .await?;
        Ok(workdir)
    }
}

struct Agent {
    nomt: Option<Nomt<Blake3Hasher>>,
    session: Option<Session<Blake3Hasher>>,
}

impl Agent {
    fn new() -> Self {
        Self {
            nomt: None,
            session: None,
        }
    }

    /// Open a NOMT instance at the specified path with the specified open parameters.
    ///
    /// Drops any pending session and instance.
    async fn perform_open(&mut self, workdir: &Path, open_params: OpenPayload) -> OpenOutcome {
        if let Some(nomt) = self.nomt.take() {
            // Drop any pending session.
            let _ = self.session.take();
            tracing::trace!("dropping the existing NOMT instance");
            drop(nomt);
        }

        let mut o = nomt::Options::new();
        o.path(workdir.join("nomt_db"));
        o.bitbox_seed(open_params.bitbox_seed);
        o.hashtable_buckets(open_params.hashtable_buckets);
        o.warm_up(open_params.warm_up);
        o.preallocate_ht(open_params.preallocate_ht);
        o.page_cache_size(open_params.page_cache_size);
        o.leaf_cache_size(open_params.leaf_cache_size);
        o.prepopulate_page_cache(open_params.prepopulate_page_cache);
        o.page_cache_upper_levels(open_params.page_cache_upper_levels);
        if let Some(n_commits) = open_params.rollback {
            o.rollback(true);
            o.max_rollback_log_len(n_commits as u32);
        } else {
            o.rollback(false);
        }
        let nomt = match block_in_place(|| Nomt::open(o), "Panic opening nomt") {
            Ok(nomt) => nomt,
            Err(ref err) if is_enospc(err) => return OpenOutcome::StorageFull,
            Err(ref err) => return OpenOutcome::UnknownFailure(err.to_string()),
        };
        self.nomt = Some(nomt);
        OpenOutcome::Success
    }

    /// Begin a session, this must be called before `read` and `commit`,
    /// which require an opened session.
    ///
    /// Panics if a session was already opened.
    fn begin_session(&mut self) {
        // UNWRAP: `nomt` is always `Some` except recreation.
        let nomt = self.nomt.as_ref().unwrap();
        assert!(self.session.is_none());
        self.session
            .replace(nomt.begin_session(SessionParams::default()));
    }

    /// Read the specified keys from an already opened session, it requires
    /// `begin_session` to have been called.
    async fn read(&mut self, reads: Vec<Key>, read_concurrency: usize) {
        // UNWRAP: `read` is expected to be called after `begin_session`.
        let session = Arc::new(self.session.take().unwrap());
        let reads = Arc::new(reads);

        let mut reads_task = JoinSet::<()>::new();
        let reads_per_thread = reads.len() / read_concurrency;
        for i in 0..read_concurrency {
            let start = i * reads_per_thread;
            let end = if i == read_concurrency - 1 {
                reads.len()
            } else {
                (i + 1) * reads_per_thread
            };
            reads_task.spawn_blocking({
                let reads = reads.clone();
                let session = session.clone();
                move || {
                    for key in &reads[start..end] {
                        let _res = session.read(*key).expect("read failed");
                    }
                }
            });
        }
        reads_task.join_all().await;

        self.session.replace(Arc::into_inner(session).unwrap());
    }

    /// Commit the specified changeset to an already opened session, it requires
    /// `begin_session` to have been called.
    ///
    /// It consumes the session, thus after the call the session is expected to be reopened .
    async fn commit(&mut self, changeset: Vec<KeyValueChange>) -> Outcome {
        // UNWRAP: `nomt` is always `Some` except recreation.
        let nomt = self.nomt.as_ref().unwrap();
        // UNWRAP: `commit` is expected to be called after `begin_session`.
        let session = self.session.take().unwrap();

        let mut actuals = Vec::with_capacity(changeset.len());
        for change in changeset {
            match change {
                KeyValueChange::Insert(key, value) => {
                    actuals.push((key, nomt::KeyReadWrite::Write(Some(value))));
                }
                KeyValueChange::Delete(key) => {
                    actuals.push((key, nomt::KeyReadWrite::Write(None)));
                }
            }
        }

        // Perform the commit.
        let commit_result =
            block_in_place(|| session.finish(actuals)?.commit(&nomt), "Panic in commit");
        let commit_outcome = classify_result(commit_result);

        // Log the outcome if it was not successful.
        if !matches!(commit_outcome, Outcome::Success) {
            trace!("unsuccessful commit: {:?}", commit_outcome);
        }

        commit_outcome
    }

    /// Perform a rollback of `n_commits`.
    async fn rollback(&mut self, n_commits: usize) -> Outcome {
        // UNWRAP: `nomt` is always `Some` except recreation.
        let nomt = self.nomt.as_ref().unwrap();

        // Perform the rollback.

        let rollback_result = block_in_place(|| nomt.rollback(n_commits), "Panic in rollback");
        let rollback_outcome = classify_result(rollback_result);

        // Log the outcome if it was not successful.
        if !matches!(rollback_outcome, Outcome::Success) {
            trace!("unsuccessful rollback: {:?}", rollback_outcome);
        }

        rollback_outcome
    }

    fn query(&mut self, key: message::Key) -> Result<Option<message::Value>> {
        // UNWRAP: `nomt` is always `Some` except recreation.
        let nomt = self.nomt.as_ref().unwrap();
        let value = nomt.read(key)?;
        Ok(value)
    }

    fn query_sync_seqn(&mut self) -> u32 {
        // UNWRAP: `nomt` is always `Some` except recreation.
        let nomt = self.nomt.as_ref().unwrap();
        nomt.sync_seqn()
    }
}

/// Runs the provided blocking function on the current thread without
/// blocking the executor, handling panics and returning them as stringified errors
/// along with the specified `panic_context`.
fn block_in_place<F, R>(task: F, panic_context: &str) -> anyhow::Result<R>
where
    R: Send,
    F: FnOnce() -> anyhow::Result<R> + Send,
{
    tokio::task::block_in_place(|| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task()))
            .unwrap_or_else(|e| panic_to_err(panic_context, e))
    })
}

/// Classify an operation result into one of the outcome.
fn classify_result(operation_result: anyhow::Result<()>) -> Outcome {
    match operation_result {
        Ok(()) => Outcome::Success,
        Err(ref err) if is_enospc(err) => Outcome::StorageFull,
        Err(err) => Outcome::UnknownFailure(err.to_string()),
    }
}

/// Examines the given error to determine if it is an `ENOSPC` IO error.
fn is_enospc(err: &anyhow::Error) -> bool {
    let Some(io_err) = err.downcast_ref::<std::io::Error>() else {
        return false;
    };
    let Some(errno) = io_err.raw_os_error() else {
        return false;
    };
    errno == libc::ENOSPC
}

/// Abstraction over the stream of messages from the supervisor.
struct Stream {
    rd_stream: SymmetricallyFramed<
        FramedRead<BufReader<OwnedReadHalf>, LengthDelimitedCodec>,
        Envelope<ToAgent>,
        SymmetricalBincode<Envelope<ToAgent>>,
    >,
    wr_stream: SymmetricallyFramed<
        FramedWrite<BufWriter<OwnedWriteHalf>, LengthDelimitedCodec>,
        Envelope<ToSupervisor>,
        SymmetricalBincode<Envelope<ToSupervisor>>,
    >,
}

impl Stream {
    /// Creates a stream wrapper for the given unix stream.
    fn new(command_stream: UnixStream) -> Self {
        let (rd, wr) = command_stream.into_split();
        let rd_stream = SymmetricallyFramed::new(
            FramedRead::new(
                BufReader::new(rd),
                LengthDelimitedCodec::builder()
                    .length_field_length(8)
                    .max_frame_length(MAX_ENVELOPE_SIZE)
                    .new_codec(),
            ),
            SymmetricalBincode::default(),
        );
        let wr_stream = SymmetricallyFramed::new(
            FramedWrite::new(
                BufWriter::new(wr),
                LengthDelimitedCodec::builder()
                    .length_field_length(8)
                    .max_frame_length(MAX_ENVELOPE_SIZE)
                    .new_codec(),
            ),
            SymmetricalBincode::default(),
        );
        Self {
            rd_stream,
            wr_stream,
        }
    }

    async fn recv(&mut self) -> Result<Envelope<ToAgent>> {
        match self.rd_stream.try_next().await {
            Ok(Some(envelope)) => Ok(envelope),
            Ok(None) => bail!("EOF"),
            Err(e) => Err(anyhow!(e)),
        }
    }

    async fn send(&mut self, message: Envelope<ToSupervisor>) -> Result<()> {
        self.wr_stream.send(message).await?;
        Ok(())
    }
}
