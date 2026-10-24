use clap::{Args, Parser};

#[derive(Parser, Debug, Clone)]
pub struct Cli {
    /// Number of entries to populate database with
    #[arg(short, long, default_value_t = 500_000_000)]
    pub entry_count: u64,

    /// Directory to store the database's persistent files (e.g., /mnt/nvme/QMDB)
    #[arg(long, default_value = "/tmp/QMDB")]
    pub db_dir: String,

    /// Source of randomness for workload generation  
    #[arg(short, long, default_value = "./randsrc.dat")]
    pub randsrc_filename: String,

    /// Block device name (as in /proc/diskstats) for OS physical R/W counters.
    /// Empty = auto-derive from --db-dir's mount via findmnt (recommended).
    #[arg(long, default_value = "")]
    pub monitor_disk: String,

    /// Number of changesets per task/transaction
    #[arg(long, default_value_t = 2, alias = "changesets-per-txn")]
    pub changesets_per_task: u64,

    /// Target number of operations per block. Will be rounded to nearest achievable number based on changesets-per-task and used to calculate transactions (tasks) per block.
    #[arg(long, default_value_t = 100_000)]
    pub ops_per_block: u64,

    #[command(flatten)]
    pub hover: HoverWorkloadArgs,

    /// Benchmark threads
    #[arg(long, default_value_t = 64)]
    pub rayon_threads: usize,

    // ETH real workload path
    #[arg(long)]
    pub eth_trace_dir: Option<String>,

    /// Path to a pre-computed InitDbBundle (bincode) produced by `gen-init-db`.
    /// When set, Phase 1 loads init_db + boundary_ops from this file instead of
    /// regenerating them from traces via BaseStateSplitter. --eth-trace-dir is
    /// still required (Phase 3 replay reads its ops from the traces).
    #[arg(long)]
    pub init_db_path: Option<String>,

    /// Ethereum Trace Replay start block id
    #[arg(long, default_value_t = 15012300)]
    pub eth_start_height: u64,

    /// Ethereum Trace Replay end block id (used for Phase 1 populate)
    #[arg(long, default_value_t = 15490216)]
    pub eth_end_height: u64,

    /// Ethereum REPLAY start block id (Phase 3); 0 = fallback to eth_start_height
    #[arg(long, default_value_t = 0)]
    pub eth_replay_start_height: u64,

    /// Ethereum REPLAY end block id (Phase 3); 0 = fallback to eth_end_height
    #[arg(long, default_value_t = 0)]
    pub eth_replay_end_height: u64,
}

#[derive(Args, Debug, Clone)]
pub struct HoverWorkloadArgs {
    // TODO: Make this a function of entries rather than by block
    /// Interval between hover tasks (dev=10, test=1000)
    #[arg(long, default_value_t = if cfg!(debug_assertions) { 10 } else { 10000 })]
    pub hover_interval: u64,

    /// Number of blocks? to insert in hover tasks (dev=2, test=50)
    #[arg(long, default_value_t = if cfg!(debug_assertions) { 2 } else { 50 })]
    pub hover_recreate_block: u64,

    /// Number of blocks? to write in hover tasks (dev=2, test=50)
    #[arg(long, default_value_t = if cfg!(debug_assertions) { 2 } else { 50 })]
    pub hover_write_block: u64,

    /// Zipf exponent for read/write hover workload key selection.
    /// 0.0 = uniform random (default), 0.99 = YCSB-style skew, >1.0 = extreme skew.
    /// Must not be exactly 1.0; use 0.99 or 1.01.
    #[arg(long, default_value_t = 0.0)]
    pub hover_zipf_theta: f64,
}
