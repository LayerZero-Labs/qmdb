use clap::Parser;
use eth_replay_kv::{
    new_s3_client, parse_storage_path, run_block_data_producer, run_ops_producer,
    BaseStateSplitter, Operation, PREFETCH_BUFFER_BLOCKS, StoragePath,
};
use std::collections::HashMap;
use tokio::sync::mpsc;

const SHARD_COUNT: usize = 16;

#[derive(Parser, Debug)]
#[command(
    name = "eth-kv-analyzer",
    about = "Analyzes KV Base State size and Access Distribution (Zipfian/Sharding)"
)]
struct Args {
    #[arg(required = true)]
    start_height: u64,
    #[arg(required = true)]
    end_height: u64,
    #[arg(required = true)]
    input: String,
}

fn calculate_hash(key: &[u8]) -> [u8; 32] {
    let hash = blake3::hash(key);
    *hash.as_bytes()
}

fn byte0_to_shard_id(byte0: u8) -> usize {
    (byte0 as usize) * SHARD_COUNT / 256
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

    println!("=== 1. Analyzing Base State Size (Snapshot Size) ===");
    analyze_base_state(
        args.start_height,
        args.end_height,
        &source,
        s3_client.as_ref(),
    )
    .await?;

    println!("\n=== 2. Analyzing Access Distribution (Hotspot/Pareto) ===");
    analyze_access_and_sharding(
        args.start_height,
        args.end_height,
        &source,
        s3_client.as_ref(),
    )
    .await?;

    Ok(())
}

async fn analyze_base_state(
    start_height: u64,
    end_height: u64,
    source: &StoragePath,
    s3_client: Option<&aws_sdk_s3::Client>,
) -> anyhow::Result<()> {
    type Msg = Result<(u64, Option<Vec<(Vec<u8>, Vec<u8>)>>, Option<Vec<Operation>>), anyhow::Error>;
    let (tx, mut rx) = mpsc::channel::<Msg>(PREFETCH_BUFFER_BLOCKS);
    let source = source.clone();
    let client = s3_client.cloned();
    
    let producer = tokio::spawn(async move {
        run_block_data_producer(start_height, end_height, source, client, tx).await
    });

    let mut splitter = BaseStateSplitter::new();
    println!("Loading blocks to extract pure Base State...");
    
    while let Some(msg) = rx.recv().await {
        let (height, base_kv, ops) = msg?;
        if let Some(b) = base_kv {
            splitter.ingest_base_kv(height, b);
        }
        if let Some(o) = ops {
            splitter.apply_ops(o);
        }
    }
    producer.await??;

    let (init_db, _) = splitter.into_parts();
    
    let unique_keys_count = init_db.len();
    let mut total_key_bytes = 0;
    let mut total_val_bytes = 0;

    for (k, v) in init_db.iter() {
        total_key_bytes += k.len();
        total_val_bytes += v.len();
    }

    println!("Total Unique Entries (Keys): {}", unique_keys_count);
    let total_bytes = total_key_bytes + total_val_bytes;
    println!("Total Raw Data Size: {:.2} MB", total_bytes as f64 / 1024.0 / 1024.0);
    println!("  - Keys: {:.2} MB", total_key_bytes as f64 / 1024.0 / 1024.0);
    println!("  - Values: {:.2} MB", total_val_bytes as f64 / 1024.0 / 1024.0);
    if unique_keys_count > 0 {
        println!("Avg Key Size: {} bytes", total_key_bytes / unique_keys_count);
        println!("Avg Value Size: {} bytes", total_val_bytes / unique_keys_count);
    }

    Ok(())
}

async fn analyze_access_and_sharding(
    start_height: u64,
    end_height: u64,
    source: &StoragePath,
    s3_client: Option<&aws_sdk_s3::Client>,
) -> anyhow::Result<()> {
    type Msg = Result<(u64, Option<Vec<Operation>>), anyhow::Error>;
    let (tx, mut rx) = mpsc::channel::<Msg>(PREFETCH_BUFFER_BLOCKS);
    let source = source.clone();
    let client = s3_client.cloned();

    let producer = tokio::spawn(async move {
        run_ops_producer(start_height, end_height, source, client, tx).await
    });

    let mut key_frequency: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut total_ops = 0;
    
    let mut count_read = 0;
    let mut count_create = 0;
    let mut count_update = 0;
    let mut count_delete = 0;

    let mut shard_total_ops = vec![0u64; SHARD_COUNT];

    println!("Streaming operations for traffic analysis...");

    while let Some(msg) = rx.recv().await {
        let (height, ops_opt) = msg?;
        
        if height % 100 == 0 {
            print!("\rProcessing Block {}...", height);
            use std::io::Write;
            std::io::stdout().flush().unwrap();
        }

        if let Some(ops) = ops_opt {
            total_ops += ops.len();

            for op in ops {
                let key = match op {
                    Operation::Read(k, _) => { count_read += 1; k }
                    Operation::Create(k, _) => { count_create += 1; k }
                    Operation::Update(k, _) => { count_update += 1; k }
                    Operation::Delete(k) => { count_delete += 1; k }
                };

                // 1. Update frequency
                *key_frequency.entry(key.clone()).or_insert(0) += 1;

                // 2. Update Shard Ops
                if !key.is_empty() {
                    let kh = calculate_hash(&key);
                    let shard_id = byte0_to_shard_id(kh[0]);
                    if shard_id < SHARD_COUNT {
                        shard_total_ops[shard_id] += 1;
                    }
                }
            }
        }
    }
    producer.await??;
    println!("\nAnalysis Loop Complete.");

    if total_ops == 0 {
        println!("No operations found.");
        return Ok(());
    }

    // Part A: Operation Type Stats
    println!("\n--- Operation Type Distribution ---");
    println!("Total Operations: {}", total_ops);
    println!("  - Read:   {} ({:.2}%)", count_read, (count_read as f64 / total_ops as f64) * 100.0);
    println!("  - Create: {} ({:.2}%)", count_create, (count_create as f64 / total_ops as f64) * 100.0);
    println!("  - Update: {} ({:.2}%)", count_update, (count_update as f64 / total_ops as f64) * 100.0);
    println!("  - Delete: {} ({:.2}%)", count_delete, (count_delete as f64 / total_ops as f64) * 100.0);
    
    let total_writes = count_create + count_update + count_delete;
    let rw_ratio = if total_writes > 0 { count_read as f64 / total_writes as f64 } else { count_read as f64 };
    println!("  - R/W Ratio: {:.2} : 1", rw_ratio);
    println!("-----------------------------------");

    let mut sorted_entries: Vec<(&Vec<u8>, &u32)> = key_frequency.iter().collect();
    sorted_entries.sort_by(|a, b| b.1.cmp(a.1)); // Descending

    let counts: Vec<u32> = sorted_entries.iter().map(|(_, c)| **c).collect();
    let total_unique_keys = counts.len();
    println!("Total Unique Keys Touched: {}", total_unique_keys);

    // Part B: Zipfian / Pareto Analysis
    println!("\n--- Zipfian Distribution Analysis ---");
    let s = estimate_zipf_parameter(&counts);
    println!("Estimated Zipfian Skewness (s): {:.4}", s);
    
    if s < 0.5 {
        println!("  -> Distribution is relatively UNIFORM (Low skew).");
    } else if s >= 0.5 && s < 1.0 {
        println!("  -> Distribution is MODERATELY skewed (Typical Web/DB).");
    } else {
        println!("  -> Distribution is HIGHLY skewed (Heavy tail, caching is very effective).");
    }

    println!("\n--- Cumulative Distribution Function (CDF) ---");
    let milestones = [0.50, 0.80, 0.90, 0.95, 0.99];
    let mut accumulated_ops = 0;
    let mut distinct_cnt = 0;
    let mut m_idx = 0;
    let f_total_ops = total_ops as f64;

    for &count in &counts {
        accumulated_ops += count as usize;
        distinct_cnt += 1;
        
        let pct_ops = accumulated_ops as f64 / f_total_ops;
        
        while m_idx < milestones.len() && pct_ops >= milestones[m_idx] {
            let pct_keys = (distinct_cnt as f64 / total_unique_keys as f64) * 100.0;
            println!("Top {:6.3}% keys account for {:.0}% of traffic", pct_keys, milestones[m_idx] * 100.0);
            m_idx += 1;
        }
    }

    // Part C: Shard Distribution Analysis
    println!("\n=== Shard Distribution Analysis (SHARD_COUNT = {}, Algo = Blake3) ===", SHARD_COUNT);
    
    println!("\n[A] Global Traffic Distribution (OPS per Shard)");
    print_shard_stats(&shard_total_ops, total_ops as u64, "Ops");

    let mut shard_unique_keys = vec![0u64; SHARD_COUNT];
    for (key, _) in &key_frequency {
        let kh = calculate_hash(key);
        let shard_id = byte0_to_shard_id(kh[0]);
        if shard_id < SHARD_COUNT {
            shard_unique_keys[shard_id] += 1;
        }
    }
    println!("\n[B] Global Key Distribution (KEYS per Shard)");
    print_shard_stats(&shard_unique_keys, total_unique_keys as u64, "Keys");

    let hot_threshold_count = std::cmp::max(1, total_unique_keys / 100); 
    
    let mut shard_hot_ops = vec![0u64; SHARD_COUNT];
    let mut shard_hot_unique_keys = vec![0u64; SHARD_COUNT];
    let mut total_hot_ops = 0;

    for (key, count) in sorted_entries.iter().take(hot_threshold_count) {
        let kh = calculate_hash(key);
        let shard_id = byte0_to_shard_id(kh[0]);
        
        if shard_id < SHARD_COUNT {
            shard_hot_ops[shard_id] += **count as u64;
            shard_hot_unique_keys[shard_id] += 1;
            total_hot_ops += **count as u64;
        }
    }
    println!("\n[C] Hotspot Traffic Distribution (Top 1% Keys: {} keys)", hot_threshold_count);
    print_shard_stats(&shard_hot_ops, total_hot_ops, "Ops");

    println!("\n[D] Hotspot Key Location Distribution (Top 1% Keys)");
    print_shard_stats(&shard_hot_unique_keys, hot_threshold_count as u64, "Keys");

    Ok(())
}

fn print_shard_stats(shard_data: &[u64], total_basis: u64, unit: &str) {
    if total_basis == 0 {
        println!("No data to analyze.");
        return;
    }

    let avg = total_basis as f64 / SHARD_COUNT as f64;
    let mut max_val = 0u64;
    let mut min_val = u64::MAX;
    let mut variance_sum = 0.0;

    println!("{:<5} | {:<12} | {:<8} | {:<10}", "Shard", format!("Count ({})", unit), "% Total", "Skew");
    println!("{:-<5}-|-{:-<12}-|-{:-<8}-|-{:-<10}", "", "", "", "");

    for (i, &val) in shard_data.iter().enumerate() {
        if val > max_val { max_val = val; }
        if val < min_val { min_val = val; }
        
        let pct = (val as f64 / total_basis as f64) * 100.0;
        let skew = if avg > 0.0 { val as f64 / avg } else { 0.0 };
        variance_sum += (val as f64 - avg).powi(2);

        if SHARD_COUNT <= 32 || i < 4 || i >= SHARD_COUNT - 4 {
             println!("{:<5} | {:<12} | {:6.2}% | {:6.2}x avg", i, val, pct, skew);
        } else if i == 4 {
            println!("... (hidden) ...");
        }
    }

    let std_dev = (variance_sum / SHARD_COUNT as f64).sqrt();
    let cv = if avg > 0.0 { std_dev / avg } else { 0.0 };

    println!("--------------------------------------------------");
    println!("Summary:");
    println!("  - Average:    {:.0} {}/shard", avg, unit);
    println!("  - Max Skew:   {:.2}x avg", if avg > 0.0 { max_val as f64 / avg } else { 0.0 });
    println!("  - CV (Bias):  {:.4} {}", cv, if cv < 0.2 { "(Balanced)" } else { "(Skewed)" });
}

fn estimate_zipf_parameter(sorted_frequencies: &[u32]) -> f64 {
    let sample_size = std::cmp::min(sorted_frequencies.len(), 10_000); 
    if sample_size < 2 { return 0.0; }

    let mut sum_x = 0.0; 
    let mut sum_y = 0.0; 
    let mut sum_xy = 0.0;
    let mut sum_xx = 0.0;

    for (i, &freq) in sorted_frequencies.iter().take(sample_size).enumerate() {
        let rank = (i + 1) as f64;
        let val = freq as f64;
        
        let lx = rank.ln();
        let ly = val.ln();

        sum_x += lx;
        sum_y += ly;
        sum_xy += lx * ly;
        sum_xx += lx * lx;
    }

    let n = sample_size as f64;
    let numerator = n * sum_xy - sum_x * sum_y;
    let denominator = n * sum_xx - sum_x * sum_x;

    if denominator == 0.0 {
        return 0.0;
    }

    - (numerator / denominator)
}
