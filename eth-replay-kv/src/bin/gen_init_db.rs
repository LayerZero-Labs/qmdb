// gen_init_db.rs — merged: generate init_db bincode + workload analysis in ONE pass
//
// This binary:
//   1. Streams base_kv + ops blocks from the kv_decompressed dir via
//      run_block_data_producer (same iterator as kv_analyze does in its first phase).
//   2. Feeds them to BaseStateSplitter for init_db / boundary_ops generation.
//   3. ALSO accumulates access-distribution statistics (Zipfian, sharding, locality,
//      size histogram) on the same ops — no second pass.
//   4. Writes:
//        --out-init-db <path>     bincode-serialized InitDbBundle (use on AWS to skip Phase 1)
//        --out-analysis <path>    text analysis log (subset of kv_analyze.rs format)
//
// Memory: ~80-100 GB peak on 5.4M block range (h0 has 1.5 TB so safe).

use clap::Parser;
use eth_replay_kv::{
    new_s3_client, parse_storage_path, run_block_data_producer,
    BaseStateSplitter, InitDbBundle, Operation, PREFETCH_BUFFER_BLOCKS, StoragePath,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::Instant;
use tokio::sync::mpsc;

const SHARD_COUNT: usize = 16;

#[derive(Parser, Debug)]
#[command(
    name = "gen-init-db",
    about = "Generate init_db bincode file + workload analysis log in a single pass"
)]
struct Args {
    #[arg(long)]
    start_height: u64,
    #[arg(long)]
    end_height: u64,
    #[arg(long)]
    input: String,
    /// bincode output path for InitDbBundle (consumed by speed.rs on AWS)
    #[arg(long)]
    out_init_db: String,
    /// text output path for workload analysis log
    #[arg(long)]
    out_analysis: String,
}

fn blake3_hash(key: &[u8]) -> [u8; 32] {
    *blake3::hash(key).as_bytes()
}

fn byte0_to_shard_id(byte0: u8) -> usize {
    (byte0 as usize) * SHARD_COUNT / 256
}

struct AccessStats {
    key_frequency: HashMap<Vec<u8>, u32>,
    total_ops: u64,
    count_read: u64,
    count_create: u64,
    count_update: u64,
    count_delete: u64,
    shard_total_ops: Vec<u64>,
    last_seen_block: HashMap<Vec<u8>, u64>,
    intra_block_reuses: u64,
    distance_histogram: HashMap<u64, u64>,
    total_reuses: u64,
    key_size_hist: HashMap<usize, u64>,
    val_size_hist: HashMap<usize, u64>,
}

impl AccessStats {
    fn new() -> Self {
        Self {
            key_frequency: HashMap::new(),
            total_ops: 0,
            count_read: 0,
            count_create: 0,
            count_update: 0,
            count_delete: 0,
            shard_total_ops: vec![0; SHARD_COUNT],
            last_seen_block: HashMap::new(),
            intra_block_reuses: 0,
            distance_histogram: HashMap::new(),
            total_reuses: 0,
            key_size_hist: HashMap::new(),
            val_size_hist: HashMap::new(),
        }
    }

    fn update(&mut self, height: u64, ops: &[Operation]) {
        self.total_ops += ops.len() as u64;
        for op in ops {
            let (key, opt_val) = match op {
                Operation::Read(k, _) => { self.count_read += 1; (k, None) }
                Operation::Create(k, v) => { self.count_create += 1; (k, Some(v)) }
                Operation::Update(k, v) => { self.count_update += 1; (k, Some(v)) }
                Operation::Delete(k) => { self.count_delete += 1; (k, None) }
            };

            *self.key_frequency.entry(key.clone()).or_insert(0) += 1;
            if !key.is_empty() {
                let kh = blake3_hash(key);
                let shard_id = byte0_to_shard_id(kh[0]);
                if shard_id < SHARD_COUNT {
                    self.shard_total_ops[shard_id] += 1;
                }
            }
            if let Some(&prev) = self.last_seen_block.get(key) {
                self.total_reuses += 1;
                let dist = height - prev;
                if dist == 0 {
                    self.intra_block_reuses += 1;
                } else {
                    *self.distance_histogram.entry(dist).or_insert(0) += 1;
                }
            }
            self.last_seen_block.insert(key.clone(), height);
            *self.key_size_hist.entry(key.len()).or_insert(0) += 1;
            if let Some(v) = opt_val {
                *self.val_size_hist.entry(v.len()).or_insert(0) += 1;
            }
        }
    }

    fn write_analysis<W: Write>(&self, w: &mut W) -> std::io::Result<()> {
        let total_ops = self.total_ops;
        let total_writes = self.count_create + self.count_update + self.count_delete;
        writeln!(w, "\n--- Operation Type Distribution ---")?;
        writeln!(w, "Total Operations: {}", total_ops)?;
        if total_ops > 0 {
            writeln!(w, "  - Read:   {} ({:.2}%)", self.count_read, self.count_read as f64 / total_ops as f64 * 100.0)?;
            writeln!(w, "  - Create: {} ({:.2}%)", self.count_create, self.count_create as f64 / total_ops as f64 * 100.0)?;
            writeln!(w, "  - Update: {} ({:.2}%)", self.count_update, self.count_update as f64 / total_ops as f64 * 100.0)?;
            writeln!(w, "  - Delete: {} ({:.2}%)", self.count_delete, self.count_delete as f64 / total_ops as f64 * 100.0)?;
        }
        let rw_ratio = if total_writes > 0 { self.count_read as f64 / total_writes as f64 } else { self.count_read as f64 };
        writeln!(w, "  - R/W Ratio: {:.2} : 1", rw_ratio)?;

        writeln!(w, "\n--- Temporal Locality (Block Reuse Distance) ---")?;
        if total_ops > 0 {
            writeln!(w, "Total Reused Ops: {} ({:.2}% of all ops)",
                self.total_reuses, self.total_reuses as f64 / total_ops as f64 * 100.0)?;
        }
        if self.total_reuses > 0 {
            writeln!(w, "  - Intra-block (dist=0): {} ({:.2}% of reuses)",
                self.intra_block_reuses, self.intra_block_reuses as f64 / self.total_reuses as f64 * 100.0)?;
            let mut d1 = 0u64; let mut d10 = 0u64; let mut d100 = 0u64; let mut d1k = 0u64; let mut dmore = 0u64;
            for (&dist, &count) in &self.distance_histogram {
                if dist == 1 { d1 += count; }
                else if dist <= 10 { d10 += count; }
                else if dist <= 100 { d100 += count; }
                else if dist <= 1000 { d1k += count; }
                else { dmore += count; }
            }
            writeln!(w, "  - Dist = 1:     {} ({:.2}%)", d1, d1 as f64 / self.total_reuses as f64 * 100.0)?;
            writeln!(w, "  - Dist <= 10:   {} ({:.2}%)", d10, d10 as f64 / self.total_reuses as f64 * 100.0)?;
            writeln!(w, "  - Dist <= 100:  {} ({:.2}%)", d100, d100 as f64 / self.total_reuses as f64 * 100.0)?;
            writeln!(w, "  - Dist <= 1000: {} ({:.2}%)", d1k, d1k as f64 / self.total_reuses as f64 * 100.0)?;
            writeln!(w, "  - Dist > 1000:  {} ({:.2}%)", dmore, dmore as f64 / self.total_reuses as f64 * 100.0)?;
        }

        writeln!(w, "\n--- Key Size Distribution (Top 10) ---")?;
        let mut k_sizes: Vec<(&usize, &u64)> = self.key_size_hist.iter().collect();
        k_sizes.sort_by(|a, b| b.1.cmp(a.1));
        for (size, count) in k_sizes.iter().take(10) {
            writeln!(w, "  - {:<4} bytes: {:<10} ops ({:.2}%)", size, count,
                **count as f64 / total_ops as f64 * 100.0)?;
        }

        writeln!(w, "\n--- Value Size Distribution (Top 10, writes only) ---")?;
        if total_writes > 0 {
            let mut v_sizes: Vec<(&usize, &u64)> = self.val_size_hist.iter().collect();
            v_sizes.sort_by(|a, b| b.1.cmp(a.1));
            for (size, count) in v_sizes.iter().take(10) {
                writeln!(w, "  - {:<4} bytes: {:<10} writes ({:.2}%)", size, count,
                    **count as f64 / total_writes as f64 * 100.0)?;
            }
        }

        // Zipfian
        let mut sorted: Vec<(&Vec<u8>, &u32)> = self.key_frequency.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(a.1));
        let counts: Vec<u32> = sorted.iter().map(|(_, c)| **c).collect();
        let total_unique_keys = counts.len();
        writeln!(w, "\nTotal Unique Keys Touched: {}", total_unique_keys)?;

        writeln!(w, "\n--- Zipfian Distribution Analysis ---")?;
        let s = estimate_zipf_parameter(&counts);
        writeln!(w, "Estimated Zipfian Skewness (s): {:.4}", s)?;
        if s < 0.5 { writeln!(w, "  -> Distribution is relatively UNIFORM")?; }
        else if s < 1.0 { writeln!(w, "  -> Distribution is MODERATELY skewed")?; }
        else { writeln!(w, "  -> Distribution is HIGHLY skewed")?; }

        writeln!(w, "\n--- CDF: top X% keys account for Y% of traffic ---")?;
        let milestones = [0.5, 0.8, 0.9, 0.95, 0.99];
        let mut acc = 0u64;
        let mut cnt = 0usize;
        let mut m_idx = 0usize;
        let f_total = total_ops as f64;
        for &c in &counts {
            acc += c as u64;
            cnt += 1;
            let pct = acc as f64 / f_total;
            while m_idx < milestones.len() && pct >= milestones[m_idx] {
                let pct_keys = cnt as f64 / total_unique_keys.max(1) as f64 * 100.0;
                writeln!(w, "Top {:6.3}% keys = {:.0}% of traffic",
                    pct_keys, milestones[m_idx] * 100.0)?;
                m_idx += 1;
            }
        }

        // Shard distribution
        writeln!(w, "\n--- Shard Distribution (SHARD_COUNT={}, Blake3) ---", SHARD_COUNT)?;
        writeln!(w, "\n[A] Ops per Shard:")?;
        write_shard_stats(w, &self.shard_total_ops, total_ops, "Ops")?;

        let mut shard_unique = vec![0u64; SHARD_COUNT];
        for (key, _) in &self.key_frequency {
            let kh = blake3_hash(key);
            let sid = byte0_to_shard_id(kh[0]);
            if sid < SHARD_COUNT { shard_unique[sid] += 1; }
        }
        writeln!(w, "\n[B] Keys per Shard:")?;
        write_shard_stats(w, &shard_unique, total_unique_keys as u64, "Keys")?;

        let hot_thr = std::cmp::max(1, total_unique_keys / 100);
        let mut shard_hot_ops = vec![0u64; SHARD_COUNT];
        let mut shard_hot_keys = vec![0u64; SHARD_COUNT];
        let mut total_hot_ops = 0u64;
        for (key, count) in sorted.iter().take(hot_thr) {
            let kh = blake3_hash(key);
            let sid = byte0_to_shard_id(kh[0]);
            if sid < SHARD_COUNT {
                shard_hot_ops[sid] += **count as u64;
                shard_hot_keys[sid] += 1;
                total_hot_ops += **count as u64;
            }
        }
        writeln!(w, "\n[C] Hot (top 1%) Ops per Shard:")?;
        write_shard_stats(w, &shard_hot_ops, total_hot_ops, "Ops")?;
        writeln!(w, "\n[D] Hot (top 1%) Keys per Shard:")?;
        write_shard_stats(w, &shard_hot_keys, hot_thr as u64, "Keys")?;

        Ok(())
    }
}

fn write_shard_stats<W: Write>(w: &mut W, data: &[u64], total: u64, unit: &str) -> std::io::Result<()> {
    if total == 0 { writeln!(w, "  (no data)")?; return Ok(()); }
    let avg = total as f64 / SHARD_COUNT as f64;
    writeln!(w, "{:<5} | {:<12} | {:<8} | Skew", "Shard", format!("{}", unit), "% Total")?;
    for (i, &v) in data.iter().enumerate() {
        let pct = v as f64 / total as f64 * 100.0;
        let skew = if avg > 0.0 { v as f64 / avg } else { 0.0 };
        writeln!(w, "{:<5} | {:<12} | {:6.2}% | {:6.2}x", i, v, pct, skew)?;
    }
    Ok(())
}

// MLE estimate of Zipfian parameter using log-log fit (simplified).
fn estimate_zipf_parameter(sorted_counts: &[u32]) -> f64 {
    if sorted_counts.len() < 2 { return 0.0; }
    let n_keys = sorted_counts.len().min(10000); // sample top
    if n_keys < 2 { return 0.0; }
    let mut sum_x = 0.0_f64; let mut sum_y = 0.0_f64;
    let mut sum_xy = 0.0_f64; let mut sum_xx = 0.0_f64;
    let mut n = 0.0_f64;
    for i in 0..n_keys {
        let rank = (i + 1) as f64;
        let freq = sorted_counts[i] as f64;
        if freq <= 0.0 { continue; }
        let x = rank.ln();
        let y = freq.ln();
        sum_x += x; sum_y += y;
        sum_xy += x * y; sum_xx += x * x;
        n += 1.0;
    }
    if n < 2.0 { return 0.0; }
    let denom = n * sum_xx - sum_x * sum_x;
    if denom == 0.0 { return 0.0; }
    let slope = (n * sum_xy - sum_x * sum_y) / denom;
    -slope // s = -slope of log-rank vs log-freq
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let source = parse_storage_path(&args.input);
    let s3_client = if source.is_s3() { Some(new_s3_client().await) } else { None };

    let total_start = Instant::now();
    eprintln!("=== gen-init-db: start={} end={} ===", args.start_height, args.end_height);
    eprintln!("input: {}", args.input);
    eprintln!("out_init_db: {}", args.out_init_db);
    eprintln!("out_analysis: {}", args.out_analysis);

    type Msg = Result<(u64, Option<Vec<(Vec<u8>, Vec<u8>)>>, Option<Vec<Operation>>), anyhow::Error>;
    let (tx, mut rx) = mpsc::channel::<Msg>(PREFETCH_BUFFER_BLOCKS);
    let src = source.clone();
    let client = s3_client.clone();
    let start = args.start_height;
    let end = args.end_height;
    let producer = tokio::spawn(async move {
        run_block_data_producer(start, end, src, client, tx).await
    });

    let mut splitter = BaseStateSplitter::new();
    let mut stats = AccessStats::new();

    let mut progress_blocks: u64 = 0;
    let progress_start = Instant::now();
    while let Some(msg) = rx.recv().await {
        let (height, base_kv, ops) = msg?;
        if let Some(b) = base_kv {
            splitter.ingest_base_kv(height, b);
        }
        if let Some(ops_vec) = ops {
            // Accumulate stats by-ref first, then consume for splitter
            stats.update(height, &ops_vec);
            splitter.apply_ops(ops_vec);
        }
        progress_blocks += 1;
        if progress_blocks % 10000 == 0 {
            let dur = progress_start.elapsed().as_secs_f64();
            eprintln!("[progress] processed {} blocks at height={} ({:.0} blocks/s, elapsed {:.0}s)",
                progress_blocks, height, progress_blocks as f64 / dur, dur);
        }
    }
    producer.await??;
    let scan_dur = total_start.elapsed();
    eprintln!("[done] Scanned {} blocks in {:.0}s", progress_blocks, scan_dur.as_secs_f64());

    // Build InitDbBundle and write
    let (init_db_hashmap, boundary_ops) = splitter.into_parts();
    let init_db: Vec<(Vec<u8>, Vec<u8>)> = init_db_hashmap.into_iter().collect();
    eprintln!("[init_db] unique entries: {}", init_db.len());
    eprintln!("[boundary_ops] blocks with corrections: {}", boundary_ops.len());

    let bundle = InitDbBundle {
        populate_start: args.start_height,
        populate_end: args.end_height,
        init_db,
        boundary_ops,
    };
    let t_write = Instant::now();
    let f = File::create(&args.out_init_db)?;
    let mut bw = BufWriter::with_capacity(64 * 1024 * 1024, f);
    bincode::serialize_into(&mut bw, &bundle)
        .map_err(|e| anyhow::anyhow!("bincode serialize failed: {}", e))?;
    bw.flush()?;
    eprintln!("[init_db_file] wrote {} in {:.1}s", args.out_init_db, t_write.elapsed().as_secs_f64());

    // Write analysis log
    let f = File::create(&args.out_analysis)?;
    let mut w = BufWriter::new(f);
    writeln!(&mut w, "=== Workload Analysis ===")?;
    writeln!(&mut w, "Range: {} -> {} ({} blocks)", args.start_height, args.end_height, progress_blocks)?;
    writeln!(&mut w, "Scan duration: {:.0}s", scan_dur.as_secs_f64())?;
    writeln!(&mut w, "Init DB entries: {}", bundle.init_db.len())?;
    writeln!(&mut w, "Boundary blocks: {}", bundle.boundary_ops.len())?;
    stats.write_analysis(&mut w)?;
    w.flush()?;
    eprintln!("[analysis] wrote {}", args.out_analysis);

    eprintln!("=== ALL DONE in {:.0}s ===", total_start.elapsed().as_secs_f64());
    Ok(())
}
