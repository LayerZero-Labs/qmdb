//! Ethereum replay workload: per-block `{height}_base_kv.bin` and
//! `{height}_operation_kv.bin` files (bincode) read from a local directory, loaded
//! in parallel and delivered in block order, and the splitter that derives the
//! initial state from the base_kv snapshots.

use anyhow::Context;
use std::collections::{btree_map::Entry, BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(serde::Deserialize, Debug)]
pub enum Operation {
    Create(Vec<u8>, Vec<u8>), // key, value
    Read(Vec<u8>, Vec<u8>),   // key, value (empty value = miss)
    Update(Vec<u8>, Vec<u8>), // key, value
    Delete(Vec<u8>),          // key
}

pub fn decode_bincode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> anyhow::Result<T> {
    bincode::deserialize(bytes).context("Failed to deserialize bincode")
}

pub fn read_file_bytes(path: impl AsRef<Path>) -> anyhow::Result<Option<Vec<u8>>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(None);
    }
    let mut file = File::open(path).with_context(|| format!("Failed to open file {:?}", path))?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)
        .with_context(|| format!("Failed to read file {:?}", path))?;
    Ok(Some(buffer))
}

/// Panic with a helpful message if an error is caused by too many open files.
fn panic_on_too_many_open_files(err: &anyhow::Error) {
    for cause in err.chain() {
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>() {
            if io_err.raw_os_error() == Some(24) {
                panic!(
                    "Too many open files (EMFILE). \
                     Raise the file-descriptor limit and retry:\n\n    \
                     ulimit -n 102400\n"
                );
            }
        }
    }
}

/// Read raw bytes for `filename` from the trace directory.
fn read_bytes(dir: &Path, filename: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let result = read_file_bytes(dir.join(filename));
    if let Err(ref err) = result {
        panic_on_too_many_open_files(err);
    }
    result
}

pub async fn load_optional_bincode<T: serde::de::DeserializeOwned>(
    dir: &Path,
    filename: &str,
) -> anyhow::Result<Option<T>> {
    match read_bytes(dir, filename)? {
        Some(bytes) => Ok(Some(decode_bincode::<T>(&bytes)?)),
        None => Ok(None),
    }
}

/// Data for one block: base_kv snapshot and operation list.
/// Used by prefetch pipeline to overlap I/O with processing.
pub type BlockData = (
    Option<Vec<(Vec<u8>, Vec<u8>)>>,
    Option<Vec<Operation>>,
);

/// Load both base_kv and operation_kv for a block (two concurrent reads).
pub async fn load_block_data(dir: &Path, height: u64) -> anyhow::Result<BlockData> {
    let base_name = format!("{}_base_kv.bin", height);
    let ops_name = format!("{}_operation_kv.bin", height);
    let (base_kv, ops) = tokio::join!(
        load_optional_bincode::<Vec<(Vec<u8>, Vec<u8>)>>(dir, &base_name),
        load_optional_bincode::<Vec<Operation>>(dir, &ops_name),
    );
    Ok((base_kv?, ops?))
}

/// Number of blocks to prefetch: channel capacity and max concurrent reads.
/// Producer runs up to this many block loads in parallel, reorders by height,
/// and sends in block order so the consumer still processes sequentially.
pub const PREFETCH_BUFFER_BLOCKS: usize = 100;

/// Runs a block-data producer that loads up to `PREFETCH_BUFFER_BLOCKS` blocks
/// in parallel and sends `(height, base_kv, ops)` to `tx` in ascending height order.
/// Spawn this in a task and receive on the channel in the main thread.
pub async fn run_block_data_producer(
    start_height: u64,
    end_height: u64,
    dir: PathBuf,
    tx: tokio::sync::mpsc::Sender<
        Result<(u64, Option<Vec<(Vec<u8>, Vec<u8>)>>, Option<Vec<Operation>>), anyhow::Error>,
    >,
) -> anyhow::Result<()> {
    use futures::StreamExt;
    let dir = Arc::new(dir);
    let mut pending: BTreeMap<u64, BlockData> = BTreeMap::new();
    let mut next_to_send = start_height;
    let stream = futures::stream::iter(start_height..=end_height)
        .map(|height| {
            let dir = Arc::clone(&dir);
            async move {
                let r = load_block_data(dir.as_ref(), height).await;
                (height, r)
            }
        })
        .buffer_unordered(PREFETCH_BUFFER_BLOCKS);

    tokio::pin!(stream);
    while let Some((height, result)) = stream.next().await {
        let data = result?;
        match pending.entry(height) {
            Entry::Vacant(v) => {
                v.insert(data);
            }
            Entry::Occupied(_) => unreachable!("each height loaded once"),
        }
        while let Some(data) = pending.remove(&next_to_send) {
            if tx.send(Ok((next_to_send, data.0, data.1))).await.is_err() {
                return Ok(());
            }
            next_to_send += 1;
        }
    }
    Ok(())
}

/// Runs an ops-only producer: loads `{height}_operation_kv.bin` for up to
/// `PREFETCH_BUFFER_BLOCKS` heights in parallel, sends in block order.
pub async fn run_ops_producer(
    start_height: u64,
    end_height: u64,
    dir: PathBuf,
    tx: tokio::sync::mpsc::Sender<Result<(u64, Option<Vec<Operation>>), anyhow::Error>>,
) -> anyhow::Result<()> {
    use futures::StreamExt;
    let dir = Arc::new(dir);
    let mut pending: BTreeMap<u64, Option<Vec<Operation>>> = BTreeMap::new();
    let mut next_to_send = start_height;
    let stream = futures::stream::iter(start_height..=end_height)
        .map(|height| {
            let dir = Arc::clone(&dir);
            async move {
                let ops_name = format!("{}_operation_kv.bin", height);
                let r = load_optional_bincode::<Vec<Operation>>(dir.as_ref(), &ops_name).await;
                (height, r)
            }
        })
        .buffer_unordered(PREFETCH_BUFFER_BLOCKS);

    tokio::pin!(stream);
    while let Some((height, result)) = stream.next().await {
        let data = result?;
        match pending.entry(height) {
            Entry::Vacant(v) => {
                v.insert(data);
            }
            Entry::Occupied(_) => unreachable!("each height loaded once"),
        }
        while let Some(data) = pending.remove(&next_to_send) {
            if tx.send(Ok((next_to_send, data))).await.is_err() {
                return Ok(());
            }
            next_to_send += 1;
        }
    }
    Ok(())
}

/// Splits replay input into **base state** (init_db) and **per-block boundary ops**
/// so that applying init_db then boundary_ops at each height matches the base_kv snapshots.
///
/// Two levels of decision:
/// - **Streaming**: "Has this key ever been written by the operation stream?" → if not,
///   the key can go into base (init_db) when first seen in a base_kv snapshot.
/// - **Per-block**: For each (key, value) in a block's base_kv, decide Create/Update/Delete
///   or add to init_db, and keep internal state in sync.
pub struct BaseStateSplitter {
    /// Current key→value state after all inputs processed so far (sim).
    state: HashMap<Vec<u8>, Vec<u8>>,
    /// Streaming: keys that have been written by the operation stream (Create/Update/Delete).
    ever_written_by_ops: HashSet<Vec<u8>>,
    /// Keys classified as base-only; initial snapshot for replay.
    init_db: HashMap<Vec<u8>, Vec<u8>>,
    /// Per-block ops so replay (init_db + boundary_ops) matches base_kv snapshots.
    boundary_ops: HashMap<u64, Vec<Operation>>,
}

impl Default for BaseStateSplitter {
    fn default() -> Self {
        Self::new()
    }
}

impl BaseStateSplitter {
    pub fn new() -> Self {
        Self {
            state: HashMap::new(),
            ever_written_by_ops: HashSet::new(),
            init_db: HashMap::new(),
            boundary_ops: HashMap::new(),
        }
    }

    /// Streaming-level: key should go into base (init_db) when first seen in base_kv
    /// only if it has never been written by the operation stream.
    fn key_belongs_in_base(&self, key: &[u8]) -> bool {
        !self.ever_written_by_ops.contains(key) && !self.init_db.contains_key(key)
    }

    /// Per-block: ingest this block's base_kv snapshot. Updates internal state and
    /// either adds to init_db (base) or records boundary ops for this height.
    /// Returns keys that were newly added to init_db (for callers that track key origins).
    pub fn ingest_base_kv(
        &mut self,
        height: u64,
        base_kv: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>,
    ) -> Vec<Vec<u8>> {
        let mut added_to_base = Vec::new();
        for (k, v) in base_kv {
            if v.is_empty() {
                if self.state.contains_key(&k) {
                    self.boundary_ops
                        .entry(height)
                        .or_default()
                        .push(Operation::Delete(k.clone()));
                    self.state.remove(&k);
                }
                continue;
            }
            if let Some(sim_val) = self.state.get(&k) {
                if sim_val != &v {
                    self.boundary_ops
                        .entry(height)
                        .or_default()
                        .push(Operation::Update(k.clone(), v.clone()));
                    self.state.insert(k, v);
                }
            } else {
                self.state.insert(k.clone(), v.clone());
                if self.key_belongs_in_base(&k) {
                    self.init_db.insert(k.clone(), v);
                    added_to_base.push(k);
                } else {
                    self.boundary_ops
                        .entry(height)
                        .or_default()
                        .push(Operation::Create(k.clone(), v.clone()));
                }
            }
        }
        added_to_base
    }

    /// Streaming: apply this block's operation stream; updates state and ever_written_by_ops.
    pub fn apply_ops(&mut self, ops: impl IntoIterator<Item = Operation>) {
        for op in ops {
            match op {
                Operation::Create(k, v) | Operation::Update(k, v) => {
                    self.ever_written_by_ops.insert(k.clone());
                    self.state.insert(k, v);
                }
                Operation::Delete(k) => {
                    self.ever_written_by_ops.insert(k.clone());
                    self.state.remove(&k);
                }
                Operation::Read(_, _) => {}
            }
        }
    }

    pub fn init_db_len(&self) -> usize {
        self.init_db.len()
    }

    pub fn boundary_ops_count(&self) -> usize {
        self.boundary_ops.values().map(|v| v.len()).sum()
    }

    pub fn into_parts(
        self,
    ) -> (
        HashMap<Vec<u8>, Vec<u8>>,
        HashMap<u64, Vec<Operation>>,
    ) {
        (self.init_db, self.boundary_ops)
    }
}
