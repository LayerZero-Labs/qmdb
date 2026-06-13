use clap::Parser;
use eth_replay_kv::{
    new_s3_client, parse_storage_path, run_block_data_producer, Operation,
    PREFETCH_BUFFER_BLOCKS,
};
use indicatif::{ProgressBar, ProgressStyle};
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(
    name = "kv-key-trace",
    about = "Traces all operations on a specific key across a block range"
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

    /// Hex-encoded key to trace.
    #[arg(required = true)]
    key_hex: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let source = parse_storage_path(&args.input);
    let target_key = hex::decode(&args.key_hex)?;

    let s3_client = if source.is_s3() {
        Some(new_s3_client().await)
    } else {
        None
    };

    println!(
        "Tracing key {} across blocks {}..={}",
        args.key_hex, args.start_height, args.end_height
    );
    println!();

    let num_blocks = args.end_height - args.start_height + 1;
    let pb = ProgressBar::new(num_blocks);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} blocks ({per_sec}, ETA {eta})",
        )
        .unwrap()
        .progress_chars("=>-"),
    );

    let mut found_in_base = false;
    let mut events: Vec<String> = Vec::new();

    type Msg = Result<(u64, Option<Vec<(Vec<u8>, Vec<u8>)>>, Option<Vec<Operation>>), anyhow::Error>;
    let (tx, mut rx) = mpsc::channel::<Msg>(PREFETCH_BUFFER_BLOCKS);
    let source = source.clone();
    let client = s3_client.clone();
    let producer = tokio::spawn(async move {
        run_block_data_producer(
            args.start_height,
            args.end_height,
            source,
            client,
            tx,
        )
        .await
    });

    // Process in block order: producer sends ascending height, channel is FIFO.
    while let Some(msg) = rx.recv().await {
        let (height, block_base_kv, ops) = msg?;
        if let Some(block_base_kv) = block_base_kv {
            for (i, (k, v)) in block_base_kv.iter().enumerate() {
                if k == &target_key {
                    events.push(format!(
                        "  height={} BASE_KV idx={} value_len={} value_empty={}",
                        height, i, v.len(), v.is_empty()
                    ));
                    found_in_base = true;
                }
            }
        }
        if let Some(ops) = ops {
            for (idx, op) in ops.iter().enumerate() {
                match op {
                    Operation::Read(k, v) if k == &target_key => {
                        events.push(format!(
                            "  height={} OPERATION idx={} READ value_len={} value_empty={}",
                            height, idx, v.len(), v.is_empty()
                        ));
                    }
                    Operation::Create(k, v) if k == &target_key => {
                        events.push(format!(
                            "  height={} OPERATION idx={} CREATE value_len={}",
                            height, idx, v.len()
                        ));
                    }
                    Operation::Update(k, v) if k == &target_key => {
                        events.push(format!(
                            "  height={} OPERATION idx={} UPDATE value_len={}",
                            height, idx, v.len()
                        ));
                    }
                    Operation::Delete(k) if k == &target_key => {
                        events.push(format!("  height={} OPERATION idx={} DELETE", height, idx));
                    }
                    _ => {}
                }
            }
        }
        pb.inc(1);
    }
    producer.await??;

    pb.finish_and_clear();

    println!("\n========== Trace for key {} ==========", args.key_hex);
    if events.is_empty() {
        println!("  (no events found)");
    } else {
        for e in &events {
            println!("{e}");
        }
    }
    if !found_in_base {
        println!("  (key was NOT found in any base_kv file)");
    }
    println!("==========================================");

    Ok(())
}
