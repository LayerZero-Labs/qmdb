use bench::{
    cli::Cli,
    speed::results::BenchmarkResults,
    speed::{db_backend, test_gen::TestGenV2, zipf::ZipfSampler},
    RandSrc, SimpleTask, COUNTERS,
};

use qmdb_common::def::{OP_CREATE, OP_DELETE, OP_READ, OP_WRITE};
use qmdb_common::def::SHARD_COUNT;
use qmdb_common::entryfile::entry::EntryBz;
use qmdb_common::task::taskid::join_task_id;
use qmdb_common::utils::changeset::ChangeSet;

use chrono::Utc;
use clap::Parser;
use log::{info, warn};
use parking_lot::RwLock;

use byteorder::BigEndian;
use byteorder::ByteOrder;

use std::collections::{HashMap, HashSet};
use rayon::prelude::*;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::mem;
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;

use eth_replay_kv::{self, Operation, PREFETCH_BUFFER_BLOCKS};

#[cfg(feature = "qmdb_bench_correctness")]
use bench::speed::correctness::{build_key_and_expected_value, BitmapModel};

#[cfg(all(
    feature = "qmdb_bench_correctness",
    not(feature = "use_mdbx"),
    not(feature = "use_rocksdb"),
    not(feature = "use_nomt"),
    not(feature = "use_reth"),
    not(feature = "use_lvmt")
))]
use bench::speed::qmdb;

const N_TABLES: usize = 1;
const PRIME1: u64 = 1299827; // Used for stride for hover_recreate_block

use bench::speed::result_log;

// TODO: 这个是否有用
use qmdb_common::indexer::indexer_trait::IndexerTrait;

struct RunConfig<'a> {
    table_id: usize,
    db_dir: &'a str,
    test_gen: &'a mut TestGenV2,
    randsrc: &'a mut RandSrc,
    results: &'a mut BenchmarkResults,
    hover_interval: u64,
    hover_recreate_block: u64,
    hover_write_block: u64,
    /// Zipf exponent for read/write key selection. 0.0 = uniform.
    zipf_theta: f64,
    output_filename: &'a str,

    #[cfg(feature = "qmdb_bench_correctness")]
    check_correctness: bool,
}

struct HoverTaskConfig<'a> {
    table_id: usize,
    db_dir: &'a str,
    randsrc: &'a mut RandSrc,
    height: &'a mut i64,
    test_gen: &'a mut TestGenV2,
    hover_recreate_block: u64,
    hover_write_block: u64,
    /// Zipf exponent for read/write key selection. 0.0 = uniform.
    zipf_theta: f64,
    /// True only for the post-population hover (last block), start-from-existing,
    /// or the immediate-existing hover. Insert+delete run ONLY when this is true;
    /// mid-population hovers run read/write only.
    is_final: bool,
    results: &'a mut BenchmarkResults,

    /// Cumulative count of keys deleted across all hover runs so far.
    /// Used to compute per-run stride_offset for delete (no cross-run double-deletion)
    /// and the sequential insert offset above entry_count.
    total_deleted_keys: &'a mut u64,

    // IO Stat States
    last_snapshot_time: &'a mut Instant,
    last_user_bytes: &'a mut u64,
    last_gc_bytes: &'a mut u64,

    #[cfg(feature = "qmdb_bench_correctness")]
    check_correctness: bool,
    #[cfg(feature = "qmdb_bench_correctness")]
    correctness_model: &'a mut Option<BitmapModel>,
}

struct HoverExecuteConfig<'a> {
    table_id: usize,
    operation_name: &'a str,
    op_type: u8,
    block_count: u64,
    op_in_blk: u64,
    stride: u64,
    max_num: u64,
    /// Added to the sampled key number before calling fill_kv. Used to shift
    /// the insert key range above entry_count without modifying max_num semantics.
    num_offset: u64,
    /// When stride != u64::MAX, added to each block's start position (mod max_num). Always 0.
    stride_offset: u64,
    /// Zipf exponent for key selection when stride == u64::MAX. 0.0 = uniform.
    zipf_theta: f64,
    test_generator: &'a mut TestGenV2,
    height: &'a mut i64,
    randsrc: &'a mut RandSrc,
    results: &'a mut BenchmarkResults,

    // IO Stat States
    last_snapshot_time: &'a mut Instant,
    last_user_bytes: &'a mut u64,
    last_gc_bytes: &'a mut u64,
}

struct SingleHoverBlockConfig<'a> {
    table_id: usize,
    randsrc: &'a mut RandSrc,
    test_gen: &'a mut TestGenV2,
    height: &'a mut i64,
    op_type: u8,
    start_num: u64,
    stride: u64,
    max_num: u64,
    num_offset: u64,
    /// Pre-built Zipf sampler. None = use uniform random.
    zipf_sampler: Option<&'a ZipfSampler>,
    /// Pre-sampled key indices for this block, laid out flat in the same order
    /// the inner loop consumes them: task-major, then cset, then op. When set,
    /// the zipf_sampler is bypassed (collision-free by construction). Length
    /// must equal task_in_block * cset_in_task * (wr_op_in_cset + delete_op_in_cset).
    pre_sampled: Option<&'a [u64]>,
}

use qmdb::VERBOSE;
macro_rules! vprintln {
    ($($arg:tt)*) => {
        if VERBOSE { println!($($arg)*); }
    };
}

fn main() {
    let args: Cli = Cli::parse();
    let start_from_existing = args.start_from_existing;
    let mut results = BenchmarkResults::new(&args);
    let table_id = 0;
    rayon::ThreadPoolBuilder::new()
        .num_threads(args.rayon_threads)
        .build_global()
        .unwrap();

    // ########### Initialize Database ###########
    // Make db_dir if it does not exist
    let is_eth_mode = args.eth_trace_dir.is_some() || args.init_db_path.is_some();
    std::fs::create_dir_all(args.db_dir.as_str()).unwrap();
    // Initialize the metrics-log sink and resolve the OS-physical-R/W disk from
    // db_dir (single source: one --db-dir drives DB path + I/O counters).
    result_log::init_result_log(&args.result_log);
    result_log::init_monitor_disk(&args.monitor_disk, args.db_dir.as_str());
    for i in 0..N_TABLES {
        db_backend::init(args.db_dir.as_str(), i, start_from_existing, is_eth_mode, args.entry_count);
    }
    println!("Using database backend: {}", db_backend::NAME);
    results.db_backend = db_backend::NAME.to_string();
    results.log_time("initialized");
    thread::sleep(Duration::from_secs(15));

    // ########### Start Benchmark ###########
    if let Some(ref bundle_path) = args.init_db_path {
        // Real Ethereum workload, but Phase 1 loads a pre-computed InitDbBundle
        // (init_db + boundary_ops) instead of regenerating it from traces.
        // --eth-trace-dir is still required: Phase 3 streams replay ops from it.
        let replay_start = if args.eth_replay_start_height == 0 { args.eth_start_height } else { args.eth_replay_start_height };
        let replay_end   = if args.eth_replay_end_height   == 0 { args.eth_end_height   } else { args.eth_replay_end_height   };
        println!(
            "Real Ethereum Workload Mode (from bundle):\n  Bundle path: {}\n  Replay start: {}\n  Replay end: {}",
            bundle_path, replay_start, replay_end,
        );
        let trace_dir = args.eth_trace_dir.as_deref()
            .expect("--eth-trace-dir is required even in bundle mode (used for Phase 3 replay ops)");
        run_eth_replay_from_bundle(
            table_id,
            bundle_path,
            trace_dir,
            replay_start,
            replay_end,
            &mut results,
        );
    } else if let Some(ref path) = args.eth_trace_dir {
        println!(
            "Real Ethereum Workload Mode:\n  Ethereum workload path: {}\n  Start block: {}\n  End block: {}",
            path,
            args.eth_start_height,
            args.eth_end_height
        );

        let replay_start = if args.eth_replay_start_height == 0 { args.eth_start_height } else { args.eth_replay_start_height };
        let replay_end   = if args.eth_replay_end_height   == 0 { args.eth_end_height   } else { args.eth_replay_end_height   };
        run_eth_replay_mode(
            table_id, 
            path,
            args.eth_start_height,
            args.eth_end_height,
            replay_start,
            replay_end,
            &mut results
        );
    } else {
        // -------------------------------------------------------------
        // 1. QMDB 应用层 GC Monitor 初始化（当前未启用）
        // -------------------------------------------------------------
        // let _monitor_handle = spawn_qmdb_monitor(args.db_dir.clone());

        // -------------------------------------------------------------
        // 2. 计算 block/task/changeset 等常量
        // -------------------------------------------------------------
        // 一个 changeset ≈ 10 个操作（典型为 9 写 + 1 删）
        // 一个 task 包含 m 个 changeset
        // ops_per_block = tasks_per_block * changesets_per_task * ops_per_changeset
        let ops_per_changeset = 10;
        let tasks_per_block = args.ops_per_block / (args.changesets_per_task * ops_per_changeset);

        // task_id 编码规则：
        // - 低 IN_BLOCK_IDX_BITS 位：task 在 block 内的局部索引
        // - 高位：block height
        // 此处检查 tasks_per_block 是否会导致 task_id 溢出
        if tasks_per_block >= join_task_id(1, 0, false) as u64 {
            panic!("tasks_per_block {tasks_per_block} is too large");
        }

        // -------------------------------------------------------------
        // 3. 检查数据库大小是否合理
        // -------------------------------------------------------------
        // entry_count 必须是 ops_per_block 的整数倍，否则无法按 block 生成
        if args.entry_count % args.ops_per_block != 0 {
            panic!(
                "entry_count {} is not divisible by ops_per_block {}",
                args.entry_count, args.ops_per_block
            );
        }

        // 数据库初始化阶段需要的 block 数
        let mut blocks_for_db_population = args.entry_count / args.ops_per_block;

        let mut initial_height: i64;

        // -------------------------------------------------------------
        // 4. 根据是否从已有数据库启动，设置初始高度与工作量
        // -------------------------------------------------------------
        if start_from_existing {
            println!("\n===============================================================");
            println!("!!! STARTING FROM AN EXISTING DB: Skipping Population Phase !!!");
            println!("===============================================================\n");

            // 1. 快进高度：让 TestGen 生成新的 key，而不是覆盖旧数据
            initial_height = (blocks_for_db_population as i64) + 1;

            // 2. 告诉 results 当前已有多少 entries，确保 TPS 计算正确
            results.update_entries_in_db(args.entry_count);

            // 3. 跳过数据库构建阶段
            blocks_for_db_population = 0;

            println!("Workload configuration (Existing DB Mode):");
            println!("  Assumed Existing Entries: {}", args.entry_count);
            println!("  Starting Height: {}", initial_height);
            println!("  Changesets per task: {}", args.changesets_per_task);
            println!("  Tasks per block: {}", tasks_per_block);

        } else {
            println!("\n=================================");
            println!("!!! STARTING FROM AN EMPTY DB !!!");
            println!("=================================\n");

            initial_height = 1;

            println!("Workload configuration:");
            println!("  Blocks for DB population: {}", blocks_for_db_population);
            println!("  Changesets per task/transaction: {}", args.changesets_per_task);
            println!("  Operations per changeset: {}", ops_per_changeset);
            println!("  Tasks (transactions) per block: {}", tasks_per_block);
            println!("  Total operations per block: ~{}", tasks_per_block * args.changesets_per_task * ops_per_changeset);

            info!(
                "{} blocks x {} ops per block = {} entries created",
                blocks_for_db_population, args.ops_per_block, args.entry_count
            );
            info!(
                "Every {} blocks ({} entries), we will benchmark ops/sec",
                args.hover.hover_interval,
                args.hover.hover_interval * args.ops_per_block
            );

            // hover 参数合法性检查
            if args.hover.hover_interval > blocks_for_db_population {
                warn!(
                    "ERROR: hover_interval {} is greater than the number of blocks in a round {}; it will never run",
                    args.hover.hover_interval,
                    blocks_for_db_population
                );
            }
            if args.hover.hover_recreate_block > blocks_for_db_population {
                warn!(
                    "hover_recreate_block {} is greater than the number of blocks in a round {}",
                    args.hover.hover_recreate_block, blocks_for_db_population
                );
            }
            if args.hover.hover_write_block > blocks_for_db_population {
                warn!(
                    "hover_write_block {} is greater than the number of blocks in a round {}",
                    args.hover.hover_write_block, blocks_for_db_population
                );
            }
        }

        // -------------------------------------------------------------
        // 5. 创建 TestGen（生成 workload）
        // -------------------------------------------------------------
        let mut randsrc: RandSrc =
            RandSrc::new(args.randsrc_filename.as_str(), &format!("qmdb-{table_id}"));
        let mut test_gen_ = TestGenV2::new(
            &mut randsrc,
            args.entry_count,
            args.changesets_per_task as usize,
            tasks_per_block as usize,
        );

        // -------------------------------------------------------------
        // 6. 正确性模型（仅在启用 qmdb_bench_correctness 时生效）
        // -------------------------------------------------------------
        #[cfg(feature = "qmdb_bench_correctness")]
        let correctness_model = if args.check_correctness {
            let entry_count_wo_del = args.entry_count * 9 / 10;
            Some(BitmapModel::new(args.entry_count, entry_count_wo_del))
        } else {
            None
        };
        #[cfg(not(feature = "qmdb_bench_correctness"))]
        let correctness_model: Option<()> = None;

        // -------------------------------------------------------------
        // 7. 运行 Benchmark
        // -------------------------------------------------------------
        run(
            RunConfig {
                table_id,
                db_dir: args.db_dir.as_str(),
                test_gen: &mut test_gen_,
                randsrc: &mut randsrc,
                results: &mut results,
                hover_interval: args.hover.hover_interval,
                hover_recreate_block: args.hover.hover_recreate_block,
                hover_write_block: args.hover.hover_write_block,
                zipf_theta: args.hover.hover_zipf_theta,
                output_filename: args.output_filename.as_str(),
                #[cfg(feature = "qmdb_bench_correctness")]
                check_correctness: args.check_correctness,
            },
            correctness_model,
            initial_height,
        );
    }
}

fn run(
    config: RunConfig,
    #[cfg(feature = "qmdb_bench_correctness")] mut correctness_model: Option<BitmapModel>,
    #[cfg(not(feature = "qmdb_bench_correctness"))] _correctness_model: Option<()>,
    initial_height: i64,
) {
    let RunConfig {
        table_id,
        db_dir,
        test_gen,
        randsrc,
        results,
        hover_interval,
        hover_recreate_block,
        hover_write_block,
        zipf_theta,
        output_filename,
        #[cfg(feature = "qmdb_bench_correctness")]
        check_correctness,
    } = config;

    let blk_in_round = test_gen.block_in_round();
    let mut height = initial_height;
    let mut current_decile_print: i64 = -1;

    let mut last_print = Instant::now();
    let mut hover_time_since_last_print = Duration::from_secs(0);

    // IO Stat Trackers
    // TODO: 除了QMDB，其他是否有用到这个
    let mut last_snapshot_time = Instant::now();
    let mut last_user_bytes = 0u64;
    let mut last_gc_bytes = 0u64;
    // Cumulative keys deleted across hover runs — drives delete stride_offset and insert num_offset.
    let mut total_deleted_keys: u64 = 0;

    let REPORT_INTERVAL = 2;

    // 开始Population的Timestamp
    println!("Timestamp: {}", Utc::now().to_rfc3339());
    let start = Instant::now();

    // Populate the database
    if height == 1 {
        for produced_blocks in 0..blk_in_round {
            // Every hover_interval blocks, we run hover tasks to benchmark ops
            // Every hover_write_block blocks, we time how long it takes to create the blocks
            // Height increases for every block added (here and in run_single_hover_block)

            // 每10%输出一次进度
            if current_decile_print < (produced_blocks as f64 / blk_in_round as f64 * 10.0) as i64
                || produced_blocks == 0
                || produced_blocks == blk_in_round - 1
                || produced_blocks % 10 == 0
            {
                let time_left = if produced_blocks > 0 {
                    start
                        .elapsed()
                        .mul_f64(((blk_in_round - produced_blocks) as f64) / (produced_blocks as f64))
                } else {
                    Duration::from_secs(0)
                };
                println!(
                    "{}%: producing block {} (entry {} - {}) [Time left: {:.2?}]",
                    produced_blocks * 100 / blk_in_round,
                    produced_blocks,
                    produced_blocks * test_gen.num_op_in_blk(),
                    (produced_blocks + 1) * test_gen.num_op_in_blk(),
                    time_left
                );
                current_decile_print = (produced_blocks as f64 / blk_in_round as f64 * 10.0) as i64;
            }
            
            // 1. 生成population的block
            let task_list = test_gen.gen_block();

            #[cfg(feature = "qmdb_bench_correctness")]
            if check_correctness {
                if let Some(model) = correctness_model.as_mut() {
                    // During population we only update the bitmap model; sampling
                    // and DB comparisons are performed during TPS benchmarking.
                    model.apply_block(&task_list);
                }
            }
            
            // 2. 提交block至backend database
            vprintln!("PIPELINE_LOG: CLIENT_GENERATE_END height={} timestamp={}", height, Utc::now().to_rfc3339());
            vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_START height={} timestamp={}", height, Utc::now().to_rfc3339());
            db_backend::create_kv(table_id, height, task_list);
            vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_END height={} timestamp={}", height, Utc::now().to_rfc3339());
            height += 1;
            vprintln!("PIPELINE_LOG: CLIENT_GENERATE_START height={} timestamp={}", height, Utc::now().to_rfc3339());

            results.update_entries_in_db((produced_blocks + 1) * test_gen.num_op_in_blk());
            // Log IO Stat
            if height % REPORT_INTERVAL == 0 {
                log_io_stat(height, &mut last_snapshot_time, &mut last_user_bytes, &mut last_gc_bytes);
            }

            // 记录Population数据
            // 1. Update the user interactively, time left, TPS -- based on time elapsed
            // 2. Log throughput every K blocks so that we can see the trend
            //    - Need to update the last value, and store a new value in a vector
            //    - Should also log the timestamp (absolute and relative)
            // Print a progress message every hover_write_block=10(dev)/1500(test) blocks
            let update_now = 
                (hover_write_block == 0) 
                || (produced_blocks % hover_write_block == 0) 
                || (produced_blocks == blk_in_round - 1);

            if update_now {
                results.record_throughput_as_total(
                    "block_population",
                    produced_blocks + 1,
                    last_print.elapsed() - hover_time_since_last_print,
                );
                // Using this is a better measure of inserts compared to recreating recently deleted entries
                results.record_throughput_as_total(
                    "fresh_inserts",
                    results.entries_in_db,
                    last_print.elapsed() - hover_time_since_last_print,
                );
                hover_time_since_last_print = Duration::from_secs(0);
                last_print = Instant::now();
            }

            // 周期性运行Hover test
            // We need to ensure that hover_recreate_block <= number of produced entries
            // Run delete/create/update/read benchmarks every hover_interval blocks (not on the first block)
            if (produced_blocks + 1) % hover_interval == 0 || produced_blocks == blk_in_round - 1 {
                // Hover test 开始的时间
                thread::sleep(Duration::from_secs(60));
                println!("Timestamp: {}", Utc::now().to_rfc3339());

                let hover_start = Instant::now();
                results.log_system_stats();

                // Final hover = the post-population one (last block). Only then run
                // insert/delete; mid-population hovers are read/write only.
                let is_final = produced_blocks == blk_in_round - 1;
                run_hover_tasks(HoverTaskConfig {
                    table_id,
                    db_dir,
                    randsrc,
                    height: &mut height,
                    test_gen,
                    hover_recreate_block,
                    hover_write_block,
                    zipf_theta,
                    is_final,
                    results,
                    total_deleted_keys: &mut total_deleted_keys,
                    last_snapshot_time: &mut last_snapshot_time,
                    last_user_bytes: &mut last_user_bytes,
                    last_gc_bytes: &mut last_gc_bytes,
                    #[cfg(feature = "qmdb_bench_correctness")]
                    check_correctness,
                    #[cfg(feature = "qmdb_bench_correctness")]
                    correctness_model: &mut correctness_model,
                });

                // Hover test 结束的时间
                println!("Timestamp: {}", Utc::now().to_rfc3339());

                println!("Writing partial results to file: {output_filename}");
                results.write_to_file(output_filename);
                hover_time_since_last_print += hover_start.elapsed();
            }

            if height % 10 == 0 {
                COUNTERS.print();
            }
        }
    } else {
        // === 从已有数据启动 (Start From Existing) ===
        // 我们跳过了造数据过程，但我们希望能测一下当前已有数据的性能 (Hover Test)
        // 就像原本 Population 最后一次 Hover Test 那样
        
        println!("!!! Skipping Population Loop (Starting from height {}) !!!", height);
        println!("Running immediate Hover Test on existing database state...");

        thread::sleep(Duration::from_secs(60));
        println!("Timestamp: {}", Utc::now().to_rfc3339());
        
        // 1. test_gen更新
        for i in 0..(height - 1) {
            let _ = test_gen.gen_block(); 
            
            // 打印一点进度，防止以为卡死了
            if i % 100 == 0 {
                use std::io::Write;
                print!("\nFast-forwarding: {}/{}", i, height - 1);
                std::io::stdout().flush().unwrap();
            }
        }
        println!("Timestamp: {}", Utc::now().to_rfc3339());

        // 2. cache warmup
        // TODO: RETH需要这个逻辑
        // TODO: 这里的read是错的！需要重写这部分
        #[cfg(any(feature = "use_nomt"))]
        {
            let total_entries = test_gen.sp.entry_count as u64;
            println!("Warmup START: Reading entire database sequentially (0 to {}) !!!", total_entries);
            let warmup_start = Instant::now();
            
            // 每次提交的并发量，可根据机器配置调整
            let batch_size = 100000; 
            let mut tasks = Vec::with_capacity(batch_size);

            for i in 0..total_entries {
                let mut k = [0u8; 20];
                let mut v = [0u8; 32];
                
                // 使用 test_gen 计算确定性的 Key (和 Populate 阶段一致)
                let kh = test_gen.fill_kv(i as u64, &mut k[..], &mut v[..]);

                
                // 构造 READ 操作
                let mut cset = ChangeSet::new();
                cset.add_op(OP_READ, &kh, &k[..], &v[..], None);
                cset.sort();
                
                // 封装成 SimpleTask
                let task = SimpleTask::new(vec![cset]);
                tasks.push(RwLock::new(Some(task)));

                // 批次发送
                if tasks.len() >= batch_size {
                    db_backend::read_kv(table_id, tasks);
                    tasks = Vec::with_capacity(batch_size); // Reset buffer

                    if (i + 1) % 1_000_000 == 0 {
                        let elapsed = warmup_start.elapsed().as_secs_f64();
                        println!(
                            "Warmup progress: {}/{} ({:.1}%) - Speed: {:.2} M ops/sec", 
                            i + 1, 
                            total_entries, 
                            ((i + 1) as f64 / total_entries as f64) * 100.0,
                            (i + 1) as f64 / 1_000_000.0 / elapsed
                        );
                    }
                }
            }

            // 处理剩余部分
            if !tasks.is_empty() {
                db_backend::read_kv(table_id, tasks);
            }

            println!("!!! Warmup COMPLETE: Took {:.2?} !!!", warmup_start.elapsed());
            
            // 强制休眠，确保 OS 层面的预读/Writeback 彻底平息，不影响后续 Hover 压测的 IO 统计
            thread::sleep(Duration::from_secs(60));
        }
        #[cfg(feature = "use_reth")]
        {   
            // warmup metadata part
            db_backend::warmup_trie();
        }
        println!("Timestamp: {}", Utc::now().to_rfc3339());
        
        // 3. 开始CRUD性能测试
        let hover_start = Instant::now();
        results.log_system_stats();

        run_hover_tasks(HoverTaskConfig {
            table_id,
            db_dir,
            randsrc,
            height: &mut height,
            test_gen,
            hover_recreate_block,
            hover_write_block,
            zipf_theta,
            is_final: true,
            results,
            total_deleted_keys: &mut total_deleted_keys,
            last_snapshot_time: &mut last_snapshot_time,
            last_user_bytes: &mut last_user_bytes,
            last_gc_bytes: &mut last_gc_bytes,
            #[cfg(feature = "qmdb_bench_correctness")]
            check_correctness,
            #[cfg(feature = "qmdb_bench_correctness")]
            correctness_model: &mut correctness_model,
        });

        // 记录一下时间，虽然不计入 population throughput，但有助于日志对齐
        hover_time_since_last_print += hover_start.elapsed();
        thread::sleep(Duration::from_secs(60));
    }

    println!("Block population complete. Writing partial results to file: {output_filename}");
    results.write_to_file(output_filename);

    results.log_system_stats();
    results.success = true;
    results.complete = true;
    results.log_time("complete");
    results.print_summary();

    println!("Writing results to file: {output_filename}");
    results.write_to_file(output_filename);
    println!("Benchmarking completed successfully");
}

fn run_hover_tasks(config: HoverTaskConfig) {
    let HoverTaskConfig {
        table_id,
        db_dir,
        randsrc,
        height,
        test_gen,
        hover_recreate_block,
        hover_write_block,
        zipf_theta,
        is_final,
        results,
        total_deleted_keys,
        last_snapshot_time,
        last_user_bytes,
        last_gc_bytes,
        #[cfg(feature = "qmdb_bench_correctness")]
        check_correctness,
        #[cfg(feature = "qmdb_bench_correctness")]
        correctness_model,
    } = config;

    let op_in_blk = test_gen.num_op_in_blk();

    // 如果未population完成，就在当前数据库大小下测试，如果完成，就在整个数据库下测试
    let max_num = if test_gen.cur_round == 0 {
        test_gen.cur_num
    } else {
        test_gen.sp.entry_count
    };

    // --- UPDATES (write) ---
    hover_execute_op(HoverExecuteConfig {
        table_id,
        operation_name: "updates",
        op_type: OP_WRITE,
        block_count: hover_write_block,
        op_in_blk,
        stride: u64::MAX,
        max_num,
        num_offset: 0,
        stride_offset: 0,
        zipf_theta,
        test_generator: test_gen,
        height,
        randsrc,
        results,
        last_snapshot_time,
        last_user_bytes,
        last_gc_bytes,
    });

    // --- READS ---
    hover_execute_op(HoverExecuteConfig {
        table_id,
        operation_name: "reads",
        op_type: OP_READ,
        block_count: hover_write_block,
        op_in_blk,
        stride: u64::MAX,
        max_num,
        num_offset: 0,
        stride_offset: 0,
        zipf_theta,
        test_generator: test_gen,
        height,
        randsrc,
        results,
        last_snapshot_time,
        last_user_bytes,
        last_gc_bytes,
    });

    // Insert/delete run ONLY on the final (post-population) hover. Mid-population
    // hovers run read/write only — inserting/deleting before population completes
    // corrupts entry bookkeeping and can crash on deleted-key lookups.
    if !is_final {
        println!("Mid-population hover: read/write only (insert/delete deferred to final hover)");
    } else if zipf_theta != 0.0 {
        println!("Skipping delete and insert benchmarks (zipf_theta={zipf_theta} != 0)");
    } else if hover_recreate_block * op_in_blk < max_num {
        let n_hover_ops = hover_recreate_block * op_in_blk;
        // Always base above the FINAL entry_count (not cur_num), so population never
        // fills in the same key numbers that hover inserts, preventing duplicate-key panics.
        let entry_count = test_gen.sp.entry_count;

        // --- INSERTS first (brand-new keys above entry_count, advancing each run) ---
        // num_offset = entry_count + keys inserted in previous hover runs.
        // These keys are never touched by normal population, so no collision.
        let insert_num_offset = entry_count + *total_deleted_keys;
        hover_execute_op(HoverExecuteConfig {
            table_id,
            operation_name: "inserts",
            op_type: OP_CREATE,
            block_count: hover_recreate_block,
            op_in_blk,
            stride: PRIME1,
            max_num: n_hover_ops,
            num_offset: insert_num_offset,
            stride_offset: 0,
            zipf_theta: 0.0,
            test_generator: test_gen,
            height,
            randsrc,
            results,
            last_snapshot_time,
            last_user_bytes,
            last_gc_bytes,
        });

        // --- DELETES (PRIME1 stride, stride_offset advances each run to avoid cross-run double-deletion) ---
        // stride=PRIME1 is coprime to max_num → visits N distinct keys in [0, max_num) per run.
        // stride_offset shifts the starting position each run so successive runs target different keys.
        let delete_stride_offset = total_deleted_keys.wrapping_mul(PRIME1) % max_num;
        hover_execute_op(HoverExecuteConfig {
            table_id,
            operation_name: "deletes",
            op_type: OP_DELETE,
            block_count: hover_recreate_block,
            op_in_blk,
            stride: PRIME1,
            max_num,
            num_offset: 0,
            stride_offset: delete_stride_offset,
            zipf_theta: 0.0,
            test_generator: test_gen,
            height,
            randsrc,
            results,
            last_snapshot_time,
            last_user_bytes,
            last_gc_bytes,
        });
        *total_deleted_keys += n_hover_ops;
    } else {
        println!("skip hover delete and insert blocks (db too small)");
    }

    #[cfg(all(
        feature = "qmdb_bench_correctness",
        not(feature = "use_mdbx"),
        not(feature = "use_rocksdb"),
        not(feature = "use_nomt"),
        not(feature = "use_reth"),
        not(feature = "use_lvmt")
    ))]
    if check_correctness {
        if let Some(model) = correctness_model.as_mut() {
            db_backend::flush(table_id);
            // Reuse the current `max_num` as the upper bound for sampling.
            run_hover_correctness_sampling(table_id, max_num, test_gen, model, randsrc, results);
        }
    }

    //measure_read_latency(
    //    table_id,
    //    max_num,
    //    randsrc,
    //    results,
    //    num_read_latency_samples,
    //);

    let db_size_bytes = dir_size(Path::new(db_dir));
    let rss_kb = get_rss_kb();
    result_log::write_line(&format!(
        "HOVER_END_STATS: db_size={:.2} GB  rss={:.2} GB",
        db_size_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        rss_kb as f64 / (1024.0 * 1024.0),
    ));
}

fn hover_execute_op(config: HoverExecuteConfig) {
    let HoverExecuteConfig {
        table_id,
        operation_name,
        op_type,
        block_count,
        op_in_blk,
        stride,
        max_num,
        num_offset,
        stride_offset,
        zipf_theta,
        test_generator,
        height,
        randsrc,
        results,
        last_snapshot_time,
        last_user_bytes,
        last_gc_bytes,
    } = config;

    // Build Zipf sampler once for the entire operation (avoids re-init per block).
    // Only used when stride == u64::MAX (random mode) and theta > 0.
    let zipf_sampler: Option<ZipfSampler> = if stride == u64::MAX && zipf_theta > 0.0 {
        Some(ZipfSampler::new(max_num, zipf_theta))
    } else {
        None
    };

    // Pre-sample all Zipf keys up front, so the bench-timer loop never pays
    // the per-key sampling cost (which dominates at high theta and caps
    // measured TPS far below true engine throughput).
    //
    // Layout: flat Vec<u64>, indexed task-major then cset then op, matching
    // exactly the consumption order in run_single_hover_block. Dedup per cset
    // is applied during pre-sampling so the consumer can blindly index in.
    let pre_sampled_keys: Option<Vec<u64>> = if let Some(sampler) = zipf_sampler.as_ref() {
        let task_in_block = test_generator.task_in_block;
        let cset_in_task = test_generator.cset_in_task;
        let n_per_cset = test_generator.wr_op_in_cset + test_generator.delete_op_in_cset;
        let keys_per_block = task_in_block * cset_in_task * n_per_cset;
        let total_keys = (block_count as usize) * keys_per_block;
        let t0 = Instant::now();
        println!(
            "[ZIPF_PRESAMPLE] op={} block_count={} keys_per_block={} total_keys={} (~{} MB)",
            operation_name,
            block_count,
            keys_per_block,
            total_keys,
            total_keys * 8 / 1024 / 1024,
        );
        let mut buf: Vec<u64> = Vec::with_capacity(total_keys);
        let mut used: std::collections::HashSet<u64> =
            std::collections::HashSet::with_capacity(n_per_cset);
        let log_every: usize = 10_000_000;
        let mut next_log: usize = log_every;
        let total_csets = (block_count as usize) * task_in_block * cset_in_task;
        for _cset_idx in 0..total_csets {
            used.clear();
            for _j in 0..n_per_cset {
                let mut cand = sampler.sample(randsrc);
                while used.contains(&cand) {
                    cand = sampler.sample(randsrc);
                }
                used.insert(cand);
                buf.push(cand);
                if buf.len() >= next_log {
                    println!(
                        "[ZIPF_PRESAMPLE]   progress {}/{} ({:.1}s)",
                        buf.len(), total_keys, t0.elapsed().as_secs_f64()
                    );
                    next_log += log_every;
                }
            }
        }
        println!(
            "[ZIPF_PRESAMPLE] op={} done in {:.2}s ({:.2} us/key)",
            operation_name,
            t0.elapsed().as_secs_f64(),
            t0.elapsed().as_micros() as f64 / total_keys.max(1) as f64,
        );
        Some(buf)
    } else {
        None
    };

    // 1. 输出当前的磁盘的信息
    let (start_os_read, start_os_write) = get_linux_disk_rw_sectors(result_log::monitor_disk()).unwrap_or((0, 0));
    println!("Timestamp (Start): {}", Utc::now().to_rfc3339());

    // 2. 核心执行逻辑
    let (tx, rx) = mpsc::sync_channel(10);
    let consumer_handle = thread::spawn(move || {
        while let Ok((blk_height, tasks)) = rx.recv() {
            match op_type {
                OP_CREATE => {
                    vprintln!("PIPELINE_LOG: CLIENT_GENERATE_END height={} timestamp={}", blk_height, Utc::now().to_rfc3339());
                    vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_START height={} timestamp={}", blk_height, Utc::now().to_rfc3339());
                    db_backend::create_kv(table_id, blk_height, tasks);
                    vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_END height={} timestamp={}", blk_height, Utc::now().to_rfc3339());
                },
                OP_DELETE => db_backend::delete_kv(table_id, blk_height, tasks),
                OP_WRITE => db_backend::update_kv(table_id, blk_height, tasks),
                OP_READ => {
                    #[cfg(all(not(feature = "use_mdbx"), not(feature = "use_rocksdb"), not(feature = "use_nomt"), not(feature = "use_reth"), not(feature = "use_lvmt")))]
                    db_backend::update_kv(table_id, blk_height, tasks);

                    #[cfg(any(feature = "use_mdbx", feature = "use_rocksdb", feature = "use_nomt", feature = "use_reth", feature = "use_lvmt"))]
                    db_backend::read_kv(table_id, tasks);
                }
                _ => panic!("invalid op_type: {}", op_type),
            }
        }
    });

    let bm_start = Instant::now();
    print!(
        "Hover Test {} ({} ops, {} blocks, stride={})...",
        operation_name,
        block_count * op_in_blk,
        block_count,
        if stride == u64::MAX {
            if zipf_theta > 0.0 {
                format!("zipf(theta={zipf_theta})")
            } else {
                "random".to_string()
            }
        } else {
            format!("{stride}")
        }
    );

    let mut current_height = *height;

    for i in 0..block_count {
        let start: u64 = if stride == u64::MAX {
            rand_between(randsrc, 0, max_num as usize) as u64
        } else {
            (stride_offset + i * stride * op_in_blk) % max_num
        };
        let block_slice: Option<&[u64]> = pre_sampled_keys.as_ref().map(|v| {
            let kpb = test_generator.task_in_block
                * test_generator.cset_in_task
                * (test_generator.wr_op_in_cset + test_generator.delete_op_in_cset);
            let lo = (i as usize) * kpb;
            &v[lo..lo + kpb]
        });
        let tasks = run_single_hover_block(SingleHoverBlockConfig {
            table_id,
            randsrc,
            test_gen: test_generator,
            height: &mut current_height,
            op_type,
            start_num: start,
            stride,
            max_num,
            num_offset,
            zipf_sampler: zipf_sampler.as_ref(),
            pre_sampled: block_slice,
        });

        tx.send((current_height, tasks)).unwrap();
        current_height += 1;

        if i % 10 == 5 {
            println!("hover {operation_name} {i} (partial)");
        }
        if i % 2 == 0 {
            log_io_stat(*height, last_snapshot_time, last_user_bytes, last_gc_bytes);
        }
    }
    drop(tx); 
    consumer_handle.join().unwrap();
    // Wait for QMDB engine pipeline to fully drain before stopping the timer.
    // Without this, the channel-buffered consumer thread architecture lets the
    // bench timer stop ~2 blocks before engine completion, causing ~2-9% over-
    // measurement of throughput.
    db_backend::flush(table_id);
    results.record_throughput(operation_name, block_count * op_in_blk, bm_start.elapsed());

    // 3. 输出当前的磁盘信息
    println!("Timestamp (End): {}", Utc::now().to_rfc3339());
    thread::sleep(Duration::from_secs(60)); 
    let (end_os_read, end_os_write) = get_linux_disk_rw_sectors(result_log::monitor_disk()).unwrap_or((0, 0));

    print_unified_report(operation_name, block_count * op_in_blk, start_os_read, end_os_read, start_os_write, end_os_write);

    *height = current_height;
}

fn run_single_hover_block(config: SingleHoverBlockConfig) -> Vec<RwLock<Option<SimpleTask>>> {
    let SingleHoverBlockConfig {
        table_id,
        randsrc,
        test_gen,
        height,
        op_type,
        mut start_num,
        stride,
        max_num,
        num_offset,
        zipf_sampler,
        pre_sampled,
    } = config;

    let n_per_cset = test_gen.wr_op_in_cset + test_gen.delete_op_in_cset;
    let cset_in_task = test_gen.cset_in_task;
    let task_in_block = test_gen.task_in_block;

    // Fast path: when pre_sampled is set, build tasks in parallel via rayon.
    // Each task is independent (fill_kv takes &self, all per-op state is on
    // the closure's stack), so this is a strict speedup that yields the same
    // output as the sequential path. Used to push gen below process latency
    // so reads at high theta expose true engine TPS.
    if let Some(ps) = pre_sampled {
        let height_val = *height;
        let is_qmdb = cfg!(all(
            not(feature = "use_mdbx"),
            not(feature = "use_rocksdb"),
            not(feature = "use_nomt"),
            not(feature = "use_reth"),
            not(feature = "use_lvmt")
        ));
        let task_list_par: Vec<RwLock<Option<SimpleTask>>> = (0..task_in_block)
            .into_par_iter()
            .map(|task_idx| {
                let mut cset_list = Vec::with_capacity(cset_in_task);
                for cset_idx in 0..cset_in_task {
                    let mut cset = ChangeSet::new();
                    let mut k = [0u8; 20];
                    let mut v = [0u8; 32];
                    let mut used_in_cset: std::collections::HashSet<u64> =
                        std::collections::HashSet::with_capacity(n_per_cset);
                    for j in 0..n_per_cset {
                        let flat_idx = task_idx * cset_in_task * n_per_cset
                            + cset_idx * n_per_cset
                            + j;
                        let x = ps[flat_idx];
                        used_in_cset.insert(x);
                        let kh = test_gen.fill_kv(x + num_offset, &mut k[..], &mut v[..]);

                        if op_type == OP_WRITE {
                            let h_bytes = (height_val as u64).to_be_bytes();
                            for idx in 0..8 {
                                v[idx] ^= h_bytes[idx];
                            }
                        }

                        if op_type == OP_READ && task_idx == 0 && is_qmdb {
                            cset.add_op(OP_WRITE, &kh, &k[..], &v[..], None);
                        } else {
                            cset.add_op(op_type, &kh, &k[..], &v[..], None);
                        }
                    }
                    cset.sort();
                    cset_list.push(cset);
                }
                let task = SimpleTask::new(cset_list);
                RwLock::new(Some(task))
            })
            .collect();
        return task_list_par;
    }

    // Sequential fallback (when pre_sampled is None — uniform random or stride mode).
    let mut presample_cursor: usize = 0;
    let mut task_list = Vec::with_capacity(test_gen.task_in_block);
    for _i in 0..test_gen.task_in_block {
        let mut cset_list = Vec::with_capacity(test_gen.cset_in_task);
        for i in 0..test_gen.cset_in_task {
            let mut cset = ChangeSet::new();
            let mut k = [0u8; 20];
            let mut v = [0u8; 32];
            let n = test_gen.wr_op_in_cset + test_gen.delete_op_in_cset;
            // Track used keys within this changeset to prevent duplicate-key deadlocks.
            let mut used_in_cset: std::collections::HashSet<u64> =
                std::collections::HashSet::with_capacity(n);
            for j in 0..n {
                let mut x = start_num;
                if stride == u64::MAX {
                    x = if let Some(ps) = pre_sampled {
                        let key = ps[presample_cursor];
                        presample_cursor += 1;
                        key
                    } else if let Some(sampler) = zipf_sampler {
                        // Zipf: re-sample on collision to avoid duplicate keys in one changeset.
                        let mut candidate = sampler.sample(randsrc);
                        while used_in_cset.contains(&candidate) {
                            candidate = sampler.sample(randsrc);
                        }
                        candidate
                    } else {
                        // Uniform: segment-based to avoid intra-changeset duplicates
                        rand_between_seg_idx(randsrc, 0, max_num as usize, n, j) as u64
                    };
                } else {
                    start_num = (start_num + stride) % max_num;
                }
                used_in_cset.insert(x);
                let kh = test_gen.fill_kv(x + num_offset, &mut k[..], &mut v[..]);
                
                if op_type == OP_WRITE {
                    let h_bytes = (*height as u64).to_be_bytes();
                    for idx in 0..8 {
                        v[idx] ^= h_bytes[idx]; 
                    }
                }
                
                // if this is qmdb, then we need at least 1 write for read block to have block id += 1
                let is_qmdb = cfg!(all(
                    not(feature = "use_mdbx"),
                    not(feature = "use_rocksdb"),
                    not(feature = "use_nomt"),
                    not(feature = "use_reth"),
                    not(feature = "use_lvmt")
                ));
                if op_type == OP_READ && _i == 0 && is_qmdb {
                    // 只有 QMDB 才强行塞一个 Write 进去
                    cset.add_op(OP_WRITE, &kh, &k[..], &v[..], None);
                } else {
                    cset.add_op(op_type, &kh, &k[..], &v[..], None);
                }
            }
            cset.sort();
            cset_list.push(cset);
        }
        let task = SimpleTask::new(cset_list);
        task_list.push(RwLock::new(Some(task)));
    }

    task_list
}

fn run_eth_replay_mode(
    table_id: usize,
    input_dir: &str, 
    populate_start: u64,
    populate_end: u64,
    replay_start: u64,
    replay_end: u64,
    results: &mut BenchmarkResults,
) {
    let start_height = populate_start;
    let end_height = populate_end;
    println!("Running Real Ethereum Workload");
    println!("Populate Range: Block {} to {}", populate_start, populate_end);
    println!("Replay   Range: Block {} to {}", replay_start, replay_end);

    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
    let source = eth_replay_kv::parse_storage_path(input_dir);
    let s3_client = if source.is_s3() {
        Some(rt.block_on(eth_replay_kv::new_s3_client()))
    } else {
        None
    };

    let mut global_db_height: i64 = 1;

    // =========================================================
    // Phase 1: 将KV文件中的内容读取到内存，生成init db内存版本
    // =========================================================
    println!("\n=== Phase 1: Populating Base State ===");
    
    let (tx_p1, mut rx_p1) = tokio::sync::mpsc::channel(eth_replay_kv::PREFETCH_BUFFER_BLOCKS);
    let source_clone_p1 = source.clone();
    let client_clone_p1 = s3_client.clone();
    
    // 启动生产者，按区块顺序吐出 base_kv 和 ops
    rt.spawn(async move {
        eth_replay_kv::run_block_data_producer(start_height, end_height, source_clone_p1, client_clone_p1, tx_p1).await
    });

    let mut splitter = eth_replay_kv::BaseStateSplitter::new();
    
    rt.block_on(async {
        while let Some(msg) = rx_p1.recv().await {
            let (height, base_kv_opt, ops_opt) = msg.expect("Failed to receive from producer");
            if let Some(b) = base_kv_opt {
                splitter.ingest_base_kv(height, b);
            }
            if let Some(o) = ops_opt {
                splitter.apply_ops(o);
            }
        }
    });

    let (init_db, boundary_ops) = splitter.into_parts();
    println!("init_db size: {}, boundary blocks: {}", init_db.len(), boundary_ops.len());

    // =========================================================
    // Phase 2: 将init db内存版本populate到DB
    // =========================================================
    let mut total_init_entries = 0;
    let ops_per_block = 100000;
    let ops_per_cset = 10;
    let csets_per_task = 2;
    let tasks_per_block = ops_per_block / (ops_per_cset * csets_per_task);

    let mut current_cset = ChangeSet::new();
    let mut current_cset_list: Vec<ChangeSet> = Vec::with_capacity(csets_per_task);
    let mut batch_tasks: Vec<parking_lot::RwLock<Option<SimpleTask>>> = Vec::with_capacity(tasks_per_block);

    for (k, v) in init_db {
        if v.is_empty() {
            println!("Empty value for key: {:?}", k);
            continue;
        }

        let kh = qmdb_common::entryfile::entry::entry_key_hash(&k);
        current_cset.add_op(OP_CREATE, &kh, &k, &v, None);
        total_init_entries += 1;

        // 1. 检查 ChangeSet 是否已满 (达到 10 ops)
        if current_cset.op_list.len() >= ops_per_cset {
            current_cset.sort();
            current_cset_list.push(current_cset);
            current_cset = ChangeSet::new(); // 重置当前 ChangeSet

            // 2. 检查 Task 是否已满 (达到 2 csets)
            if current_cset_list.len() >= csets_per_task {
                let task = SimpleTask::new(current_cset_list);
                batch_tasks.push(parking_lot::RwLock::new(Some(task)));
                current_cset_list = Vec::with_capacity(csets_per_task); // 重置当前 Task 的 cset 列表

                // 3. 检查 Block 是否已满 (达到 5000 tasks，即 100,000 ops)
                if batch_tasks.len() >= tasks_per_block {
                    vprintln!("PIPELINE_LOG: CLIENT_GENERATE_END height={} timestamp={}",
                        global_db_height,
                        Utc::now().to_rfc3339()
                    );
                    vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_START height={} timestamp={}",
                        global_db_height,
                        Utc::now().to_rfc3339()
                    );
                    db_backend::create_kv(table_id, global_db_height, batch_tasks);
                    vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_END height={} timestamp={}",
                        global_db_height,
                        Utc::now().to_rfc3339()
                    );
                    global_db_height += 1;
                    batch_tasks = Vec::with_capacity(tasks_per_block); // 重置当前 Block
                    vprintln!("PIPELINE_LOG: CLIENT_GENERATE_START height={} timestamp={}",
                        global_db_height,
                        Utc::now().to_rfc3339()
                    );
                }
            }
        }
    }

    // --- 处理尾部残留的数据 (Flush Remaining) ---
    // 凡是没有凑够整齐配置的尾部数据，都需要在循环结束后强制打包落盘
    
    if !current_cset.op_list.is_empty() {
        current_cset.sort();
        current_cset_list.push(current_cset);
    }
    
    if !current_cset_list.is_empty() {
        let task = SimpleTask::new(current_cset_list);
        batch_tasks.push(parking_lot::RwLock::new(Some(task)));
    }
    
    vprintln!("PIPELINE_LOG: CLIENT_GENERATE_END height={} timestamp={}",
        global_db_height,
        Utc::now().to_rfc3339()
    );
    if !batch_tasks.is_empty() {
        vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_START height={} timestamp={}",
            global_db_height,
            Utc::now().to_rfc3339()
        );
        db_backend::create_kv(table_id, global_db_height, batch_tasks);
        vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_END height={} timestamp={}",
            global_db_height,
            Utc::now().to_rfc3339()
        );        
        global_db_height += 1;
    }

    db_backend::flush(table_id);
    println!("Total Entries Initialized: {}", total_init_entries);
    std::thread::sleep(Duration::from_secs(60));
    println!("Base State Population Complete.");

    // =========================================================
    // Phase 3: Replaying Operations (Benchmark) - 官方模式适配
    // =========================================================
    println!("\n=== Phase 2: Replaying Operations (Benchmark) ===");
    
    // === PRE-LOAD all replay ops into RAM before benchmark timer starts ===
    // Goal: Phase 3 main loop is bottlenecked by DB engine, not by SSD trace I/O.
    let rss_before_preload_kb = get_rss_kb();
    println!("
=== Pre-loading replay ops [{}, {}] into RAM ===", replay_start, replay_end);
    println!("  VmRSS before pre-load: {:.2} GB", rss_before_preload_kb as f64 / 1024.0 / 1024.0);
    let preload_start = Instant::now();
    let mut replay_blocks: Vec<(u64, Option<Vec<eth_replay_kv::Operation>>)> = Vec::new();
    {
        let (tx_pre, mut rx_pre) = tokio::sync::mpsc::channel(eth_replay_kv::PREFETCH_BUFFER_BLOCKS);
        let source_pre = source.clone();
        let client_pre = s3_client.clone();
        rt.spawn(async move {
            eth_replay_kv::run_ops_producer(replay_start, replay_end, source_pre, client_pre, tx_pre).await
        });
        rt.block_on(async {
            while let Some(msg) = rx_pre.recv().await {
                let (h, ops_opt) = msg.expect("Pre-load failed");
                replay_blocks.push((h, ops_opt));
            }
        });
    }
    let preload_dur = preload_start.elapsed();
    let est_ops_bytes: usize = replay_blocks.iter().map(|(_, v)| v.as_ref().map_or(0, |ops| ops.iter().map(|op| match op {
        eth_replay_kv::Operation::Create(k, val) | eth_replay_kv::Operation::Update(k, val) | eth_replay_kv::Operation::Read(k, val) => k.len() + val.len() + 24,
        eth_replay_kv::Operation::Delete(k) => k.len() + 24,
    }).sum::<usize>())).sum();
    let rss_after_preload_kb = get_rss_kb();
    let preload_delta_kb = rss_after_preload_kb.saturating_sub(rss_before_preload_kb);
    println!("Pre-loaded {} blocks in {:?}", replay_blocks.len(), preload_dur);
    println!("  Estimated ops payload: {:.2} GB", est_ops_bytes as f64 / 1024.0 / 1024.0 / 1024.0);
    println!("  Actual VmRSS after pre-load: {:.2} GB (delta {:.2} GB)", 
        rss_after_preload_kb as f64 / 1024.0 / 1024.0,
        preload_delta_kb as f64 / 1024.0 / 1024.0);

    // Drop OS page cache so Phase 3 SSD I/O reflects DB engine only.
    println!("Dropping OS page cache after pre-load...");
    let _ = std::process::Command::new("sync").status();
    let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");
    std::thread::sleep(Duration::from_secs(5));

    let bench_start = Instant::now();
    let total_blocks = replay_end - replay_start + 1;

    // --- 新增参数：N 个 ETH block 打包成 1 个 DB Task 批次 ---
    let eth_blocks_per_db_block = 32; 
    let ops_per_cset = 10;            
    let csets_per_task = 2;           

    // 2. 完全复刻官方 `hover_execute_op` 的消费者线程逻辑
    let (db_tx, db_rx) = std::sync::mpsc::sync_channel(10);
    let consumer_handle = std::thread::spawn(move || {
        while let Ok((blk_height, tasks)) = db_rx.recv() {
            vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_START height={} timestamp={}",
                blk_height,
                Utc::now().to_rfc3339()
            );
            db_backend::update_kv(table_id, blk_height, tasks);
            vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_END height={} timestamp={}",
                blk_height,
                Utc::now().to_rfc3339()
            );
        }
    });

    // 3. 主线程：负责解析 ETH 数据，并按 `run_single_hover_block` 的结构拼装
    let mut total_ops = 0;
    let mut processed_eth_blocks = 0;
    let mut interval_start = Instant::now();
    let mut interval_ops = 0;

    vprintln!("PIPELINE_LOG: CLIENT_GENERATE_START height={} timestamp={}",
        global_db_height,
        Utc::now().to_rfc3339()
    );

    rt.block_on(async {
        let mut eth_block_counter = 0;
        
        // 严格按照官方的三层结构预分配
        let mut task_list: Vec<parking_lot::RwLock<Option<SimpleTask>>> = Vec::new();
        let mut cset_list: Vec<ChangeSet> = Vec::with_capacity(csets_per_task);
        let mut current_cset = ChangeSet::new();
        let mut current_batch_keys: HashSet<Vec<u8>> = HashSet::new(); // 保留防冲突机制

        for (h, ops_opt) in replay_blocks.into_iter() {
            let current_boundary_ops = boundary_ops.get(&h);
            
            if ops_opt.is_none() && current_boundary_ops.is_none() { continue; }

            let mut process_op = |op: &eth_replay_kv::Operation| {
                let (op_code, key_ref, val_ref) = match op {
                    eth_replay_kv::Operation::Create(k, v) => (OP_CREATE, k, Some(v)),
                    eth_replay_kv::Operation::Update(k, v) => (OP_WRITE, k, Some(v)),
                    eth_replay_kv::Operation::Delete(k)    => (OP_DELETE, k, None),
                    eth_replay_kv::Operation::Read(k, v)   => (OP_READ, k, Some(v)), 
                };

                // 【切分逻辑 1】：防 Key 冲突 (真实 ETH 数据必须要有，否则会 Panic/错误)
                if current_batch_keys.contains(key_ref) {
                    current_cset.sort();
                    cset_list.push(std::mem::replace(&mut current_cset, ChangeSet::new()));
                    current_batch_keys.clear();
                    
                    if cset_list.len() >= csets_per_task {
                        let task = SimpleTask::new(std::mem::replace(&mut cset_list, Vec::with_capacity(csets_per_task)));
                        task_list.push(parking_lot::RwLock::new(Some(task)));
                    }
                }

                let kh = qmdb_common::entryfile::entry::entry_key_hash(key_ref);
                let val_slice = val_ref.map(|v| v.as_slice()).unwrap_or(&[]);
                current_cset.add_op(op_code, &kh, key_ref, val_slice, None);
                current_batch_keys.insert(key_ref.clone());
                
                total_ops += 1;
                interval_ops += 1;

                // 【切分逻辑 2】：正常达到 10 ops 的阈值
                if current_cset.op_list.len() >= ops_per_cset {
                    current_cset.sort();
                    cset_list.push(std::mem::replace(&mut current_cset, ChangeSet::new()));
                    current_batch_keys.clear();

                    if cset_list.len() >= csets_per_task {
                        let task = SimpleTask::new(std::mem::replace(&mut cset_list, Vec::with_capacity(csets_per_task)));
                        task_list.push(parking_lot::RwLock::new(Some(task)));
                    }
                }
            };

            // 执行操作装配
            if let Some(b_ops) = current_boundary_ops {
                for op in b_ops { process_op(op); }
            }
            if let Some(ops) = ops_opt {
                for op in &ops { process_op(op); }
            }

            eth_block_counter += 1;
            processed_eth_blocks += 1;

            // 【打包逻辑】：如果凑够了 N 个 ETH Block，则推入 DB 执行通道
            if eth_block_counter >= eth_blocks_per_db_block {
                // 清理并压入最后没装满的 cset 和 task
                if !current_cset.op_list.is_empty() {
                    current_cset.sort();
                    cset_list.push(std::mem::replace(&mut current_cset, ChangeSet::new()));
                    current_batch_keys.clear();
                }
                if !cset_list.is_empty() {
                    let task = SimpleTask::new(std::mem::replace(&mut cset_list, Vec::with_capacity(csets_per_task)));
                    task_list.push(parking_lot::RwLock::new(Some(task)));
                }

                if !task_list.is_empty() {
                    // 发送给官方的 consumer 线程处理
                    vprintln!("PIPELINE_LOG: CLIENT_GENERATE_END height={} timestamp={}",
                        global_db_height,
                        Utc::now().to_rfc3339()
                    );
                    db_tx.send((global_db_height as i64, std::mem::replace(&mut task_list, Vec::new()))).unwrap();
                    global_db_height += 1;
                    vprintln!("PIPELINE_LOG: CLIENT_GENERATE_START height={} timestamp={}",
                        global_db_height,
                        Utc::now().to_rfc3339()
                    );
                }
                eth_block_counter = 0;
            }

            // 保持原本的统计打印节奏
            if interval_start.elapsed().as_secs() >= 3 {
                let elapsed = interval_start.elapsed().as_secs_f64();
                let progress = (processed_eth_blocks as f64 / total_blocks as f64) * 100.0;
                println!("Replay [{:.1}%] Block {} | TPS: {:.0} ops/s", progress, h, interval_ops as f64 / elapsed);
                interval_start = Instant::now();
                interval_ops = 0;
            }
        }

        vprintln!("PIPELINE_LOG: CLIENT_GENERATE_END height={} timestamp={}",
            global_db_height,
            Utc::now().to_rfc3339()
        );
        // 循环结束：处理尾部残余数据并发送
        if !current_cset.op_list.is_empty() {
            current_cset.sort();
            cset_list.push(current_cset);
        }
        if !cset_list.is_empty() {
            task_list.push(parking_lot::RwLock::new(Some(SimpleTask::new(cset_list))));
        }
        if !task_list.is_empty() {
            db_tx.send((global_db_height as i64, task_list)).unwrap();
            global_db_height += 1;
        }
    });

    // 4. 等待队列处理完毕 (完全复刻官方行为)
    drop(db_tx); 
    consumer_handle.join().unwrap();

    let total_duration = bench_start.elapsed();
    println!("Benchmark Finished.");
    println!("Total Ops: {}", total_ops);
    println!("Duration: {:.2}s", total_duration.as_secs_f64());
    
    results.record_throughput_as_total("eth_replay_ops", total_ops, total_duration);
}

/// Streaming source for the ETH-replay base state.
///
/// Bundle mode reads the `InitDbBundle` field-by-field off ONE `BufReader` so
/// the ~60 GB `init_db` Vec is never materialized at once (Phase-1 peak RSS
/// ~1.3 GB instead of ~64 GB, which is what lets the replay run on a 62 GB box).
/// Trace mode just wraps the already-in-memory splitter output.
///
/// This works because bincode 1.3's free functions (`serialize`/
/// `deserialize_from`, used both by the writer that produced the bundle and
/// here) use a flat, fixint, u64-length-prefixed layout. So reading the fields
/// in struct-declaration order off the same reader —
///   populate_start:u64, populate_end:u64, init_db.len():u64,
///   then each (Vec<u8>,Vec<u8>) entry, then boundary_ops.len():u64,
///   then each (u64, Vec<Operation>) pair —
/// is byte-identical to a single `deserialize_from::<InitDbBundle>`.
enum InitDbSource {
    Mem {
        iter: Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)>>,
        boundary: std::collections::HashMap<u64, Vec<eth_replay_kv::Operation>>,
    },
    Stream {
        r: std::io::BufReader<std::fs::File>,
        remaining: u64,
    },
}

impl InitDbSource {
    /// Next base-state (key, value), or None once the init_db section is done.
    fn next_entry(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
        match self {
            InitDbSource::Mem { iter, .. } => iter.next(),
            InitDbSource::Stream { r, remaining } => {
                if *remaining == 0 {
                    return None;
                }
                let kv: (Vec<u8>, Vec<u8>) = bincode::deserialize_from(&mut *r)
                    .expect("bincode deserialize init_db entry");
                *remaining -= 1;
                Some(kv)
            }
        }
    }

    /// Consume the source and return the boundary_ops restricted to
    /// [replay_start, replay_end]. In stream mode this reads the trailing
    /// HashMap off the same reader, so it MUST be called only after
    /// `next_entry` has returned None (init_db fully drained); in mem mode it
    /// just filters the in-memory map.
    fn finish_boundary_ops(
        self,
        replay_start: u64,
        replay_end: u64,
    ) -> std::collections::HashMap<u64, Vec<eth_replay_kv::Operation>> {
        match self {
            InitDbSource::Mem { mut boundary, .. } => {
                boundary.retain(|h, _| *h >= replay_start && *h <= replay_end);
                boundary
            }
            InitDbSource::Stream { mut r, remaining } => {
                assert_eq!(remaining, 0,
                    "finish_boundary_ops called before init_db was fully drained");
                let n: u64 = bincode::deserialize_from(&mut r)
                    .expect("bincode deserialize boundary_ops len");
                let mut map = std::collections::HashMap::new();
                for _ in 0..n {
                    let (h, ops): (u64, Vec<eth_replay_kv::Operation>) =
                        bincode::deserialize_from(&mut r)
                            .expect("bincode deserialize boundary_ops entry");
                    if h >= replay_start && h <= replay_end {
                        map.insert(h, ops);
                    }
                }
                map
            }
        }
    }
}

fn run_eth_replay_from_bundle(
    table_id: usize,
    bundle_path: &str,
    trace_dir: &str,
    replay_start: u64,
    replay_end: u64,
    results: &mut BenchmarkResults,
) {
    run_eth_replay_inner(
        table_id, Some(trace_dir), Some(bundle_path),
        0, 0, replay_start, replay_end, results,
    );
}

fn run_eth_replay_inner(
    table_id: usize,
    input_dir: Option<&str>,
    bundle_path: Option<&str>,
    populate_start: u64,
    populate_end: u64,
    replay_start: u64,
    replay_end: u64,
    results: &mut BenchmarkResults,
) {
    let start_height = populate_start;
    let end_height = populate_end;
    println!("Running Real Ethereum Workload");
    if let Some(p) = bundle_path { println!("Mode: bundle  path={}", p); }
    else { println!("Mode: trace  Populate Range: Block {} to {}", populate_start, populate_end); }
    println!("Replay   Range: Block {} to {}", replay_start, replay_end);

    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
    let source = input_dir.map(eth_replay_kv::parse_storage_path);
    let s3_client = if source.as_ref().map_or(false, |s| s.is_s3()) {
        Some(rt.block_on(eth_replay_kv::new_s3_client()))
    } else {
        None
    };

    let mut global_db_height: i64 = 1;

    // =========================================================
    // Phase 1: produce (init_db, boundary_ops) — either from bundle or from trace
    // =========================================================
    let mut src: InitDbSource = if let Some(bp) = bundle_path {
        println!("\n=== Phase 1: Streaming InitDbBundle from {} ===", bp);
        let t0 = std::time::Instant::now();
        let f = std::fs::File::open(bp).expect("open bundle file");
        let mut r = std::io::BufReader::with_capacity(64 * 1024 * 1024, f);
        // Read only the bundle header off the reader, in struct-declaration
        // order: populate_start, populate_end, then the init_db Vec length.
        // The init_db entries and the trailing boundary_ops map stay on disk
        // and are pulled lazily (next_entry / finish_boundary_ops).
        let bundle_populate_start: u64 = bincode::deserialize_from(&mut r)
            .expect("bincode deserialize populate_start");
        let bundle_populate_end: u64 = bincode::deserialize_from(&mut r)
            .expect("bincode deserialize populate_end");
        let n_init: u64 = bincode::deserialize_from(&mut r)
            .expect("bincode deserialize init_db len");
        println!("Bundle header read in {:.1}s  populate=[{},{}]  init_db={} (streaming)",
            t0.elapsed().as_secs_f64(),
            bundle_populate_start, bundle_populate_end, n_init);
        InitDbSource::Stream { r, remaining: n_init }
    } else {
        println!("\n=== Phase 1: Populating Base State (from trace) ===");
        let source = source.clone().expect("source required when bundle not given");
        let (tx_p1, mut rx_p1) = tokio::sync::mpsc::channel(eth_replay_kv::PREFETCH_BUFFER_BLOCKS);
        let source_clone_p1 = source.clone();
        let client_clone_p1 = s3_client.clone();
        rt.spawn(async move {
            eth_replay_kv::run_block_data_producer(start_height, end_height, source_clone_p1, client_clone_p1, tx_p1).await
        });
        let mut splitter = eth_replay_kv::BaseStateSplitter::new();
        rt.block_on(async {
            while let Some(msg) = rx_p1.recv().await {
                let (height, base_kv_opt, ops_opt) = msg.expect("Failed to receive from producer");
                if let Some(b) = base_kv_opt {
                    splitter.ingest_base_kv(height, b);
                }
                if let Some(o) = ops_opt {
                    splitter.apply_ops(o);
                }
            }
        });
        let (hm, bops) = splitter.into_parts();
        InitDbSource::Mem { iter: Box::new(hm.into_iter()), boundary: bops }
    };

    // =========================================================
    // Phase 2: 将init db内存版本populate到DB (streaming: pull one entry at a time
    // via src.next_entry() so the full init_db Vec is never held in memory)
    // =========================================================
    let mut total_init_entries = 0;
    let ops_per_block = 100000;
    let ops_per_cset = 10;
    let csets_per_task = 2;
    let tasks_per_block = ops_per_block / (ops_per_cset * csets_per_task);

    let mut current_cset = ChangeSet::new();
    let mut current_cset_list: Vec<ChangeSet> = Vec::with_capacity(csets_per_task);
    let mut batch_tasks: Vec<parking_lot::RwLock<Option<SimpleTask>>> = Vec::with_capacity(tasks_per_block);

    while let Some((k, v)) = src.next_entry() {
        if v.is_empty() {
            println!("Empty value for key: {:?}", k);
            continue;
        }

        let kh = qmdb_common::entryfile::entry::entry_key_hash(&k);
        current_cset.add_op(OP_CREATE, &kh, &k, &v, None);
        total_init_entries += 1;

        // 1. 检查 ChangeSet 是否已满 (达到 10 ops)
        if current_cset.op_list.len() >= ops_per_cset {
            current_cset.sort();
            current_cset_list.push(current_cset);
            current_cset = ChangeSet::new(); // 重置当前 ChangeSet

            // 2. 检查 Task 是否已满 (达到 2 csets)
            if current_cset_list.len() >= csets_per_task {
                let task = SimpleTask::new(current_cset_list);
                batch_tasks.push(parking_lot::RwLock::new(Some(task)));
                current_cset_list = Vec::with_capacity(csets_per_task); // 重置当前 Task 的 cset 列表

                // 3. 检查 Block 是否已满 (达到 5000 tasks，即 100,000 ops)
                if batch_tasks.len() >= tasks_per_block {
                    vprintln!("PIPELINE_LOG: CLIENT_GENERATE_END height={} timestamp={}",
                        global_db_height,
                        Utc::now().to_rfc3339()
                    );
                    vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_START height={} timestamp={}",
                        global_db_height,
                        Utc::now().to_rfc3339()
                    );
                    db_backend::create_kv(table_id, global_db_height, batch_tasks);
                    vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_END height={} timestamp={}",
                        global_db_height,
                        Utc::now().to_rfc3339()
                    );
                    global_db_height += 1;
                    batch_tasks = Vec::with_capacity(tasks_per_block); // 重置当前 Block
                    vprintln!("PIPELINE_LOG: CLIENT_GENERATE_START height={} timestamp={}",
                        global_db_height,
                        Utc::now().to_rfc3339()
                    );
                }
            }
        }
    }

    // --- 处理尾部残留的数据 (Flush Remaining) ---
    // 凡是没有凑够整齐配置的尾部数据，都需要在循环结束后强制打包落盘
    
    if !current_cset.op_list.is_empty() {
        current_cset.sort();
        current_cset_list.push(current_cset);
    }
    
    if !current_cset_list.is_empty() {
        let task = SimpleTask::new(current_cset_list);
        batch_tasks.push(parking_lot::RwLock::new(Some(task)));
    }
    
    vprintln!("PIPELINE_LOG: CLIENT_GENERATE_END height={} timestamp={}",
        global_db_height,
        Utc::now().to_rfc3339()
    );
    if !batch_tasks.is_empty() {
        vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_START height={} timestamp={}",
            global_db_height,
            Utc::now().to_rfc3339()
        );
        db_backend::create_kv(table_id, global_db_height, batch_tasks);
        vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_END height={} timestamp={}",
            global_db_height,
            Utc::now().to_rfc3339()
        );        
        global_db_height += 1;
    }

    db_backend::flush(table_id);
    println!("Total Entries Initialized: {}", total_init_entries);
    std::thread::sleep(Duration::from_secs(60));
    println!("Base State Population Complete.");

    // Consume the source's boundary_ops, keeping only blocks in the replay
    // range. In stream mode this reads the trailing boundary_ops HashMap off
    // the same bundle reader (init_db is now fully drained) and filters as it
    // reads, so the ~5M-block full map is never materialized; in trace/mem mode
    // it just filters the in-memory map. We only access boundary_ops via
    // .get(&h) for h in [replay_start, replay_end], so dropping the rest frees
    // tens of GB on limited-RAM machines.
    let boundary_ops = src.finish_boundary_ops(replay_start, replay_end);
    println!("boundary_ops in replay range [{}, {}]: {} blocks retained",
        replay_start, replay_end, boundary_ops.len());

    // =========================================================
    // Phase 3: Replaying Operations (Benchmark) - 官方模式适配
    // =========================================================
    println!("\n=== Phase 2: Replaying Operations (Benchmark) ===");
    
    // === STREAMING mode: 50-block bounded channel ===
    // Memory bounded ~25 MB (50 blocks × ~0.5 MB), supports arbitrary replay range.
    // Producer runs ahead in background, main loop drains from channel.
    let rss_before_stream_kb = get_rss_kb();
    println!("\n=== Streaming replay ops [{}, {}] (queue=50 blocks) ===", replay_start, replay_end);
    println!("  VmRSS before streaming: {:.2} GB", rss_before_stream_kb as f64 / 1024.0 / 1024.0);
    let source_stream = source.clone().expect("eth-trace-dir is required for Phase 3 replay");
    let (tx_stream, mut rx_stream) = tokio::sync::mpsc::channel(50);
    let source_pre = source_stream.clone();
    let client_pre = s3_client.clone();
    rt.spawn(async move {
        eth_replay_kv::run_ops_producer(replay_start, replay_end, source_pre, client_pre, tx_stream).await
    });

    // Drop OS page cache so Phase 3 SSD I/O is cold for QMDB engine.
    println!("Dropping OS page cache before bench...");
    let _ = std::process::Command::new("sync").status();
    let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");
    std::thread::sleep(Duration::from_secs(5));

    let bench_start = Instant::now();
    let total_blocks = replay_end - replay_start + 1;

    // --- 新增参数：N 个 ETH block 打包成 1 个 DB Task 批次 ---
    let eth_blocks_per_db_block = 32; 
    let ops_per_cset = 10;            
    let csets_per_task = 2;           

    // 2. 完全复刻官方 `hover_execute_op` 的消费者线程逻辑
    let (db_tx, db_rx) = std::sync::mpsc::sync_channel(10);
    let consumer_handle = std::thread::spawn(move || {
        while let Ok((blk_height, tasks)) = db_rx.recv() {
            vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_START height={} timestamp={}",
                blk_height,
                Utc::now().to_rfc3339()
            );
            db_backend::update_kv(table_id, blk_height, tasks);
            vprintln!("PIPELINE_LOG: CLIENT_SUBMIT_END height={} timestamp={}",
                blk_height,
                Utc::now().to_rfc3339()
            );
        }
    });

    // 3. 主线程：负责解析 ETH 数据，并按 `run_single_hover_block` 的结构拼装
    let mut total_ops = 0;
    let mut processed_eth_blocks = 0;
    let mut interval_start = Instant::now();
    let mut interval_ops = 0;

    vprintln!("PIPELINE_LOG: CLIENT_GENERATE_START height={} timestamp={}",
        global_db_height,
        Utc::now().to_rfc3339()
    );

    rt.block_on(async {
        let mut eth_block_counter = 0;
        
        // 严格按照官方的三层结构预分配
        let mut task_list: Vec<parking_lot::RwLock<Option<SimpleTask>>> = Vec::new();
        let mut cset_list: Vec<ChangeSet> = Vec::with_capacity(csets_per_task);
        let mut current_cset = ChangeSet::new();
        let mut current_batch_keys: HashSet<Vec<u8>> = HashSet::new(); // 保留防冲突机制

        while let Some(msg) = rx_stream.recv().await {
            let (h, ops_opt) = msg.expect("streaming read failed");
            let current_boundary_ops = boundary_ops.get(&h);
            
            if ops_opt.is_none() && current_boundary_ops.is_none() { continue; }

            let mut process_op = |op: &eth_replay_kv::Operation| {
                let (op_code, key_ref, val_ref) = match op {
                    eth_replay_kv::Operation::Create(k, v) => (OP_CREATE, k, Some(v)),
                    eth_replay_kv::Operation::Update(k, v) => (OP_WRITE, k, Some(v)),
                    eth_replay_kv::Operation::Delete(k)    => (OP_DELETE, k, None),
                    eth_replay_kv::Operation::Read(k, v)   => (OP_READ, k, Some(v)), 
                };

                // 【切分逻辑 1】：防 Key 冲突 (真实 ETH 数据必须要有，否则会 Panic/错误)
                if current_batch_keys.contains(key_ref) {
                    current_cset.sort();
                    cset_list.push(std::mem::replace(&mut current_cset, ChangeSet::new()));
                    current_batch_keys.clear();
                    
                    if cset_list.len() >= csets_per_task {
                        let task = SimpleTask::new(std::mem::replace(&mut cset_list, Vec::with_capacity(csets_per_task)));
                        task_list.push(parking_lot::RwLock::new(Some(task)));
                    }
                }

                let kh = qmdb_common::entryfile::entry::entry_key_hash(key_ref);
                let val_slice = val_ref.map(|v| v.as_slice()).unwrap_or(&[]);
                current_cset.add_op(op_code, &kh, key_ref, val_slice, None);
                current_batch_keys.insert(key_ref.clone());
                
                total_ops += 1;
                interval_ops += 1;

                // 【切分逻辑 2】：正常达到 10 ops 的阈值
                if current_cset.op_list.len() >= ops_per_cset {
                    current_cset.sort();
                    cset_list.push(std::mem::replace(&mut current_cset, ChangeSet::new()));
                    current_batch_keys.clear();

                    if cset_list.len() >= csets_per_task {
                        let task = SimpleTask::new(std::mem::replace(&mut cset_list, Vec::with_capacity(csets_per_task)));
                        task_list.push(parking_lot::RwLock::new(Some(task)));
                    }
                }
            };

            // 执行操作装配
            if let Some(b_ops) = current_boundary_ops {
                for op in b_ops { process_op(op); }
            }
            if let Some(ops) = ops_opt {
                for op in &ops { process_op(op); }
            }

            eth_block_counter += 1;
            processed_eth_blocks += 1;

            // 【打包逻辑】：如果凑够了 N 个 ETH Block，则推入 DB 执行通道
            if eth_block_counter >= eth_blocks_per_db_block {
                // 清理并压入最后没装满的 cset 和 task
                if !current_cset.op_list.is_empty() {
                    current_cset.sort();
                    cset_list.push(std::mem::replace(&mut current_cset, ChangeSet::new()));
                    current_batch_keys.clear();
                }
                if !cset_list.is_empty() {
                    let task = SimpleTask::new(std::mem::replace(&mut cset_list, Vec::with_capacity(csets_per_task)));
                    task_list.push(parking_lot::RwLock::new(Some(task)));
                }

                if !task_list.is_empty() {
                    // 发送给官方的 consumer 线程处理
                    vprintln!("PIPELINE_LOG: CLIENT_GENERATE_END height={} timestamp={}",
                        global_db_height,
                        Utc::now().to_rfc3339()
                    );
                    db_tx.send((global_db_height as i64, std::mem::replace(&mut task_list, Vec::new()))).unwrap();
                    global_db_height += 1;
                    vprintln!("PIPELINE_LOG: CLIENT_GENERATE_START height={} timestamp={}",
                        global_db_height,
                        Utc::now().to_rfc3339()
                    );
                }
                eth_block_counter = 0;
            }

            // 保持原本的统计打印节奏
            if interval_start.elapsed().as_secs() >= 3 {
                let elapsed = interval_start.elapsed().as_secs_f64();
                let progress = (processed_eth_blocks as f64 / total_blocks as f64) * 100.0;
                println!("Replay [{:.1}%] Block {} | TPS: {:.0} ops/s", progress, h, interval_ops as f64 / elapsed);
                interval_start = Instant::now();
                interval_ops = 0;
            }
        }

        vprintln!("PIPELINE_LOG: CLIENT_GENERATE_END height={} timestamp={}",
            global_db_height,
            Utc::now().to_rfc3339()
        );
        // 循环结束：处理尾部残余数据并发送
        if !current_cset.op_list.is_empty() {
            current_cset.sort();
            cset_list.push(current_cset);
        }
        if !cset_list.is_empty() {
            task_list.push(parking_lot::RwLock::new(Some(SimpleTask::new(cset_list))));
        }
        if !task_list.is_empty() {
            db_tx.send((global_db_height as i64, task_list)).unwrap();
            global_db_height += 1;
        }
    });

    // 4. 等待队列处理完毕 (完全复刻官方行为)
    drop(db_tx); 
    consumer_handle.join().unwrap();

    let total_duration = bench_start.elapsed();
    println!("Benchmark Finished.");
    println!("Total Ops: {}", total_ops);
    println!("Duration: {:.2}s", total_duration.as_secs_f64());
    
    results.record_throughput_as_total("eth_replay_ops", total_ops, total_duration);
}

// ####################################################################
// ############################## Tools ###############################
// ####################################################################
fn rand_between_seg_idx(
    randsrc: &mut RandSrc,
    min: usize,
    max: usize,
    n: usize,
    seg_index: usize,
) -> usize {
    let range = max - min;
    let step = range / n;

    let seg_start = min + seg_index * step;
    let seg_end = if seg_index + 1 == n {
        max
    } else {
        seg_start + step
    };

    if seg_end - seg_start > 1 {
        rand_between(randsrc, seg_start, seg_end)
    } else {
        seg_start
    }
}

#[inline]
fn rand_between(randsrc: &mut RandSrc, min: usize, max: usize) -> usize {
    let span = max - min;
    min + randsrc.get_uint32() as usize % span
}

fn get_rss_kb() -> u64 {
    if let Ok(content) = fs::read_to_string("/proc/self/status") {
        for line in content.lines() {
            if line.starts_with("VmRSS:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    return parts[1].parse::<u64>().unwrap_or(0);
                }
            }
        }
    }
    0
}

fn get_linux_disk_rw_sectors(disk_name: &str) -> io::Result<(u64, u64)> {
    let content = fs::read_to_string("/proc/diskstats")?;
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() > 9 { // 确保有足够字段
            if parts[2] == disk_name {
                let read_sectors = parts[5].parse::<u64>().unwrap_or(0);
                // 第10个字段 (parts[9]) 通常是 sectors written
                let write_sectors = parts[9].parse::<u64>().unwrap_or(0);
                return Ok((read_sectors, write_sectors));
            }
        }
    }
    Ok((0, 0))
}

fn dir_size(path: &Path) -> u64 {
    let mut size = 0;

    if path.is_file() {
        if let Ok(metadata) = fs::metadata(path) {
            size += metadata.len();
        }
    } else if path.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let path = entry.path();
                size += dir_size(&path);
            }
        }
    }

    size
}

fn print_unified_report(
    phase: &str, ops_count: u64,
    start_os_read_sectors: u64, 
    end_os_read_sectors: u64,
    start_os_write_sectors: u64, 
    end_os_write_sectors: u64,
) {
    const LINUX_SECTOR_SIZE: u64 = 512;
    const KEY_SIZE: u64 = 20;
    const VALUE_SIZE: u64 = 32;
    const KV_PAIR_SIZE: u64 = KEY_SIZE + VALUE_SIZE;

    let total_kv_size = ops_count * KV_PAIR_SIZE;
    let os_read_bytes = (end_os_read_sectors.saturating_sub(start_os_read_sectors)) * LINUX_SECTOR_SIZE;
    let os_write_bytes = (end_os_write_sectors.saturating_sub(start_os_write_sectors)) * LINUX_SECTOR_SIZE;

    result_log::write_line("----------------------------------------------------------------");
    result_log::write_line(&format!("TEST_REPORT: Phase={} | Ops={}", phase, ops_count));
    result_log::write_line(&format!("  > Total KV Size (Pure)  = {} bytes ({:.2} MB)", total_kv_size, total_kv_size as f64 / 1024.0 / 1024.0));
    result_log::write_line("  [OS-level Disk IO]");
    result_log::write_line(&format!("  > OS Physical Write = {} bytes ({:.2} MB)", os_write_bytes, os_write_bytes as f64 / 1024.0 / 1024.0));
    result_log::write_line(&format!("  > OS Physical Read  = {} bytes ({:.2} MB)", os_read_bytes, os_read_bytes as f64 / 1024.0 / 1024.0));
    result_log::write_line("----------------------------------------------------------------");
}

pub fn log_io_stat(
    _height: i64, _last_snapshot_time: &mut Instant,
    _last_user_bytes: &mut u64, _last_gc_bytes: &mut u64,
) {
    //
}


#[cfg(all(not(feature="use_mdbx"), not(feature="use_rocksdb"), not(feature="use_reth"), not(feature="use_nomt"), not(feature="use_lvmt")))]
fn spawn_qmdb_monitor(db_dir: String) {
    let table_id_for_monitor = 0; 
    thread::spawn(move || {
        loop {
            if let Some((metadb_arc, indexer_arc)) = db_backend::get_db_handles(table_id_for_monitor) {
                let metadb = metadb_arc.read(); 
                let mut total_garbage_entries: u64 = 0;
                let mut total_active_entries: u64 = 0;
                let mut total_on_disk_entries: u64 = 0;
                let mut total_disk_size_bytes: u64 = 0;
                let mut estimated_garbage_size_bytes: f64 = 0.0;

                for shard_id in 0..SHARD_COUNT {
                    let sn_start = metadb.get_oldest_active_sn(shard_id);
                    let sn_end = metadb.get_next_serial_num(shard_id);
                    let active_entries = indexer_arc.len(shard_id) as u64;
                    let file_size = metadb.get_entry_file_size(shard_id);

                    let shard_total_entries = if sn_end > sn_start { sn_end - sn_start } else { 0 };
                    let shard_garbage_entries = if shard_total_entries > active_entries { shard_total_entries - active_entries } else { 0 };
                    
                    let garbage_ratio = if shard_total_entries > 0 {
                        shard_garbage_entries as f64 / shard_total_entries as f64
                    } else {
                        0.0
                    };

                    total_garbage_entries += shard_garbage_entries;
                    total_active_entries += active_entries;
                    total_on_disk_entries += shard_total_entries;
                    total_disk_size_bytes += file_size as u64;
                    estimated_garbage_size_bytes += (file_size as f64) * garbage_ratio;
                }
                drop(metadb); 

                // ===== 获取物理磁盘真实使用量 =====
                let mut physical_used_bytes: u64 = 0;
                let mut physical_total_bytes: u64 = 0;

                unsafe {
                    let path = CString::new(db_dir.as_str()).unwrap_or_default();
                    let mut stats: libc::statvfs = mem::zeroed();
                    if libc::statvfs(path.as_ptr(), &mut stats) == 0 {
                        let block_size = stats.f_frsize as u64;
                        let total_blocks = stats.f_blocks as u64;
                        let free_blocks = stats.f_bfree as u64;
                        physical_total_bytes = total_blocks * block_size;
                        physical_used_bytes = (total_blocks - free_blocks) * block_size;
                    } else {
                        warn!("Failed to get disk stats for {}", db_dir);
                    }
                }

                // println!(
                //     "GC_SNAPSHOT: Timestamp={} TotalEntries(Disk)={} ActiveEntries(Mem)={} GarbageEntries={} TotalDiskSizeMB={:.2} EstGarbageSizeMB={:.2} PhysicalDiskUsedMB={:.2} PhysicalDiskTotalMB={:.2}",
                //     Utc::now().to_rfc3339(),
                //     total_on_disk_entries,
                //     total_active_entries,
                //     total_garbage_entries,
                //     total_disk_size_bytes as f64 / (1024.0 * 1024.0),
                //     estimated_garbage_size_bytes / (1024.0 * 1024.0),
                //     physical_used_bytes as f64 / (1024.0 * 1024.0),
                //     physical_total_bytes as f64 / (1024.0 * 1024.0)
                // );
            } else {
                // println!("GC_SNAPSHOT: 等待数据库句柄...");
            }
            thread::sleep(Duration::from_secs(5));
        }
    });
}

#[cfg(any(feature="use_mdbx", feature="use_rocksdb", feature="use_reth", feature="use_nomt", feature="use_lvmt"))]
fn spawn_qmdb_monitor(_: String) {
    // 其他数据库不需要此监控，函数体留空
}

// ####################################################################
// ########################## 暂时未使用的code ##########################
// ####################################################################

/// Perform lightweight correctness sampling during hover reads.
///
/// This is only enabled for the QMDB backend (no `use_mdbx` / `use_rocksdb`)
/// and when the `qmdb_bench_correctness` feature is active.
#[cfg(all(
    feature = "qmdb_bench_correctness",
    not(feature = "use_mdbx"),
    not(feature = "use_rocksdb"),
    not(feature = "use_reth"),
    not(feature = "use_lvmt")
))]
fn run_hover_correctness_sampling(
    table_id: usize,
    max_num: u64,
    _test_gen: &mut TestGenV2,
    model: &BitmapModel,
    randsrc: &mut RandSrc,
    results: &mut BenchmarkResults,
) {
    // Limit the number of samples per hover invocation to keep overhead low.
    const MAX_SAMPLES: usize = 128;

    let mut key_list: Vec<[u8; 20]> = Vec::new();
    let mut expected_values: Vec<[u8; 32]> = Vec::new();
    let mut expected_exists: Vec<bool> = Vec::new();
    let mut expected_rounds: Vec<u32> = Vec::new();

    // We sample from the "interesting" ranges:
    // - Stable range: [0, entry_count_wo_del), which should always exist.
    // - Deletable range: only indices that are still marked as existing.
    //
    // New high-numbered entries are currently not tracked by the bitmap model
    // and are therefore skipped in this sampling routine.

    let entry_count_wo_del = model.entry_count_wo_del;

    // If we haven't yet populated the full stable range, skip sampling to
    // avoid treating not-yet-created keys as existing ones.
    if max_num < entry_count_wo_del {
        return;
    }

    // Helper closure: try to add one sample for a given logical `num`.
    let mut try_add_sample = |num: u64| {
        if key_list.len() >= MAX_SAMPLES {
            return;
        }

        // Determine existence state according to the bitmap model:
        //   Some(true)  => key should exist
        //   Some(false) => key should have been deleted
        //   None        => key is outside tracked ranges; skip
        let exists_state = model.existence_state(num);
        let Some(should_exist) = exists_state else {
            return;
        };

        // Decide expected "round" for the value when it should exist:
        // - Stable range: round 0 if never updated, 1 if updated at least once.
        // - Deletable range: values are created in round 0 and never updated.
        let expected_round: u32 = if num < entry_count_wo_del {
            if model.is_updated(num) {
                1
            } else {
                0
            }
        } else {
            0
        };

        let (k, v) = build_key_and_expected_value(num, expected_round);
        key_list.push(k);
        expected_values.push(v);
        expected_exists.push(should_exist);
        expected_rounds.push(expected_round);
    };

    // 1) Sample from the stable range.
    if entry_count_wo_del > 0 {
        let samples = (MAX_SAMPLES / 2).max(1) as u64;
        for _ in 0..samples {
            let num = rand_between(randsrc, 0, entry_count_wo_del as usize) as u64;
            try_add_sample(num);
        }
    }

    // 2) Sample from the deletable range (only if any entries are tracked there).
    if model.delete_count > 0 {
        let delete_span = model.delete_count as usize;
        let samples = (MAX_SAMPLES / 2).max(1) as u64;
        for _ in 0..samples {
            let offset = rand_between(randsrc, 0, delete_span) as u64;
            let num = entry_count_wo_del + offset;
            try_add_sample(num);
        }
    }

    if key_list.is_empty() {
        return;
    }

    // Perform the actual reads from QMDB and compare with expected values.
    let actual_values = qmdb::read_kv_values(table_id, &key_list);
    let mut mismatches = 0u64;

    for (idx, actual_opt) in actual_values.into_iter().enumerate() {
        let expected = &expected_values[idx];
        let should_exist = expected_exists[idx];
        let expected_round = expected_rounds[idx];
        match (should_exist, actual_opt) {
            // Expected to exist but missing in DB.
            (true, None) => {
                mismatches += 1;
                eprintln!(
                    "QMDB correctness: missing key for sampled num={} (table_id={})",
                    {
                        let mut buf = [0u8; 8];
                        buf.copy_from_slice(&key_list[idx][12..20]);
                        u64::from_be_bytes(buf)
                    },
                    table_id
                );
            }
            // Expected to exist, and we got a value – compare contents.
            (true, Some(actual)) => {
                let entry = EntryBz { bz: &actual };
                // Check if actual bytes are enough to be a valid entry or at least parse the header
                // We trust QMDB returns valid entries, but let's be safe against empty/garbage
                if actual.len() < 5 {
                    mismatches += 1;
                    eprintln!("QMDB correctness: read invalid entry (too short) for sampled idx");
                    continue;
                }
                let actual_val = entry.value();

                if actual_val != expected.as_slice() {
                    mismatches += 1;
                    let num = {
                        let mut buf = [0u8; 8];
                        buf.copy_from_slice(&key_list[idx][12..20]);
                        u64::from_be_bytes(buf)
                    };
                    eprintln!(
                        "QMDB correctness: value mismatch for sampled num={} (table_id={})",
                        num, table_id
                    );
                    eprintln!("  Expected: {}", hex::encode(expected));
                    eprintln!("  Actual:   {}", hex::encode(actual_val));
                    eprintln!("  Expected Round: {}", expected_round);
                    eprintln!("  Updated in Model: {}", model.is_updated(num));
                }
            }
            // Expected to be deleted, but DB still returns a value.
            (false, Some(_actual)) => {
                mismatches += 1;
                eprintln!(
                    "QMDB correctness: key should have been deleted for sampled num={} (table_id={})",
                    {
                        let mut buf = [0u8; 8];
                        buf.copy_from_slice(&key_list[idx][12..20]);
                        u64::from_be_bytes(buf)
                    },
                    table_id
                );
            }
            // Expected to be deleted and DB returns None: OK.
            (false, None) => {}
        }
    }

    if mismatches > 0 {
        // For now we just print and record a summary entry. This can be
        // extended later to keep detailed error samples in `BenchmarkResults`.
        println!(
            "QMDB correctness: {} mismatches detected in {} sampled reads",
            mismatches,
            key_list.len()
        );
        results
            .summary
            .entry("correctness.hover_mismatches".to_string())
            .and_modify(|v| *v += mismatches as f64)
            .or_insert(mismatches as f64);
    }
}

#[allow(dead_code)]
#[inline]
fn generate_read_key(randsrc: &mut RandSrc, max_num: u64) -> [u8; 20] {
    let num = rand_between(randsrc, 0, max_num as usize);
    let mut k = [0u8; 20];
    BigEndian::write_u64(&mut k[12..20], num as u64);
    let hash = qmdb_common::entryfile::entry::entry_key_hash(&k[12..20]);
    for i in 0..12 {
        k[i] = hash[i] ^ hash[12 + i];
    }
    k
}

// fn do_hover_read(
//     table_id: usize,
//     _hover_read_count: u64,
//     max_num: u64,
//     randsrc: &mut RandSrc,
//     results: &mut BenchmarkResults,
//     _read_batch: u64,
//     num_read_latency_samples: u64,
// ) {
//     db_backend::flush(table_id);

//     // Benchmark read throughput
//     let bm_start = Instant::now();
//     let mut key_list = Vec::with_capacity(read_batch as usize);

//     for i in 0..hover_read_count {
//         thread::scope(|s| {
//             // First: Add the new key to the list
//             key_list.push(generate_read_key(randsrc, max_num));

//             // Then: Process the batch if we've hit the batch size or it's the last iteration
//             if i % read_batch == read_batch - 1 || i == hover_read_count - 1 {
//                 let mut list = Vec::with_capacity(read_batch as usize);
//                 std::mem::swap(&mut list, &mut key_list);
//                 if i != hover_read_count - 1 {
//                     println!("\tBenchmarking reads {}", i + 1);
//                 }
//                 s.spawn(move || db_backend::read_kv(table_id, &list));
//             }
//         });
//     }

//     results.record_throughput("reads", hover_read_count, bm_start.elapsed());
// }

// fn measure_read_latency(
//     table_id: usize,
//     max_num: u64,
//     randsrc: &mut RandSrc,
//     results: &mut BenchmarkResults,
//     num_read_latency_samples: u64,
// ) {
//     db_backend::flush(table_id);
//     print!("Benchmarking read latency...");
//     let latency = results.latencies.get_mut("reads").unwrap();
//     for _ in 0..num_read_latency_samples {
//         let k = generate_read_key(randsrc, max_num);
//         let start = Instant::now();
//         db_backend::read_kv(table_id, &vec![k]);
//         let duration = start.elapsed();
//         latency.record(duration);
//     }

//     if let Some(stats) = latency.detailed_stats() {
//         stats.print("Read", true);
//     }
// }

// // 核心执行逻辑：按顺序解析 Operation，统一打包成 ChangeSet 交给 update_kv
// fn apply_operations(
//     table_id: usize, 
//     height: u64, 
//     ops: Vec<eth_replay_kv::Operation>, 
//     boundary_ops: Option<&Vec<eth_replay_kv::Operation>>
// ) -> u64 {
//     let mut total_ops_count = ops.len() as u64;
//     if let Some(b_ops) = boundary_ops {
//         total_ops_count += b_ops.len() as u64;
//     }

//     if total_ops_count == 0 { return 0; }

//     let mut final_change_sets = Vec::new();
//     let mut current_cset = ChangeSet::new();
//     let mut current_batch_keys: HashSet<Vec<u8>> = HashSet::new();

//     // 内部闭包：处理单个 Operation，将其转化为底层 op_code
//     let mut process_op = |op: &eth_replay_kv::Operation| {
//         let (op_code, key_ref, val_ref) = match op {
//             eth_replay_kv::Operation::Create(k, v) => (OP_CREATE, k, Some(v)),
//             eth_replay_kv::Operation::Update(k, v) => (OP_WRITE, k, Some(v)),
//             eth_replay_kv::Operation::Delete(k)    => (OP_DELETE, k, None),
//             eth_replay_kv::Operation::Read(k, v)   => (OP_READ, k, Some(v)), 
//         };

//         // 如果遇到同一个区块中对同一个 Key 操作多次，触发分片换一个新的 ChangeSet
//         if current_batch_keys.contains(key_ref) {
//             current_cset.sort();
            
//             // 【修复关键点】：使用 std::mem::replace 替换值，避免 Move 所有权
//             let completed_cset = std::mem::replace(&mut current_cset, ChangeSet::new());
//             final_change_sets.push(completed_cset);
            
//             current_batch_keys.clear();
//         }

//         let kh = qmdb_common::entryfile::entry::entry_key_hash(key_ref);
//         let val_slice = val_ref.map(|v| v.as_slice()).unwrap_or(&[]);
        
//         current_cset.add_op(op_code, &kh, key_ref, val_slice, None);
//         current_batch_keys.insert(key_ref.clone());
//     };

//     // 1. 严格按照顺序，先处理本区块由于状态缺失产生的 boundary_ops
//     if let Some(b_ops) = boundary_ops {
//         for op in b_ops {
//             process_op(op);
//         }
//     }

//     // 2. 然后再处理真实的交易链路 ops
//     for op in &ops {
//         process_op(op);
//     }

//     // NLL（非词法作用域生命周期）会在这里自动结束 process_op 对变量的借用，
//     // 所以下面再次使用 current_cset 完全合法。

//     if !current_cset.op_list.is_empty() {
//         current_cset.sort();
//         final_change_sets.push(current_cset);
//     }

//     // // 3. 将包含读写的混合区块直接发送给 update_kv 接口
//     // if !final_change_sets.is_empty() {
//     //     let task = SimpleTask {
//     //         change_sets: Arc::new(final_change_sets),
//     //     };
//     //     db_backend::update_kv(table_id, height as i64, vec![parking_lot::RwLock::new(Some(task))]);
//     // }
//     if !final_change_sets.is_empty() {
//         let mut tasks = Vec::new();
//         // 如果想充分利用核心，切分粒度可以自己调，这里以每 10 个 changeset 为一个 Task 举例
//         let chunk_size = std::cmp::max(1, final_change_sets.len() / 16); 
        
//         for chunk in final_change_sets.chunks(chunk_size) {
//             let task = SimpleTask {
//                 change_sets: Arc::new(chunk.to_vec()),
//             };
//             tasks.push(parking_lot::RwLock::new(Some(task)));
//         }

//         // 把拆分后（长度 > 1）的 tasks 数组发给底层，Rayon 就可以发力了！
//         db_backend::update_kv(table_id, height as i64, tasks);
//     }

//     total_ops_count
// }
