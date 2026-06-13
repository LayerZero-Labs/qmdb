use clap::Parser;
use eth_replay_kv::{load_optional_bincode, new_s3_client, parse_storage_path, Operation};

#[derive(Parser, Debug)]
#[command(
    name = "kv-dump-ops",
    about = "Dump operation_kv.bin in human-readable form for manual inspection",
    long_about = "Reads `{height}_operation_kv.bin` for each block and prints each operation \
                  (Create/Read/Update/Delete) with key (hex) and value length. Use to manually \
                  check operations per block."
)]
struct Args {
    /// First block height (inclusive).
    #[arg(required = true)]
    start_height: u64,

    /// Last block height (inclusive).
    #[arg(required = true)]
    end_height: u64,

    /// Directory or S3 URI containing `{height}_operation_kv.bin`.
    #[arg(required = true)]
    input: String,

    /// Max operations to print per block (omit for no limit).
    #[arg(long)]
    max_ops_per_block: Option<usize>,
}

fn fmt_op(height: u64, idx: usize, op: &Operation) -> String {
    let key_hex = |k: &[u8]| hex::encode(k);
    match op {
        Operation::Create(k, v) => format!(
            "  [{}] {:6}  CREATE  key={}  value_len={}",
            height,
            idx,
            key_hex(k),
            v.len()
        ),
        Operation::Read(k, v) => {
            let miss = if v.is_empty() { " (miss)" } else { "" };
            format!(
                "  [{}] {:6}  READ    key={}  value_len={}{}",
                height,
                idx,
                key_hex(k),
                v.len(),
                miss
            )
        }
        Operation::Update(k, v) => format!(
            "  [{}] {:6}  UPDATE  key={}  value_len={}",
            height,
            idx,
            key_hex(k),
            v.len()
        ),
        Operation::Delete(k) => format!("  [{}] {:6}  DELETE  key={}", height, idx, key_hex(k)),
    }
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

    for height in args.start_height..=args.end_height {
        let ops_name = format!("{}_operation_kv.bin", height);
        let Some(ops) =
            load_optional_bincode::<Vec<Operation>>(&source, s3_client.as_ref(), &ops_name).await?
        else {
            println!("[{}] (no file)", height);
            continue;
        };

        println!("========== block {} ({} ops) ==========", height, ops.len());

        let limit = args.max_ops_per_block.unwrap_or(ops.len());
        for (idx, op) in ops.iter().take(limit).enumerate() {
            println!("{}", fmt_op(height, idx, op));
        }
        if ops.len() > limit {
            println!("  ... ({} more ops, use --max-ops-per-block to see more)", ops.len() - limit);
        }
        println!();
    }

    Ok(())
}
