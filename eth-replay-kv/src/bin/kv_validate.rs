use clap::Parser;
use eth_replay_kv::{
    new_s3_client, parse_storage_path, run_block_data_producer, run_ops_producer,
    BaseStateSplitter, Operation, PREFETCH_BUFFER_BLOCKS, StoragePath,
};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashMap;
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(
    name = "kv-validate",
    about = "Replays KV operations and reports ordering outliers",
    long_about = "Builds a KV store from base snapshots, then replays operation_kv files block by block. \
                  Instead of panicking, it collects and reports all ordering violations: \
                  (1) Update on a non-existent key, (2) Delete on a non-existent key, \
                  (3) Create on an already-existing key, (4) Read value mismatch."
)]
struct Args {
    /// First block height (inclusive).
    #[arg(required = true)]
    start_height: u64,

    /// Last block height (inclusive).
    #[arg(required = true)]
    end_height: u64,

    /// Directory (or S3 URI) containing `{height}_base_kv.bin` and `{height}_operation_kv.bin`.
    #[arg(required = true)]
    input: String,

    /// Print every individual outlier (can be very verbose).
    #[arg(long)]
    verbose: bool,

    /// Maximum number of individual outliers to print per violation type per block (when --verbose).
    #[arg(long, default_value = "10")]
    max_per_block: usize,
}

#[derive(Default, Debug, Clone, Copy)]
struct OutlierCounts {
    create_exists: u64,
    update_missing: u64,
    delete_missing: u64,
    read_mismatch: u64,
}

impl OutlierCounts {
    fn total(&self) -> u64 {
        self.create_exists + self.update_missing + self.delete_missing + self.read_mismatch
    }

    fn add(&mut self, other: &OutlierCounts) {
        self.create_exists += other.create_exists;
        self.update_missing += other.update_missing;
        self.delete_missing += other.delete_missing;
        self.read_mismatch += other.read_mismatch;
    }
}

#[derive(Default, Debug, Clone, Copy)]
struct OpCounts {
    create: u64,
    read: u64,
    update: u64,
    delete: u64,
}

impl OpCounts {
    fn total(&self) -> u64 {
        self.create + self.read + self.update + self.delete
    }

    fn add(&mut self, other: &OpCounts) {
        self.create += other.create;
        self.read += other.read;
        self.update += other.update;
        self.delete += other.delete;
    }
}

fn hex_key(key: &[u8]) -> String {
    hex::encode(key)
}

#[derive(Debug, Clone)]
enum KeyOrigin {
    Base { height: u64 },
    Changeset { height: u64, idx: usize, op: &'static str },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let source = parse_storage_path(&args.input);

    let s3_client = if source.is_s3() {
        Some(new_s3_client().await)
    } else {
        None
    };

    let (mut db, boundary_ops, mut key_origins) =
        populate_base_state(args.start_height, args.end_height, &source, s3_client.as_ref())
            .await?;

    let (total_ops, total_outliers, outlier_details) = run_validated_replay(
        args.start_height,
        args.end_height,
        &source,
        s3_client.as_ref(),
        &mut db,
        &boundary_ops,
        &mut key_origins,
        args.verbose,
        args.max_per_block,
    )
    .await?;

    println!();
    if !outlier_details.is_empty() {
        println!("========== Outlier Keys ==========");
        for detail in &outlier_details {
            println!("{}", detail);
        }
        println!("==================================");
        println!();
    }
    println!("========== Summary ==========");
    println!(
        "Range: {}..={}",
        args.start_height, args.end_height
    );
    println!(
        "Total operations: {} (create={} read={} update={} delete={})",
        total_ops.total(),
        total_ops.create,
        total_ops.read,
        total_ops.update,
        total_ops.delete
    );
    println!(
        "Total outliers:   {} / {} ({:.4}%)",
        total_outliers.total(),
        total_ops.total(),
        if total_ops.total() > 0 {
            total_outliers.total() as f64 * 100.0 / total_ops.total() as f64
        } else {
            0.0
        }
    );
    println!(
        "  create-on-existing-key:  {} / {} creates ({:.4}%)",
        total_outliers.create_exists,
        total_ops.create,
        if total_ops.create > 0 {
            total_outliers.create_exists as f64 * 100.0 / total_ops.create as f64
        } else {
            0.0
        }
    );
    println!(
        "  read-value-mismatch:     {} / {} reads ({:.4}%)",
        total_outliers.read_mismatch,
        total_ops.read,
        if total_ops.read > 0 {
            total_outliers.read_mismatch as f64 * 100.0 / total_ops.read as f64
        } else {
            0.0
        }
    );
    println!(
        "  update-on-missing-key:   {} / {} updates ({:.4}%)",
        total_outliers.update_missing,
        total_ops.update,
        if total_ops.update > 0 {
            total_outliers.update_missing as f64 * 100.0 / total_ops.update as f64
        } else {
            0.0
        }
    );
    println!(
        "  delete-on-missing-key:   {} / {} deletes ({:.4}%)",
        total_outliers.delete_missing,
        total_ops.delete,
        if total_ops.delete > 0 {
            total_outliers.delete_missing as f64 * 100.0 / total_ops.delete as f64
        } else {
            0.0
        }
    );
    println!("Final DB size: {} keys", db.len());
    println!("=============================");

    Ok(())
}

/// Phase 1: Same logic as eth-replay-kv main — build init_db and boundary_ops
/// so that replay state matches the main binary (no spurious outliers from
/// cross-block base_kv / miner rewards).
async fn populate_base_state(
    start_height: u64,
    end_height: u64,
    source: &StoragePath,
    s3_client: Option<&aws_sdk_s3::Client>,
) -> anyhow::Result<(
    HashMap<Vec<u8>, Vec<u8>>,
    HashMap<u64, Vec<Operation>>,
    HashMap<Vec<u8>, KeyOrigin>,
)> {
    type Msg = Result<(u64, Option<Vec<(Vec<u8>, Vec<u8>)>>, Option<Vec<Operation>>), anyhow::Error>;
    let (tx, mut rx) = mpsc::channel::<Msg>(PREFETCH_BUFFER_BLOCKS);
    let source = source.clone();
    let client = s3_client.cloned();
    let producer = tokio::spawn(async move {
        run_block_data_producer(start_height, end_height, source, client, tx).await
    });

    let mut splitter = BaseStateSplitter::new();
    let mut key_origins: HashMap<Vec<u8>, KeyOrigin> = HashMap::new();
    let num_blocks = end_height - start_height + 1;
    let pb = ProgressBar::new(num_blocks);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} blocks ({per_sec}, ETA {eta}) {msg}",
        )
        .unwrap()
        .progress_chars("=>-"),
    );
    pb.set_message("Phase 1: Populating base state (prefetch pipeline)");

    // Process in block order: producer sends ascending height, channel is FIFO.
    while let Some(msg) = rx.recv().await {
        let (height, base_kv, ops) = msg?;
        if let Some(b) = base_kv {
            for k in splitter.ingest_base_kv(height, b) {
                key_origins.insert(k, KeyOrigin::Base { height: start_height });
            }
        }
        if let Some(o) = ops {
            splitter.apply_ops(o);
        }
        pb.inc(1);
    }
    producer.await??;

    let (init_db, boundary_ops) = splitter.into_parts();
    pb.finish_with_message(format!(
        "Base state ready ({} keys, {} boundary_ops)",
        init_db.len(),
        boundary_ops.values().map(|v| v.len()).sum::<usize>()
    ));
    Ok((init_db, boundary_ops, key_origins))
}

async fn run_validated_replay(
    start_height: u64,
    end_height: u64,
    source: &StoragePath,
    s3_client: Option<&aws_sdk_s3::Client>,
    db: &mut HashMap<Vec<u8>, Vec<u8>>,
    boundary_ops: &HashMap<u64, Vec<Operation>>,
    key_origins: &mut HashMap<Vec<u8>, KeyOrigin>,
    verbose: bool,
    max_per_block: usize,
) -> anyhow::Result<(OpCounts, OutlierCounts, Vec<String>)> {
    let num_blocks = end_height - start_height + 1;
    let pb = ProgressBar::new(num_blocks);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} blocks ({per_sec}, ETA {eta}) {msg}",
        )
        .unwrap()
        .progress_chars("=>-"),
    );
    pb.set_message("Phase 2: Validating replay");

    let mut total_ops = OpCounts::default();
    let mut total_outliers = OutlierCounts::default();
    let mut blocks_with_outliers: u64 = 0;
    let mut outlier_details: Vec<String> = Vec::new();

    type Msg = Result<(u64, Option<Vec<Operation>>), anyhow::Error>;
    let (tx, mut rx) = mpsc::channel::<Msg>(PREFETCH_BUFFER_BLOCKS);
    let source = source.clone();
    let client = s3_client.cloned();
    let producer = tokio::spawn(async move {
        run_ops_producer(start_height, end_height, source, client, tx).await
    });

    // Process in block order: producer sends ascending height, channel is FIFO.
    while let Some(msg) = rx.recv().await {
        let (height, ops) = msg?;
        if let Some(ops_boundary) = boundary_ops.get(&height) {
            for op in ops_boundary {
                match op {
                    Operation::Create(k, v) | Operation::Update(k, v) => {
                        db.insert(k.clone(), v.clone());
                    }
                    Operation::Delete(k) => {
                        db.remove(k);
                    }
                    _ => {}
                }
            }
        }

        let Some(ops) = ops else {
            pb.inc(1);
            continue;
        };

        let mut block_ops = OpCounts::default();
        let mut block_outliers = OutlierCounts::default();
        let mut verbose_create = 0usize;
        let mut verbose_update = 0usize;
        let mut verbose_delete = 0usize;
        let mut verbose_read = 0usize;

        for (idx, op) in ops.iter().enumerate() {
            match op {
                Operation::Read(k, v) => {
                    block_ops.read += 1;
                    let mismatch = if v.is_empty() {
                        db.contains_key(k)
                    } else {
                        db.get(k) != Some(v)
                    };
                    if mismatch {
                        block_outliers.read_mismatch += 1;
                        if verbose && verbose_read < max_per_block {
                            let origin_str = match key_origins.get(k) {
                                Some(KeyOrigin::Base { height: h }) => {
                                    format!("originally from base at height {}", h)
                                }
                                Some(KeyOrigin::Changeset { height: h, idx: i, op }) => {
                                    format!("previously {} at height {} idx={}", op, h, i)
                                }
                                None => "origin unknown".to_string(),
                            };
                            let msg = format!(
                                "  [outlier] height={} idx={} READ mismatch key {} ({})",
                                height, idx, hex_key(k), origin_str
                            );
                            pb.println(&msg);
                            outlier_details.push(msg);
                            verbose_read += 1;
                        }
                    }
                }
                Operation::Create(k, v) => {
                    block_ops.create += 1;
                    if db.contains_key(k) {
                        block_outliers.create_exists += 1;
                        if verbose && verbose_create < max_per_block {
                            let origin_str = match key_origins.get(k) {
                                Some(KeyOrigin::Base { height: h }) => {
                                    format!("originally from base at height {}", h)
                                }
                                Some(KeyOrigin::Changeset { height: h, idx: i, op }) => {
                                    format!("previously {} at height {} idx={}", op, h, i)
                                }
                                None => "origin unknown".to_string(),
                            };
                            let msg = format!(
                                "  [outlier] height={} idx={} CREATE on existing key {} ({})",
                                height, idx, hex_key(k), origin_str
                            );
                            pb.println(&msg);
                            outlier_details.push(msg);
                            verbose_create += 1;
                        }
                    }
                    key_origins.insert(k.clone(), KeyOrigin::Changeset { height, idx, op: "CREATE" });
                    db.insert(k.clone(), v.clone());
                }
                Operation::Update(k, v) => {
                    block_ops.update += 1;
                    if !db.contains_key(k) {
                        block_outliers.update_missing += 1;
                        if verbose && verbose_update < max_per_block {
                            let last_str = match key_origins.get(k) {
                                Some(KeyOrigin::Changeset { height: h, idx: i, op }) => {
                                    format!("last touched by {} at height {} idx={}", op, h, i)
                                }
                                Some(KeyOrigin::Base { height: h }) => {
                                    format!("originally from base at height {}", h)
                                }
                                None => "never seen before".to_string(),
                            };
                            let msg = format!(
                                "  [outlier] height={} idx={} UPDATE on missing key {} ({})",
                                height, idx, hex_key(k), last_str
                            );
                            pb.println(&msg);
                            outlier_details.push(msg);
                            verbose_update += 1;
                        }
                    }
                    key_origins.insert(k.clone(), KeyOrigin::Changeset { height, idx, op: "UPDATE" });
                    db.insert(k.clone(), v.clone());
                }
                Operation::Delete(k) => {
                    block_ops.delete += 1;
                    if !db.contains_key(k) {
                        block_outliers.delete_missing += 1;
                        if verbose && verbose_delete < max_per_block {
                            let last_str = match key_origins.get(k) {
                                Some(KeyOrigin::Changeset { height: h, idx: i, op }) => {
                                    format!("last touched by {} at height {} idx={}", op, h, i)
                                }
                                Some(KeyOrigin::Base { height: h }) => {
                                    format!("originally from base at height {}", h)
                                }
                                None => "never seen before".to_string(),
                            };
                            let msg = format!(
                                "  [outlier] height={} idx={} DELETE on missing key {} ({})",
                                height, idx, hex_key(k), last_str
                            );
                            pb.println(&msg);
                            outlier_details.push(msg);
                            verbose_delete += 1;
                        }
                    }
                    key_origins.insert(k.clone(), KeyOrigin::Changeset { height, idx, op: "DELETE" });
                    db.remove(k);
                }
            }
        }

        if block_outliers.total() > 0 {
            blocks_with_outliers += 1;
            pb.println(format!(
                "Height {}: {} ops, {} outliers (create_exists={} read_mismatch={} update_missing={} delete_missing={})",
                height,
                block_ops.total(),
                block_outliers.total(),
                block_outliers.create_exists,
                block_outliers.read_mismatch,
                block_outliers.update_missing,
                block_outliers.delete_missing,
            ));
        }

        total_ops.add(&block_ops);
        total_outliers.add(&block_outliers);
        pb.set_message(format!(
            "Phase 2: Validating replay | outliers: {} in {} blocks",
            total_outliers.total(),
            blocks_with_outliers,
        ));
        pb.inc(1);
    }
    producer.await??;

    pb.finish_with_message(format!(
        "Replay done. {} outliers in {} blocks",
        total_outliers.total(),
        blocks_with_outliers,
    ));

    Ok((total_ops, total_outliers, outlier_details))
}
