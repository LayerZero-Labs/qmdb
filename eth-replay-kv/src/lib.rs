use anyhow::Context;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use std::collections::{btree_map::Entry, BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::sync::Arc;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_RETRIES: u32 = 3;
const RETRY_DELAY_MS: u64 = 2000;

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub enum Operation {
    Create(Vec<u8>, Vec<u8>), // key, value
    Read(Vec<u8>, Vec<u8>),   // key, value (empty value = miss)
    Update(Vec<u8>, Vec<u8>), // key, value
    Delete(Vec<u8>),          // key
}

/// Bundle of pre-computed init_db + boundary_ops, written by gen-init-db,
/// loaded by speed.rs (--init-db-path) to skip Phase 1 (BaseStateSplitter).
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct InitDbBundle {
    pub populate_start: u64,
    pub populate_end: u64,
    pub init_db: Vec<(Vec<u8>, Vec<u8>)>,
    pub boundary_ops: HashMap<u64, Vec<Operation>>,
}

/// A local directory or S3 URI. Used for both input and output paths.
#[derive(Debug, Clone)]
pub enum StoragePath {
    LocalDir(PathBuf),
    S3 { bucket: String, prefix: String },
}

impl StoragePath {
    /// Returns `true` when this path points to S3.
    pub fn is_s3(&self) -> bool {
        matches!(self, StoragePath::S3 { .. })
    }
}

/// Parse a local directory (e.g. `./kv/`) or an S3 URI like `s3://my-bucket/some/prefix`.
pub fn parse_storage_path(path: impl AsRef<str>) -> StoragePath {
    let path = path.as_ref();
    if let Some(rest) = path.strip_prefix("s3://") {
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        StoragePath::S3 {
            bucket: bucket.to_string(),
            prefix: prefix.trim_matches('/').to_string(),
        }
    } else {
        StoragePath::LocalDir(PathBuf::from(path))
    }
}

/// Create an S3 client using default AWS config.
pub async fn new_s3_client() -> aws_sdk_s3::Client {
    let cfg = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    aws_sdk_s3::Client::new(&cfg)
}

/// Build S3 key from prefix and filename (used for both input and output).
pub fn s3_key(prefix: &str, filename: &str) -> String {
    let p = prefix.trim_matches('/');
    if p.is_empty() {
        filename.to_string()
    } else {
        format!("{}/{}", p, filename)
    }
}

/// Upload a local file to S3.
pub async fn upload_file_to_s3(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    path: impl AsRef<Path>,
) -> anyhow::Result<()> {
    let path = path.as_ref();
    for attempt in 0..MAX_RETRIES {
        let body = ByteStream::read_from()
            .path(path)
            .build()
            .await
            .with_context(|| format!("Failed to read file for upload {:?}", path))?;
        match client.put_object().bucket(bucket).key(key).body(body).send().await {
            Ok(_) => return Ok(()),
            Err(e) if attempt + 1 < MAX_RETRIES => {
                eprintln!("S3 upload retry {}/{} for {}: {}", attempt + 1, MAX_RETRIES, key, e);
                tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS * (attempt as u64 + 1))).await;
            }
            Err(e) => return Err(anyhow::anyhow!(e).context(format!(
                "Failed to upload to s3://{}/{}", bucket, key
            ))),
        }
    }
    unreachable!()
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

pub async fn read_s3_bytes(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    for attempt in 0..MAX_RETRIES {
        match client.get_object().bucket(bucket).key(key).send().await {
            Ok(output) => {
                match output.body.collect().await {
                    Ok(aggregated) => return Ok(Some(aggregated.into_bytes().to_vec())),
                    Err(e) if attempt + 1 < MAX_RETRIES => {
                        eprintln!("S3 download retry {}/{} for {}: {}", attempt + 1, MAX_RETRIES, key, e);
                        tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS * (attempt as u64 + 1))).await;
                        continue;
                    }
                    Err(e) => return Err(anyhow::anyhow!(e).context("Failed to read S3 object body")),
                }
            }
            Err(SdkError::ServiceError(e)) if e.err().is_no_such_key() => return Ok(None),
            Err(err) if attempt + 1 < MAX_RETRIES => {
                eprintln!("S3 download retry {}/{} for {}: {}", attempt + 1, MAX_RETRIES, key, err);
                tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS * (attempt as u64 + 1))).await;
            }
            Err(err) => return Err(anyhow::anyhow!(err).context(format!(
                "Failed to fetch s3://{}/{}",
                bucket, key
            ))),
        }
    }
    unreachable!()
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

/// Read raw bytes for `filename` from a `StoragePath` (local or S3).
pub async fn read_bytes(
    source: &StoragePath,
    s3_client: Option<&aws_sdk_s3::Client>,
    filename: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    let result = match source {
        StoragePath::LocalDir(dir) => read_file_bytes(dir.join(filename)),
        StoragePath::S3 { bucket, prefix } => {
            let client = s3_client.context("S3 client not initialized")?;
            read_s3_bytes(client, bucket, &s3_key(prefix, filename)).await
        }
    };
    if let Err(ref err) = result {
        panic_on_too_many_open_files(err);
    }
    result
}

pub async fn load_optional_bincode<T: serde::de::DeserializeOwned>(
    source: &StoragePath,
    s3_client: Option<&aws_sdk_s3::Client>,
    filename: &str,
) -> anyhow::Result<Option<T>> {
    match read_bytes(source, s3_client, filename).await? {
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

/// Load both base_kv and operation_kv for a block in parallel (two concurrent reads).
/// Overlaps S3/local I/O and helps when used with a prefetch pipeline.
pub async fn load_block_data(
    source: &StoragePath,
    s3_client: Option<&aws_sdk_s3::Client>,
    height: u64,
) -> anyhow::Result<BlockData> {
    let base_name = format!("{}_base_kv.bin", height);
    let ops_name = format!("{}_operation_kv.bin", height);
    let (base_kv, ops) = tokio::join!(
        load_optional_bincode::<Vec<(Vec<u8>, Vec<u8>)>>(source, s3_client, &base_name),
        load_optional_bincode::<Vec<Operation>>(source, s3_client, &ops_name),
    );
    Ok((base_kv?, ops?))
}

/// Number of blocks to prefetch: channel capacity and max concurrent S3 reads.
/// Producer runs up to this many block loads in parallel, reorders by height,
/// and sends in block order so the consumer still processes sequentially.
pub const PREFETCH_BUFFER_BLOCKS: usize = 100;

/// Runs a block-data producer that loads up to `PREFETCH_BUFFER_BLOCKS` blocks
/// in parallel and sends `(height, base_kv, ops)` to `tx` in ascending height order.
/// Spawn this in a task and receive on the channel in the main thread.
pub async fn run_block_data_producer(
    start_height: u64,
    end_height: u64,
    source: StoragePath,
    client: Option<aws_sdk_s3::Client>,
    tx: tokio::sync::mpsc::Sender<
        Result<(u64, Option<Vec<(Vec<u8>, Vec<u8>)>>, Option<Vec<Operation>>), anyhow::Error>,
    >,
) -> anyhow::Result<()> {
    use futures::StreamExt;
    let source = Arc::new(source);
    let mut pending: BTreeMap<u64, BlockData> = BTreeMap::new();
    let mut next_to_send = start_height;
    let stream = futures::stream::iter(start_height..=end_height)
        .map(|height| {
            let source = Arc::clone(&source);
            let client = client.clone();
            async move {
                let r = load_block_data(source.as_ref(), client.as_ref(), height).await;
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
    source: StoragePath,
    client: Option<aws_sdk_s3::Client>,
    tx: tokio::sync::mpsc::Sender<Result<(u64, Option<Vec<Operation>>), anyhow::Error>>,
) -> anyhow::Result<()> {
    use futures::StreamExt;
    let source = Arc::new(source);
    let mut pending: BTreeMap<u64, Option<Vec<Operation>>> = BTreeMap::new();
    let mut next_to_send = start_height;
    let stream = futures::stream::iter(start_height..=end_height)
        .map(|height| {
            let source = Arc::clone(&source);
            let client = client.clone();
            async move {
                let ops_name = format!("{}_operation_kv.bin", height);
                let r =
                    load_optional_bincode::<Vec<Operation>>(source.as_ref(), client.as_ref(), &ops_name).await;
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
