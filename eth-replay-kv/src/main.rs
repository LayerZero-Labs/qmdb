use clap::Parser;
use eth_replay_kv::{
    new_s3_client, parse_storage_path, run_block_data_producer, run_ops_producer,
    BaseStateSplitter, Operation, PREFETCH_BUFFER_BLOCKS, StoragePath,
};
use std::collections::HashMap;
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(
    name = "eth-replay-kv",
    about = "Replays per-block KV operations over a base state snapshot range"
)]
struct Args {
    #[arg(required = true)]
    start_height: u64,
    #[arg(required = true)]
    end_height: u64,
    #[arg(required = true)]
    input: String,
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

    let (mut db, boundary_ops) =
        populate_base_state(args.start_height, args.end_height, &source, s3_client.as_ref())
            .await?;
    run_replay(
        args.start_height,
        args.end_height,
        &source,
        s3_client.as_ref(),
        &mut db,
        &boundary_ops,
    )
    .await?;

    Ok(())
}

async fn populate_base_state(
    start_height: u64,
    end_height: u64,
    source: &StoragePath,
    s3_client: Option<&aws_sdk_s3::Client>,
) -> anyhow::Result<(HashMap<Vec<u8>, Vec<u8>>, HashMap<u64, Vec<Operation>>)> {
    type Msg = Result<(u64, Option<Vec<(Vec<u8>, Vec<u8>)>>, Option<Vec<Operation>>), anyhow::Error>;
    let (tx, mut rx) = mpsc::channel::<Msg>(PREFETCH_BUFFER_BLOCKS);
    let source = source.clone();
    let client = s3_client.cloned();
    let producer = tokio::spawn(async move {
        run_block_data_producer(start_height, end_height, source, client, tx).await
    });

    let mut splitter = BaseStateSplitter::new();
    println!("Phase 1: Populating Base State (prefetch pipeline)...");
    // Process in block order: producer sends ascending height, channel is FIFO.
    while let Some(msg) = rx.recv().await {
        let (height, base_kv, ops) = msg?;
        if let Some(b) = base_kv {
            splitter.ingest_base_kv(height, b);
        }
        if let Some(o) = ops {
            splitter.apply_ops(o);
        }
        if height % 100 == 0 || height == end_height {
            println!(
                "Height {}: init_db: {}, boundary_ops: {}",
                height,
                splitter.init_db_len(),
                splitter.boundary_ops_count()
            );
        }
    }
    producer.await??;

    let (init_db, boundary_ops) = splitter.into_parts();
    println!(
        "Base State Population Complete. init_db size: {}, boundary_ops: {} across {} blocks",
        init_db.len(),
        boundary_ops.values().map(|v| v.len()).sum::<usize>(),
        boundary_ops.len()
    );
    Ok((init_db, boundary_ops))
}

async fn run_replay(
    start_height: u64,
    end_height: u64,
    source: &StoragePath,
    s3_client: Option<&aws_sdk_s3::Client>,
    db: &mut HashMap<Vec<u8>, Vec<u8>>,
    boundary_ops: &HashMap<u64, Vec<Operation>>,
) -> anyhow::Result<()> {
    type Msg = Result<(u64, Option<Vec<Operation>>), anyhow::Error>;
    let (tx, mut rx) = mpsc::channel::<Msg>(PREFETCH_BUFFER_BLOCKS);
    let source = source.clone();
    let client = s3_client.cloned();
    let producer = tokio::spawn(async move {
        run_ops_producer(start_height, end_height, source, client, tx).await
    });

    println!("\nPhase 2: Simulating Replay (prefetch pipeline)...");
    let mut total_ops = 0;
    let mut total_reads = 0;
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
            println!("Skipping height {} (operation_kv not found)", height);
            continue;
        };
        let op_count = ops.len();
        total_ops += op_count;

        let mut read_count = 0;
        for (op_idx, op) in ops.into_iter().enumerate() {
            match op {
                Operation::Read(k, v) => {
                    read_count += 1;
                    if v.is_empty() {
                        if db.contains_key(&k) {
                            eprintln!(
                                "Warning: Key exists in DB but Read expects miss at height {} op_idx={} key={} (ignoring)",
                                height, op_idx, hex::encode(&k)
                            );
                        }
                    } else {
                        let db_val = db.get(&k);
                        if db_val != Some(&v) {
                            eprintln!(
                                "Warning: Read value mismatch at height {} op_idx={} key={} (expected val len={}, got {:?}) (ignoring)",
                                height, op_idx, hex::encode(&k), v.len(), db_val.map(|v| v.len())
                            );
                        }
                    }
                }
                Operation::Create(k, v) => {
                    assert!(
                        !db.contains_key(&k),
                        "Key already exists during Create at height {} op_idx={} key={}",
                        height,
                        op_idx,
                        hex::encode(&k)
                    );
                    db.insert(k, v);
                }
                Operation::Update(k, v) => {
                    assert!(
                        db.contains_key(&k),
                        "Key does not exist during Update at height {} op_idx={} key={}",
                        height,
                        op_idx,
                        hex::encode(&k)
                    );
                    db.insert(k, v);
                }
                Operation::Delete(k) => {
                    assert!(
                        db.contains_key(&k),
                        "Key does not exist during Delete at height {} op_idx={} key={}",
                        height,
                        op_idx,
                        hex::encode(&k)
                    );
                    db.remove(&k);
                }
            }
        }
        total_reads += read_count;

        if height % 100 == 0 || height == end_height {
            println!(
                "Height {}: Applied {} operations ({} reads). DB size: {}",
                height, op_count, read_count, db.len()
            );
        }
    }
    producer.await??;
    println!(
        "Replay Complete. Total operations: {} ({} reads). Final DB size: {}",
        total_ops, total_reads, db.len()
    );
    Ok(())
}
