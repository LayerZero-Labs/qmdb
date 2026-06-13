//! This module declares messages that are exchanged between the supervisor and child processes.
//!
//! This is used only for inter-process communication and thus doesn't need to care about versioning
//! or compatibility.

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub type Key = [u8; 32];
pub type Value = Vec<u8>;

/// A change in the key-value store.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum KeyValueChange {
    /// A key-value pair was inserted.
    Insert(Key, Value),
    /// A key-value pair was deleted.
    Delete(Key),
}

impl KeyValueChange {
    /// Returns the key that this changes pertains to.
    pub fn key(&self) -> &Key {
        match *self {
            KeyValueChange::Insert(ref key, _) | KeyValueChange::Delete(ref key) => key,
        }
    }

    pub fn value(&self) -> Option<Value> {
        match self {
            KeyValueChange::Insert(_, val) => Some(val.clone()),
            KeyValueChange::Delete(_) => None,
        }
    }
}

/// The parameters for the [`ToAgent::Init`] message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InitPayload {
    /// ID string that can be used to identify the agent. This is used for logging and debugging.
    pub id: String,
    /// The directory where the child should store the data and other files (such as logs).
    ///
    /// The directory must exist.
    pub workdir: String,
    /// Whether the workload will be entirely executed on top of trickfs.
    ///
    /// If so, another directory called "trickfs" will be added to the workdir.
    pub trickfs: bool,
}

/// The parameters for the [`ToAgent::Open`] message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenPayload {
    /// The seed that should be used for bitbox.
    ///
    /// Only used upon creation a new NOMT db.
    pub bitbox_seed: [u8; 16],
    /// Whether the agent is supposed to handle rollbacks.
    /// If `Some`, the maximum number of supported blocks in a single rollback is specified.
    pub rollback: Option<usize>,
    /// The number of commit workers.
    pub commit_concurrency: usize,
    /// The number of io_uring instances.
    pub io_workers: usize,
    /// The number of pages within the ht.
    pub hashtable_buckets: u32,
    /// Whether merkle page fetches should be warmed up while sessions are ongoing.
    pub warm_up: bool,
    /// Whether to preallocate the hashtable file.
    pub preallocate_ht: bool,
    /// The maximum size of the page cache.
    pub page_cache_size: usize,
    /// The maximum size of the leaf cache.
    pub leaf_cache_size: usize,
    /// Whether to prepopulate the upper layers of the page cache on startup.
    pub prepopulate_page_cache: bool,
    /// Number of upper layers contained in the cache.
    pub page_cache_upper_levels: usize,
}

/// The parameters for the [`ToAgent::Commit`] message.
#[derive(Debug, Serialize, Deserialize)]
pub struct CommitPayload {
    /// The set of keys that should be read.
    pub reads: Vec<Key>,
    /// The number of concurrent readers.
    ///
    /// It must be greater than 0.
    pub read_concurrency: usize,
    /// The set of changes that the child should commit.
    ///
    /// There must be no duplicate keys in the set.
    pub changeset: Vec<KeyValueChange>,
    /// If Some the supervisor expects the commit to crash,
    /// the crash should happen after the specified amount of time.
    pub should_crash: Option<Duration>,
}

/// The parameters for the [`ToAgent::Rollback`] message.
#[derive(Debug, Serialize, Deserialize)]
pub struct RollbackPayload {
    /// The number of commits that need to be rolled back.
    pub n_commits: usize,
    /// If Some the supervisor expects the rollback to crash,
    /// the crash should happen after the specified amount of time.
    pub should_crash: Option<Duration>,
}

/// The maximum size of an envelope, in the serialized form.
pub const MAX_ENVELOPE_SIZE: usize = 128 * 1024 * 1024;

/// A wrapper around a message that adds a request number.
#[derive(Debug, Serialize, Deserialize)]
pub struct Envelope<T> {
    /// The request number. This is used to match responses to requests.
    ///
    /// The request number is unique for each request. The response should have the same request
    /// number as the request that caused it.
    pub reqno: u64,
    /// The message itself.
    pub message: T,
}

/// Messages sent from the supervisor to the agent process.
#[derive(Debug, Serialize, Deserialize)]
pub enum ToAgent {
    /// The first message sent by the supervisor to the child process. Contains the parameters the
    /// supervisor expects the child to use. Usually sent only once per child process.
    Init(InitPayload),
    /// The supervisor sends this message to the child process to instruct it to open a database
    /// with the given parameters.
    Open(OpenPayload),
    /// The supervisor sends this message to the child process to indicate that the child should
    /// commit.
    Commit(CommitPayload),
    /// The supervisor sends this message to the child process to indicate that the child should
    /// perform a rollback.
    Rollback(RollbackPayload),
    /// The supervisor sends this message to the child process to query the value of a given key.
    Query(Key),
    /// The supervisor sends this message to the child process to query the current sequence number
    /// of the database.
    QuerySyncSeqn,
    /// The supervisor sends this message to the child process to indicate that the child should
    /// do a clean shutdown.
    GracefulShutdown,
}

/// Different outcomes of an operation.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub enum Outcome {
    /// The operation was successful.
    Success,
    /// The operation failed because the storage is full, dubbed ENOSPC.
    StorageFull,
    /// Some other failure occurred.
    UnknownFailure(String),
}

/// Elaboration on the agent initialization result inside of [`ToSupervisor::InitResponse`].
#[derive(Debug, Serialize, Deserialize)]
pub enum InitOutcome {
    /// The agent successfully initialized.
    Success,
    /// The agent failed to initialize because the workdir does not exist.
    ///
    /// This is the supervisor's failure.
    WorkdirDoesNotExist,
}

/// Elaboration on the opening the database result inside of [`ToSupervisor::OpenResponse`].
#[derive(Debug, Serialize, Deserialize)]
pub enum OpenOutcome {
    /// The agent successfully opened the database.
    Success,
    /// The agent failed to initialize because the volume is full.
    StorageFull,
    /// Uncategorised failure has happened with the given message.
    UnknownFailure(String),
}

/// Messages sent from the agent to the supervisor.
#[derive(Debug, Serialize, Deserialize)]
pub enum ToSupervisor {
    /// A generic acknowledgment message.
    Ack,
    /// The response to the [`ToAgent::Init`] request.
    InitResponse(InitOutcome),
    /// The response to the [`ToAgent::Open`] request.
    OpenResponse(OpenOutcome),
    /// The response to a completed commit request.
    CommitResponse {
        /// The time it took for the operation to complete.
        elapsed: Duration,
        /// The outcome of the commit.
        outcome: Outcome,
    },
    /// The response to a completed rollback request.
    RollbackResponse {
        /// The outcome of the rollback.
        outcome: Outcome,
    },
    /// The response to a query for a key-value pair.
    QueryValue(Option<Value>),
    /// The response to a query for the current sequence number of the database.
    SyncSeqn(u32),
}
