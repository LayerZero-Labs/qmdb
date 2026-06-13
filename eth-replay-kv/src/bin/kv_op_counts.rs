use clap::Parser;
use eth_replay_kv::{
    new_s3_client, parse_storage_path, run_ops_producer, Operation,
    PREFETCH_BUFFER_BLOCKS,
};
use indicatif::{ProgressBar, ProgressStyle};
use tokio::sync::mpsc;


#[derive(Parser, Debug)]
#[command(
    name = "kv-op-counts",
    about = "Counts Read/Create/Update/Delete operations in operation_kv files",
    long_about = "Reads `{height}_operation_kv.bin` bincode files and counts operations for a single block height or an inclusive height range."
)]
struct Args {
    /// First block height (inclusive).
    start_height: u64,

    /// Last block height (inclusive). Use the same value as `start_height` to count a single block.
    end_height: u64,

    /// Directory containing `{height}_operation_kv.bin` files.
    input: String,

    /// Print a line per height (in addition to the range total).
    #[arg(long)]
    per_block: bool,
}

#[derive(Default, Debug, Clone, Copy)]
struct Counts {
    create: u64,
    read: u64,
    update: u64,
    delete: u64,
}

impl Counts {
    fn total(&self) -> u64 {
        self.create + self.read + self.update + self.delete
    }

    fn pct_str(n: u64, total: u64) -> String {
        if total == 0 {
            return "0.00%".to_string();
        }
        format!("{:.2}%", (n as f64) * 100.0 / (total as f64))
    }

    fn add(&mut self, other: Counts) {
        self.create += other.create;
        self.read += other.read;
        self.update += other.update;
        self.delete += other.delete;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let end_height = args.end_height;
    let source = parse_storage_path(&args.input);

    let s3_client = if source.is_s3() {
        Some(new_s3_client().await)
    } else {
        None
    };

    let mut total = Counts::default();

    type Msg = Result<(u64, Option<Vec<Operation>>), anyhow::Error>;
    let (tx, mut rx) = mpsc::channel::<Msg>(PREFETCH_BUFFER_BLOCKS);
    let source = source.clone();
    let client = s3_client.clone();
    let producer = tokio::spawn(async move {
        run_ops_producer(args.start_height, end_height, source, client, tx).await
    });

    let num_blocks = end_height - args.start_height + 1;
    let pb = ProgressBar::new(num_blocks);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} blocks ({per_sec}, ETA {eta})"
        )
        .unwrap()
        .progress_chars("=>-"),
    );

    // Process in block order: producer sends ascending height, channel is FIFO.
    while let Some(msg) = rx.recv().await {
        let (height, ops) = msg?;
        let Some(ops) = ops else {
            if args.per_block {
                pb.println(format!(
                    "Height {}: (missing) create=0 read=0 update=0 delete=0 total=0",
                    height
                ));
            }
            pb.inc(1);
            continue;
        };

        let mut c = Counts::default();
        for op in &ops {
            match op {
                Operation::Create(_, _) => c.create += 1,
                Operation::Read(_, _) => c.read += 1,
                Operation::Update(_, _) => c.update += 1,
                Operation::Delete(_) => c.delete += 1,
            }
        }

        if args.per_block {
            let t = c.total();
            pb.println(format!(
                "Height {}: create={} ({}) read={} ({}) update={} ({}) delete={} ({}) total={}",
                height,
                c.create,
                Counts::pct_str(c.create, t),
                c.read,
                Counts::pct_str(c.read, t),
                c.update,
                Counts::pct_str(c.update, t),
                c.delete,
                Counts::pct_str(c.delete, t),
                t
            ));
        }
        total.add(c);
        pb.inc(1);
    }
    producer.await??;

    pb.finish_with_message("done");

    let t = total.total();
    let label = if args.start_height == end_height {
        format!("Total (height {})", args.start_height)
    } else {
        format!("Total (range {}..={})", args.start_height, end_height)
    };
    println!(
        "{}: create={} ({}) read={} ({}) update={} ({}) delete={} ({}) total={}",
        label,
        total.create,
        Counts::pct_str(total.create, t),
        total.read,
        Counts::pct_str(total.read, t),
        total.update,
        Counts::pct_str(total.update, t),
        total.delete,
        Counts::pct_str(total.delete, t),
        t
    );

    Ok(())
}
