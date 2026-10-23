use bench::{
    cli::Cli,
    speed::results::BenchmarkResults,
    speed::{db_backend, test_gen::TestGenV2, zipf::ZipfSampler},
    RandSrc, SimpleTask, COUNTERS,
};

use qmdb_common::def::{OP_CREATE, OP_DELETE, OP_READ, OP_WRITE};
use qmdb_common::entryfile::entry::EntryBz;
use qmdb_common::task::taskid::join_task_id;
use qmdb_common::utils::changeset::ChangeSet;

use chrono::Utc;
use clap::Parser;
use log::{info, warn};
use parking_lot::RwLock;

use std::collections::HashSet;
use rayon::prelude::*;
use std::fs;
use std::io;
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const N_TABLES: usize = 1;
const PRIME1: u64 = 1299827; // Used for stride for hover_recreate_block

use bench::speed::disk_monitor;
use bench::speed::eth_replay;

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
    /// Mixed read/write hover: Some(r) => each op is a READ with probability r, else a
    /// WRITE (update of an existing key). None = a single-op phase.
    read_ratio: Option<f64>,
}

struct SingleHoverBlockConfig<'a> {
    table_id: usize,
    randsrc: &'a mut RandSrc,
    test_gen: &'a mut TestGenV2,
    height: &'a mut i64,
    op_type: u8,
    /// Mixed read/write: Some(r) => per-op READ with probability r, else WRITE.
    read_ratio: Option<f64>,
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

fn main() {
    let args: Cli = Cli::parse();
    let start_from_existing = args.start_from_existing;
    let mut results = BenchmarkResults::new(&args);
    let table_id = 0;
    rayon::ThreadPoolBuilder::new()
        .num_threads(args.rayon_threads)
        .build_global()
        .unwrap();

    // Make db_dir if it does not exist
    let is_eth_mode = args.eth_trace_dir.is_some() || args.init_db_path.is_some();
    std::fs::create_dir_all(args.db_dir.as_str()).unwrap();
    // Resolve the OS-physical-R/W disk from db_dir (single source: one --db-dir
    // drives DB path + I/O counters).
    disk_monitor::init_monitor_disk(&args.monitor_disk, args.db_dir.as_str());
    for i in 0..N_TABLES {
        db_backend::init(args.db_dir.as_str(), i, start_from_existing, is_eth_mode, args.entry_count);
    }
    println!("Using database backend: {}", db_backend::NAME);
    thread::sleep(Duration::from_secs(15));

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
        );
    } else {
        // 2. 计算 block/task/changeset 等常量
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

        // 3. 检查数据库大小是否合理
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

        // 4. 根据是否从已有数据库启动，设置初始高度与工作量
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

        // 5. 创建 TestGen（生成 workload）
        let mut randsrc: RandSrc =
            RandSrc::new(args.randsrc_filename.as_str(), &format!("qmdb-{table_id}"));
        let mut test_gen_ = TestGenV2::new(
            &mut randsrc,
            args.entry_count,
            args.changesets_per_task as usize,
            tasks_per_block as usize,
        );

        // 6. 运行 Benchmark
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
            },
            initial_height,
        );
    }
}

fn run(
    config: RunConfig,
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
    } = config;

    let blk_in_round = test_gen.block_in_round();
    let mut height = initial_height;
    let mut current_decile_print: i64 = -1;

    let mut last_print = Instant::now();
    let mut hover_time_since_last_print = Duration::from_secs(0);

    // Cumulative keys deleted across hover runs — drives delete stride_offset and insert num_offset.
    let mut total_deleted_keys: u64 = 0;

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

            // 2. 提交block至backend database
            db_backend::create_kv(table_id, height, task_list);
            height += 1;

            results.update_entries_in_db((produced_blocks + 1) * test_gen.num_op_in_blk());

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
                });

                // Hover test 结束的时间
                println!("Timestamp: {}", Utc::now().to_rfc3339());

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
        });

        // 记录一下时间，虽然不计入 population throughput，但有助于日志对齐
        hover_time_since_last_print += hover_start.elapsed();
        thread::sleep(Duration::from_secs(60));
    }

    println!("Block population complete.");

    results.log_system_stats();
    results.print_summary();
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
    } = config;

    let op_in_blk = test_gen.num_op_in_blk();

    // 如果未population完成，就在当前数据库大小下测试，如果完成，就在整个数据库下测试
    let max_num = if test_gen.cur_round == 0 {
        test_gen.cur_num
    } else {
        test_gen.sp.entry_count
    };

    // When HOVER_RW_RATIOS is set (mixed read/write sweep), skip the standard
    // insert/delete phases so the ratio hover runs on a clean post-populate key
    // space (each ratio point is its own fresh populate + single mixed hover).
    let rw_sweep_active = is_final
        && std::env::var("HOVER_RW_RATIOS").map(|s| !s.trim().is_empty()).unwrap_or(false);

    // --- UPDATES (write) ---
    hover_execute_op(HoverExecuteConfig {
        table_id,
        operation_name: "updates",
        read_ratio: None,
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
    });

    // --- READS ---
    hover_execute_op(HoverExecuteConfig {
        table_id,
        operation_name: "reads",
        read_ratio: None,
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
    });

    // Insert/delete run ONLY on the final (post-population) hover. Mid-population
    // hovers run read/write only — inserting/deleting before population completes
    // corrupts entry bookkeeping and can crash on deleted-key lookups.
    if rw_sweep_active {
        println!("rw-sweep active: skipping standard insert/delete phases");
    } else if !is_final {
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
            read_ratio: None,
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
        });

        // --- DELETES (PRIME1 stride, stride_offset advances each run to avoid cross-run double-deletion) ---
        // stride=PRIME1 is coprime to max_num → visits N distinct keys in [0, max_num) per run.
        // stride_offset shifts the starting position each run so successive runs target different keys.
        let delete_stride_offset = total_deleted_keys.wrapping_mul(PRIME1) % max_num;
        hover_execute_op(HoverExecuteConfig {
            table_id,
            operation_name: "deletes",
            read_ratio: None,
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
        });
        *total_deleted_keys += n_hover_ops;
    } else {
        println!("skip hover delete and insert blocks (db too small)");
    }

    // ---- Mixed read/write ratio sweep (env HOVER_RW_RATIOS="0.0,0.1,...,1.0") ----
    // Runs only on the final (full-DB) hover. For each ratio r, every op in the block
    // is a READ with probability r, else a WRITE (update of an existing key). The block
    // is submitted via update_kv (commits + executes mixed changesets, exactly like ETH
    // replay), so a single mixed block measures true mixed read/write throughput.
    // Pure read/write only — no insert/delete. Reuses the already-populated DB.
    if is_final {
        if let Ok(spec) = std::env::var("HOVER_RW_RATIOS") {
            println!("=== Mixed read/write ratio sweep: HOVER_RW_RATIOS={spec} ===");
            for tok in spec.split(',') {
                let tok = tok.trim();
                if tok.is_empty() { continue; }
                let r: f64 = match tok.parse() {
                    Ok(v) if (0.0..=1.0).contains(&v) => v,
                    _ => { println!("  skip invalid ratio '{tok}'"); continue; }
                };
                // metric name e.g. rw_read00 (all write) .. rw_read10 (all read)
                let name: &'static str =
                    Box::leak(format!("rw_read{:02}", (r * 10.0).round() as u32).into_boxed_str());
                println!("--- mixed hover: read_ratio={r} ({name}) ---");
                hover_execute_op(HoverExecuteConfig {
                    table_id,
                    operation_name: name,
                    read_ratio: Some(r),
                    op_type: OP_WRITE,        // => update_kv submit path (commit + mixed ok)
                    block_count: hover_write_block,
                    op_in_blk,
                    stride: u64::MAX,          // uniform random keys over the stable range
                    max_num,
                    num_offset: 0,
                    stride_offset: 0,
                    zipf_theta: 0.0,
                    test_generator: test_gen,
                    height,
                    randsrc,
                    results,
                });
            }
        }
    }

    let db_size_bytes = dir_size(Path::new(db_dir));
    let rss_kb = get_rss_kb();
    println!(
        "HOVER_END_STATS: db_size={:.2} GB  rss={:.2} GB",
        db_size_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        rss_kb as f64 / (1024.0 * 1024.0),
    );
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
        read_ratio,
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
    let (start_os_read, start_os_write) = get_linux_disk_rw_sectors(disk_monitor::monitor_disk()).unwrap_or((0, 0));
    println!("Timestamp (Start): {}", Utc::now().to_rfc3339());

    // 2. 核心执行逻辑
    let (tx, rx) = mpsc::sync_channel(10);
    let consumer_handle = thread::spawn(move || {
        while let Ok((blk_height, tasks)) = rx.recv() {
            match op_type {
                OP_CREATE => {
                    db_backend::create_kv(table_id, blk_height, tasks);
                },
                OP_DELETE => db_backend::delete_kv(table_id, blk_height, tasks),
                OP_WRITE => db_backend::update_kv(table_id, blk_height, tasks),
                OP_READ => {
                    #[cfg(all(not(feature = "use_nomt"), not(feature = "use_reth"), not(feature = "use_lvmt")))]
                    db_backend::update_kv(table_id, blk_height, tasks);

                    #[cfg(any(feature = "use_nomt", feature = "use_reth", feature = "use_lvmt"))]
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
            read_ratio,
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
    }
    drop(tx); 
    consumer_handle.join().unwrap();
    // Wait for QMDB engine pipeline to fully drain before stopping the timer.
    // Without this, the channel-buffered consumer thread architecture lets the
    // bench timer stop ~2 blocks before engine completion, causing ~2-9% over-
    // measurement of throughput.
    db_backend::flush(table_id);
    // QMDB: the updater treats OP_READ as a no-op, so flush() returns while the io_uring
    // prefetcher still has outstanding disk reads (~16% of a read phase). Wait for those to
    // drain — poll DIO_READ_COUNT until it plateaus — so the timer reflects the true read
    // completion time (SSD-bound), not the no-op updater's finish. This is a one-time
    // end-of-phase poll; for write phases the counter has already plateaued at flush.
    #[cfg(all(not(feature = "use_nomt"), not(feature = "use_reth"), not(feature = "use_lvmt")))]
    let phase_dur = {
        let mut last = qmdb::prefetcher::dioprefetcher::dio_read_count();
        let mut last_change = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(2));
            let now = qmdb::prefetcher::dioprefetcher::dio_read_count();
            if now != last {
                last = now;
                last_change = Instant::now();
            } else if last_change.elapsed() >= Duration::from_millis(40) {
                break; // no new SSD read for 40ms => prefetcher drained
            }
        }
        // last_change marks the last real read; the 40ms plateau wait is excluded.
        last_change.duration_since(bm_start)
    };
    #[cfg(any(feature = "use_nomt", feature = "use_reth", feature = "use_lvmt"))]
    let phase_dur = bm_start.elapsed();
    results.record_throughput(operation_name, block_count * op_in_blk, phase_dur);

    // 3. 输出当前的磁盘信息
    println!("Timestamp (End): {}", Utc::now().to_rfc3339());
    thread::sleep(Duration::from_secs(60)); 
    let (end_os_read, end_os_write) = get_linux_disk_rw_sectors(disk_monitor::monitor_disk()).unwrap_or((0, 0));

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
        read_ratio,
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
                    not(feature = "use_nomt"),
                    not(feature = "use_reth"),
                    not(feature = "use_lvmt")
                ));
                let actual_op = if let Some(ratio) = read_ratio {
                    // Mixed read/write sweep. Force the very first op of the block to
                    // WRITE (QMDB needs >=1 write per block to advance block id); every
                    // other op is READ with probability `ratio`, else WRITE (update).
                    if is_qmdb && _i == 0 && i == 0 && j == 0 {
                        OP_WRITE
                    } else if (randsrc.get_uint32() as f64) < ratio * (u32::MAX as f64) {
                        OP_READ
                    } else {
                        OP_WRITE
                    }
                } else if op_type == OP_READ && _i == 0 && is_qmdb {
                    // 只有 QMDB 才强行塞一个 Write 进去
                    OP_WRITE
                } else {
                    op_type
                };
                cset.add_op(actual_op, &kh, &k[..], &v[..], None);
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
) {
    let start_height = populate_start;
    let end_height = populate_end;
    println!("Running Real Ethereum Workload");
    println!("Populate Range: Block {} to {}", populate_start, populate_end);
    println!("Replay   Range: Block {} to {}", replay_start, replay_end);

    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
    let source = PathBuf::from(input_dir);

    let mut global_db_height: i64 = 1;

    // Phase 1: 将KV文件中的内容读取到内存，生成init db内存版本
    println!("\n=== Phase 1: Populating Base State ===");
    
    let (tx_p1, mut rx_p1) = tokio::sync::mpsc::channel(eth_replay::PREFETCH_BUFFER_BLOCKS);
    let source_clone_p1 = source.clone();
    
    // 启动生产者，按区块顺序吐出 base_kv 和 ops
    rt.spawn(async move {
        eth_replay::run_block_data_producer(start_height, end_height, source_clone_p1, tx_p1).await
    });

    let mut splitter = eth_replay::BaseStateSplitter::new();
    
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

    // Phase 2: 将init db内存版本populate到DB
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
                    db_backend::create_kv(table_id, global_db_height, batch_tasks);
                    global_db_height += 1;
                    batch_tasks = Vec::with_capacity(tasks_per_block); // 重置当前 Block
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
    
    if !batch_tasks.is_empty() {
        db_backend::create_kv(table_id, global_db_height, batch_tasks);
        global_db_height += 1;
    }

    db_backend::flush(table_id);
    println!("Total Entries Initialized: {}", total_init_entries);
    std::thread::sleep(Duration::from_secs(60));
    println!("Base State Population Complete.");

    // Phase 3: Replaying Operations (Benchmark) - 官方模式适配
    println!("\n=== Phase 2: Replaying Operations (Benchmark) ===");
    
    // === PRE-LOAD all replay ops into RAM before benchmark timer starts ===
    // Goal: Phase 3 main loop is bottlenecked by DB engine, not by SSD trace I/O.
    println!("
=== Pre-loading replay ops [{}, {}] into RAM ===", replay_start, replay_end);
    let preload_start = Instant::now();
    let mut replay_blocks: Vec<(u64, Option<Vec<eth_replay::Operation>>)> = Vec::new();
    {
        let (tx_pre, mut rx_pre) = tokio::sync::mpsc::channel(eth_replay::PREFETCH_BUFFER_BLOCKS);
        let source_pre = source.clone();
        rt.spawn(async move {
            eth_replay::run_ops_producer(replay_start, replay_end, source_pre, tx_pre).await
        });
        rt.block_on(async {
            while let Some(msg) = rx_pre.recv().await {
                let (h, ops_opt) = msg.expect("Pre-load failed");
                replay_blocks.push((h, ops_opt));
            }
        });
    }
    let preload_dur = preload_start.elapsed();
    println!("Pre-loaded {} blocks in {:?}", replay_blocks.len(), preload_dur);

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
            db_backend::update_kv(table_id, blk_height, tasks);
        }
    });

    // 3. 主线程：负责解析 ETH 数据，并按 `run_single_hover_block` 的结构拼装
    let mut total_ops = 0;
    let mut processed_eth_blocks = 0;
    let mut interval_start = Instant::now();
    let mut interval_ops = 0;


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

            let mut process_op = |op: &eth_replay::Operation| {
                let (op_code, key_ref, val_ref) = match op {
                    eth_replay::Operation::Create(k, v) => (OP_CREATE, k, Some(v)),
                    eth_replay::Operation::Update(k, v) => (OP_WRITE, k, Some(v)),
                    eth_replay::Operation::Delete(k)    => (OP_DELETE, k, None),
                    eth_replay::Operation::Read(k, v)   => (OP_READ, k, Some(v)), 
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
                    db_tx.send((global_db_height as i64, std::mem::replace(&mut task_list, Vec::new()))).unwrap();
                    global_db_height += 1;
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
        boundary: std::collections::HashMap<u64, Vec<eth_replay::Operation>>,
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
    ) -> std::collections::HashMap<u64, Vec<eth_replay::Operation>> {
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
                    let (h, ops): (u64, Vec<eth_replay::Operation>) =
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
) {
    run_eth_replay_inner(
        table_id, Some(trace_dir), Some(bundle_path),
        0, 0, replay_start, replay_end,
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
) {
    let start_height = populate_start;
    let end_height = populate_end;
    println!("Running Real Ethereum Workload");
    if let Some(p) = bundle_path { println!("Mode: bundle  path={}", p); }
    else { println!("Mode: trace  Populate Range: Block {} to {}", populate_start, populate_end); }
    println!("Replay   Range: Block {} to {}", replay_start, replay_end);

    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
    let source = input_dir.map(PathBuf::from);

    let mut global_db_height: i64 = 1;

    // Phase 1: produce (init_db, boundary_ops) — either from bundle or from trace
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
        let (tx_p1, mut rx_p1) = tokio::sync::mpsc::channel(eth_replay::PREFETCH_BUFFER_BLOCKS);
        let source_clone_p1 = source.clone();
        rt.spawn(async move {
            eth_replay::run_block_data_producer(start_height, end_height, source_clone_p1, tx_p1).await
        });
        let mut splitter = eth_replay::BaseStateSplitter::new();
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

    // Phase 2: 将init db内存版本populate到DB (streaming: pull one entry at a time
    // via src.next_entry() so the full init_db Vec is never held in memory)
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
                    db_backend::create_kv(table_id, global_db_height, batch_tasks);
                    global_db_height += 1;
                    batch_tasks = Vec::with_capacity(tasks_per_block); // 重置当前 Block
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
    
    if !batch_tasks.is_empty() {
        db_backend::create_kv(table_id, global_db_height, batch_tasks);
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

    // Phase 3: Replaying Operations (Benchmark) - 官方模式适配
    println!("\n=== Phase 2: Replaying Operations (Benchmark) ===");
    
    // === STREAMING mode: 50-block bounded channel ===
    // Memory bounded ~25 MB (50 blocks × ~0.5 MB), supports arbitrary replay range.
    // Producer runs ahead in background, main loop drains from channel.
    println!("\n=== Streaming replay ops [{}, {}] (queue=50 blocks) ===", replay_start, replay_end);
    let source_stream = source.clone().expect("eth-trace-dir is required for Phase 3 replay");
    let (tx_stream, mut rx_stream) = tokio::sync::mpsc::channel(50);
    let source_pre = source_stream.clone();
    rt.spawn(async move {
        eth_replay::run_ops_producer(replay_start, replay_end, source_pre, tx_stream).await
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
            db_backend::update_kv(table_id, blk_height, tasks);
        }
    });

    // 3. 主线程：负责解析 ETH 数据，并按 `run_single_hover_block` 的结构拼装
    let mut total_ops = 0;
    let mut processed_eth_blocks = 0;
    let mut interval_start = Instant::now();
    let mut interval_ops = 0;


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

            let mut process_op = |op: &eth_replay::Operation| {
                let (op_code, key_ref, val_ref) = match op {
                    eth_replay::Operation::Create(k, v) => (OP_CREATE, k, Some(v)),
                    eth_replay::Operation::Update(k, v) => (OP_WRITE, k, Some(v)),
                    eth_replay::Operation::Delete(k)    => (OP_DELETE, k, None),
                    eth_replay::Operation::Read(k, v)   => (OP_READ, k, Some(v)), 
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
                    db_tx.send((global_db_height as i64, std::mem::replace(&mut task_list, Vec::new()))).unwrap();
                    global_db_height += 1;
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
}

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

    println!("----------------------------------------------------------------");
    println!("TEST_REPORT: Phase={} | Ops={}", phase, ops_count);
    println!("  > Total KV Size (Pure)  = {} bytes ({:.2} MB)", total_kv_size, total_kv_size as f64 / 1024.0 / 1024.0);
    println!("  [OS-level Disk IO]");
    println!("  > OS Physical Write = {} bytes ({:.2} MB)", os_write_bytes, os_write_bytes as f64 / 1024.0 / 1024.0);
    println!("  > OS Physical Read  = {} bytes ({:.2} MB)", os_read_bytes, os_read_bytes as f64 / 1024.0 / 1024.0);
    println!("----------------------------------------------------------------");
}
