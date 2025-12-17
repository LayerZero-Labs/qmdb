use std::panic;
// use std::thread;
use std::time::{Duration, Instant};

use bench::{
    cli::Cli,
    speed::results::BenchmarkResults,
    speed::{db_backend, test_gen::TestGenV2},
};

use qmdb::def::IN_BLOCK_IDX_BITS;
use qmdb::def::OP_CREATE;
use qmdb::def::OP_DELETE;
use qmdb::def::OP_READ;
use qmdb::def::OP_WRITE;
use qmdb::indexer::hybrid::index_cache::COUNTERS;
use qmdb::test_helper::RandSrc;
use qmdb::test_helper::SimpleTask;
use qmdb::utils::{byte0_to_shard_id, changeset::ChangeSet, hasher};
use qmdb::metrics; // <--- 用于 WA/RA

use byteorder::{BigEndian, ByteOrder};
use clap::Parser;
use log::{info, warn};
use parking_lot::RwLock;

use std::ffi::CString;
use std::mem;
use chrono::Utc; // <--- 用于 GC 监控和时间戳
use std::thread; // <--- 用于 GC 监控
use qmdb::def::SHARD_COUNT; // <--- 用于 GC 监控
use serde_json; // <--- 用于 WA/RA

const PRIME1: u64 = 1299827; // Used for stride for hover_recreate_block

const N_TABLES: usize = 1;

fn main() {
    rayon::ThreadPoolBuilder::new()
        .num_threads(128)
        .build_global()
        .unwrap();

    let args: Cli = Cli::parse();
    let mut results = BenchmarkResults::new(&args);

    // Print config
    println!("config: {:?}", args);

    println!("#Entries to populate DB with: {}", args.entry_count);

    //  Make db_dir if it does not exist
    std::fs::create_dir_all(args.db_dir.as_str()).unwrap();
    for i in 0..N_TABLES {
        db_backend::init(args.db_dir.as_str(), i);
    }
    println!("Using database backend: {}", db_backend::NAME);
    results.db_backend = db_backend::NAME.to_string();
    results.log_time("initialized");

    // ===== VVV GC 监控线程 VVV =====
    let table_id_for_monitor = 0; // 假设我们总是监控表 0
    // [New] 传入 db_dir 路径，以便监控该路径所在的分区
    let monitor_db_dir = args.db_dir.clone();
    let _monitor_handle = thread::spawn(move || {
        // 等待 15 秒，让数据库有时间完全启动并运行
        thread::sleep(Duration::from_secs(15));

        loop {
            // 尝试获取数据库句柄
            if let Some((metadb_arc, indexer_arc)) = db_backend::get_db_handles(table_id_for_monitor) {
                
                let metadb = metadb_arc.read(); // 获取 MetaDB 的读锁
                let mut total_garbage_entries: u64 = 0;
                let mut total_active_entries: u64 = 0;
                let mut total_on_disk_entries: u64 = 0;
                let mut total_disk_size_bytes: u64 = 0;
                let mut estimated_garbage_size_bytes: f64 = 0.0;

                // 遍历所有 Shard 并聚合数据
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
                
                // 在打印前显式释放锁
                drop(metadb); 

                // ===== [New] 获取物理磁盘真实使用量 =====
                let mut physical_used_bytes: u64 = 0;
                let mut physical_total_bytes: u64 = 0;

                unsafe {
                    let path = CString::new(monitor_db_dir.as_str()).unwrap_or_default();
                    let mut stats: libc::statvfs = mem::zeroed();
                    
                    if libc::statvfs(path.as_ptr(), &mut stats) == 0 {
                        let block_size = stats.f_frsize as u64; // 文件系统块大小
                        let total_blocks = stats.f_blocks as u64;
                        let free_blocks = stats.f_bfree as u64; // Root 可用的空闲块
                        // 或者用 f_bavail (非特权用户可用)，通常数据库用 f_bfree 更接近物理真实
                        
                        physical_total_bytes = total_blocks * block_size;
                        physical_used_bytes = (total_blocks - free_blocks) * block_size;
                    } else {
                        warn!("Failed to get disk stats for {}", monitor_db_dir);
                    }
                }
                // =========================================

                // 打印我们的快照日志
                println!(
                    "GC_SNAPSHOT: Timestamp={} TotalEntries(Disk)={} ActiveEntries(Mem)={} GarbageEntries={} TotalDiskSizeMB={:.2} EstGarbageSizeMB={:.2} PhysicalDiskUsedMB={:.2} PhysicalDiskTotalMB={:.2}",
                    Utc::now().to_rfc3339(),
                    total_on_disk_entries,
                    total_active_entries,
                    total_garbage_entries,
                    total_disk_size_bytes as f64 / (1024.0 * 1024.0),
                    estimated_garbage_size_bytes / (1024.0 * 1024.0),
                    physical_used_bytes as f64 / (1024.0 * 1024.0), // [New]
                    physical_total_bytes as f64 / (1024.0 * 1024.0) // [New]
                );

            } else {
                // 这种情况可能在启动时发生
                println!("GC_SNAPSHOT: 等待数据库句柄...");
            }
            
            // 定义你的采样周期
            thread::sleep(Duration::from_secs(10));
        }
    });
    // ===== ^^^ GC 监控线程 ^^^ =====

    // Calculate tasks (transactions) per block based on desired ops per block
    // Each task contains changesets_per_task changesets
    // Each changeset has ~10 operations (9 writes + 1 delete typically)
    let ops_per_changeset = 10; //test_gen.num_ops_in_cset() as usize;
    let tasks_per_block = args.ops_per_block / (args.changesets_per_task * ops_per_changeset);

    // task_ids encoded the local task_id index in the first IN_BLOCK_IDX_BITS bits of a i64 and
    // the height in the remaining bits. This just makes sure we don't overflow during testing.
    if tasks_per_block >= (1 << IN_BLOCK_IDX_BITS) {
        panic!("tasks_per_block {} is too large", tasks_per_block);
    }
    // Check that it divides evenly
    if args.entry_count % args.ops_per_block != 0 {
        panic!(
            "entry_count {} is not divisible by ops_per_block {}",
            args.entry_count, args.ops_per_block
        );
    }
    let blocks_for_db_population = args.entry_count / args.ops_per_block;
    if blocks_for_db_population < args.tps_blocks {
        panic!(
            "blocks_for_db_population ({}/{}={}) cannot be less than tps_blocks ({}). Please make entry_count larger",
            args.entry_count, args.ops_per_block,
            blocks_for_db_population, args.tps_blocks
        );
    }
    println!("blocks_for_db_population: {}", blocks_for_db_population);

    println!("Workload configuration:");
    println!(
        "  Changesets per task/transaction: {}",
        args.changesets_per_task
    );
    println!("  Operations per changeset: {}", ops_per_changeset);
    println!("  Tasks (transactions) per block: {}", tasks_per_block);
    println!(
        "  Total operations per block: ~{}",
        tasks_per_block * args.changesets_per_task * ops_per_changeset
    );

    // We divide blocks into rounds
    info!(
        "{} blocks x {} ops per block = {} entries created",
        blocks_for_db_population, args.ops_per_block, args.entry_count
    );
    info!(
        "Every {} blocks ({} entries), we will benchmark ops/sec",
        args.hover.hover_interval,
        args.hover.hover_interval * args.ops_per_block
    );

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

    // Future: allow for multiple threads, one per table
    let table_id = 0;
    // TODO: Create a random source file from /dev/urandom if it doesn't exist
    let mut randsrc: RandSrc = RandSrc::new(
        args.randsrc_filename.as_str(),
        &format!("qmdb-{}", table_id),
    );
    // Create test generator with calculated parameters
    let mut test_gen_ = TestGenV2::new(
        &mut randsrc,
        args.entry_count,
        args.changesets_per_task as usize,
        tasks_per_block as usize,
    );
    run(
        0,
        &mut test_gen_,
        &mut randsrc,
        &mut results,
        args.tps_blocks,
        args.hover.hover_interval,
        args.hover.hover_recreate_block,
        args.hover.hover_write_block,
        args.hover.num_read_latency_samples,
        args.output_filename.as_str(),
    );
}

fn run(
    table_id: usize,
    test_gen: &mut TestGenV2,
    randsrc: &mut RandSrc,
    results: &mut BenchmarkResults,
    tps_blocks: u64,
    hover_interval: u64,
    hover_recreate_block: u64,
    hover_write_block: u64,
    num_read_latency_samples: u64,
    output_filename: &str,
) {
    // We create 500 blocks every round
    // 50000 tasks in a block * 2cset * (9 writes + 1 delete) = 1 million ops
    // -> 2 million entries per block
    // -> 100 million entries per round
    let blk_in_round = test_gen.block_in_round();

    let mut last_print = Instant::now();
    let start = Instant::now();
    let mut height = 1;
    let mut hover_time_since_last_print = Duration::from_secs(0);
    let mut current_decile_print: i64 = -1;

    // let bm_result = panic::catch_unwind(move || {
    let mut last_snapshot_time = Instant::now();
    let mut last_user_bytes = 0u64;
    let mut last_gc_bytes = 0u64;
    let REPORT_INTERVAL = 2; // 每 4 个 Block 输出一次
    
    // ===== VVV TIMESTAMP 1 (Population Start) VVV =====
    println!("Timestamp: {}", Utc::now().to_rfc3339());
    // ===== ^^^ TIMESTAMP ^^^ =====

    // Populate the database
    for produced_blocks in 0..blk_in_round {
        // Every hover_interval blocks, we run hover tasks to benchmark ops
        // Every hover_write_block blocks, we time how long it takes to create the blocks
        // Height increases for every block added (here and in run_single_hover_block)

        // Throttle the print rate: print every 10%, or on the last block, or on the first block

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

        // Generate transactions to populate the database.

        // ------------- add timestamp -------------
        println!(
            "PIPELINE_LOG: CLIENT_GENERATE_START height={} timestamp={}",
            height,
            Utc::now().to_rfc3339()
        );
        // -----------------------------------------
        let task_list = test_gen.gen_block();
        // ------------- add timestamp -------------
        println!(
            "PIPELINE_LOG: CLIENT_GENERATE_END height={} timestamp={}",
            height,
            Utc::now().to_rfc3339()
        );
        // -----------------------------------------

        // let task_count = task_list.len() as i64;
        // if height % 10 == 5 {
        //     // Progress update every increase in height of 5
        //     println!(
        //         "Elapse produced={} task_count={} {:#08x} elapsed={:.2?}",
        //         produced_blocks,
        //         task_count,
        //         task_count,
        //         start.elapsed()
        //     );
        // }

        // ------------- add timestamp -------------
        println!(
            "PIPELINE_LOG: CLIENT_SUBMIT_START height={} timestamp={}",
            height,
            Utc::now().to_rfc3339()
        );
        // -----------------------------------------

        db_backend::create_kv(table_id, height, task_list);

        // ------------- add timestamp -------------        
        println!(
            "PIPELINE_LOG: CLIENT_SUBMIT_END height={} timestamp={}",
            height,
            Utc::now().to_rfc3339()
        );
        // -----------------------------------------
        
        height += 1;

        results.update_entries_in_db((produced_blocks + 1) * test_gen.num_op_in_blk());

        if height % REPORT_INTERVAL == 0 {
            // 使用通用函数打印并更新快照
            log_io_stat(height, &mut last_snapshot_time, &mut last_user_bytes, &mut last_gc_bytes);
        }

        // Two times of progress bar
        // 1. Update the user interactively, time left, TPS -- based on time elapsed
        // 2. Log throughput every K blocks so that we can see the trend
        //    - Need to update the last value, and store a new value in a vector
        //    - Should also log the timestamp (absolute and relative)

        // Print a progress message every hover_write_block=10(dev)/1500(test) blocks
        let update_now =
            produced_blocks % hover_write_block == 0 || produced_blocks == blk_in_round - 1;
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

        // We need to ensure that hover_recreate_block <= number of produced entries
        // Run delete/create/update/read benchmarks every hover_interval blocks (not on the first block)
        if (produced_blocks + 1) % hover_interval == 0 || produced_blocks == blk_in_round - 1 {
            // Record time taken in this part of loop so that we can offset it from block population
            let hover_start = Instant::now();
            results.log_system_stats();

            // ===== VVV TIMESTAMP 2 (Hover Start) VVV =====
            println!("Timestamp: {}", Utc::now().to_rfc3339());
            // ===== ^^^ TIMESTAMP ^^^ =====
            
            run_hover_tasks(
                table_id,
                randsrc,
                &mut height,
                test_gen,
                hover_recreate_block,
                hover_write_block,
                num_read_latency_samples,
                results,
                &mut last_snapshot_time,
                &mut last_user_bytes,
                &mut last_gc_bytes,
            );

            println!("Writing partial results to file: {}", output_filename);
            results.write_to_file(output_filename);
            hover_time_since_last_print += hover_start.elapsed();
        }

        if height % 10 == 0 {
            // Print hybrid cache counters
            COUNTERS.print();
        }
    }

    println!(
        "Block population complete. Writing partial results to file: {}",
        output_filename
    );
    results.write_to_file(output_filename);
    results.print_summary();

    println!(
        "Benchmarking TPS: {} transactions, {} ops, {} blocks",
        tps_blocks * test_gen.num_txn_in_blk(),
        tps_blocks * test_gen.num_op_in_blk(),
        tps_blocks
    );

    // ===== VVV TIMESTAMP 7 (TPS Start) VVV =====
    println!("Timestamp: {}", Utc::now().to_rfc3339());
    // ===== ^^^ TIMESTAMP ^^^ =====

    // Benchmarking TPS
    let tps_start = Instant::now();
    let mut transactions_performed: u64 = 0;
    for _ in 0..tps_blocks {
        // Each transaction is a task
        // task_count is the number of transactions
        let task_list = test_gen.gen_block();
        let task_count = task_list.len();
        db_backend::update_kv(table_id, height, task_list);
        height += 1;
        transactions_performed += task_count as u64;
        // TODO: Progress update with generated_blocks
    }

    // ===== VVV TIMESTAMP 6 (TPS End) VVV =====
    println!("Timestamp: {}", Utc::now().to_rfc3339());
    // ===== ^^^ TIMESTAMP ^^^ =====

    results.record_throughput_as_total("transactions", transactions_performed, tps_start.elapsed());
    results.log_system_stats();
    println!("Benchmarking completed successfully");
    results.success = true;
    // });

    // if let Err(e) = bm_result {
    //     println!("Benchmarking failed: {:?}", e);
    //     results.success = false;
    // }
    results.complete = true;
    results.log_time("complete");
    results.print_summary();

    println!("Writing results to file: {}", output_filename);
    results.write_to_file(output_filename);
}

/// 打印 IO_STAT，并更新 last_snapshot_time / last_user_bytes / last_gc_bytes
pub fn log_io_stat(
    height: i64,
    last_snapshot_time: &mut Instant,
    last_user_bytes: &mut u64,
    last_gc_bytes: &mut u64,
) {
    // 计算时间间隔
    let now = Instant::now();
    let elapsed = now.duration_since(*last_snapshot_time).as_secs_f64();
    if elapsed <= 0.0 {
        return;
    }

    // 读取当前 metrics 快照
    let curr_metrics = metrics::get_all_counters();

    // 计算增量
    let delta_user = curr_metrics.physical_write_bytes_user.saturating_sub(*last_user_bytes);
    let delta_gc = curr_metrics.physical_write_bytes_gc.saturating_sub(*last_gc_bytes);
    let delta_total = delta_user.saturating_add(delta_gc);

    // 计算 MB/s
    let user_mbps = (delta_user as f64) / 1024.0 / 1024.0 / elapsed;
    let gc_mbps = (delta_gc as f64) / 1024.0 / 1024.0 / elapsed;
    let total_mbps = (delta_total as f64) / 1024.0 / 1024.0 / elapsed;

    // 计算 WA（写放大）
    let wa = if user_mbps > 0.0 { total_mbps / user_mbps } else { 0.0 };

    println!(
        "IO_STAT: Height={} Time={} UserWrite={:.2} MB/s GC_Move={:.2} MB/s Total={:.2} MB/s WA={:.2}",
        height,
        Utc::now().to_rfc3339(),
        user_mbps,
        gc_mbps,
        total_mbps,
        wa
    );

    // 更新快照
    *last_snapshot_time = now;
    *last_user_bytes = curr_metrics.physical_write_bytes_user;
    *last_gc_bytes = curr_metrics.physical_write_bytes_gc;
}



fn run_hover_tasks(
    table_id: usize,
    randsrc: &mut RandSrc,
    height: &mut i64,
    test_gen: &mut TestGenV2,
    hover_recreate_block: u64,
    hover_write_block: u64,
    num_read_latency_samples: u64,
    results: &mut BenchmarkResults,
    last_snapshot_time: &mut Instant,
    last_user_bytes: &mut u64,
    last_gc_bytes: &mut u64,
) {
    let op_in_blk = test_gen.num_op_in_blk();

    let max_num = if test_gen.cur_round == 0 {
        test_gen.cur_num
    } else {
        test_gen.sp.entry_count
    };

    if hover_recreate_block * op_in_blk < max_num {
        let stride: u64 = (max_num / (op_in_blk * hover_recreate_block)) * PRIME1;
        
        // --- Deletes ---
        println!("IO_SNAPSHOT: phase_start=deletes counters={}", serde_json::to_string(&metrics::get_all_counters()).unwrap());
        hover_execute_op(
            table_id,
            "deletes",
            OP_DELETE,
            hover_recreate_block,
            op_in_blk,
            stride,
            max_num,
            test_gen,
            height,
            randsrc,
            results,
            last_snapshot_time,
            last_user_bytes,
            last_gc_bytes,
        );
        println!("IO_SNAPSHOT: phase_end=deletes counters={}", serde_json::to_string(&metrics::get_all_counters()).unwrap());
        // ===== VVV TIMESTAMP 3 (Delete End) VVV =====
        println!("Timestamp: {}", Utc::now().to_rfc3339());
        // ===== ^^^ TIMESTAMP ^^^ =====

        // --- Inserts ---
        println!("IO_SNAPSHOT: phase_start=inserts counters={}", serde_json::to_string(&metrics::get_all_counters()).unwrap());
        hover_execute_op(
            table_id,
            "inserts",
            OP_CREATE,
            hover_recreate_block,
            op_in_blk,
            stride,
            max_num,
            test_gen,
            height,
            randsrc,
            results,
            last_snapshot_time,
            last_user_bytes,
            last_gc_bytes,
        );
        println!("IO_SNAPSHOT: phase_end=inserts counters={}", serde_json::to_string(&metrics::get_all_counters()).unwrap());
        // ===== VVV TIMESTAMP 4 (Insert End) VVV =====
        println!("Timestamp: {}", Utc::now().to_rfc3339());
        // ===== ^^^ TIMESTAMP ^^^ =====

    } else {
        println!("skip hover delete and recreate blocks");
    }

    // --- Updates ---
    println!("IO_SNAPSHOT: phase_start=updates counters={}", serde_json::to_string(&metrics::get_all_counters()).unwrap());
    hover_execute_op(
        table_id,
        "updates",
        OP_WRITE,
        hover_write_block,
        op_in_blk,
        u64::MAX,
        max_num,
        test_gen,
        height,
        randsrc,
        results,
        last_snapshot_time,
        last_user_bytes,
        last_gc_bytes,
    );
    println!("IO_SNAPSHOT: phase_end=updates counters={}", serde_json::to_string(&metrics::get_all_counters()).unwrap());

    // ===== VVV TIMESTAMP 5 (Write End) VVV =====
    println!("Timestamp: {}", Utc::now().to_rfc3339());
    // ===== ^^^ TIMESTAMP ^^^ =====
    // 打印并重置 "updates" 阶段的延迟
    // qmdb::metrics::reset_and_print_write_latency("updates");


    // --- Reads ---
    println!("IO_SNAPSHOT: phase_start=reads counters={}", serde_json::to_string(&metrics::get_all_counters()).unwrap());
    
    hover_execute_op(
        table_id,
        "reads",
        OP_READ,
        hover_write_block,
        op_in_blk,
        u64::MAX,
        max_num,
        test_gen,
        height,
        randsrc,
        results,
        last_snapshot_time,
        last_user_bytes,
        last_gc_bytes,
    );

    // ===== VVV TIMESTAMP 6 (Read End) VVV =====
    println!("Timestamp: {}", Utc::now().to_rfc3339());
    // ===== ^^^ TIMESTAMP ^^^ =====

    // 2. 取消注释这个块 (同步延迟测量):
    measure_read_latency(
        table_id,
        max_num,
        randsrc,
        results,
        num_read_latency_samples,
    );

    println!("IO_SNAPSHOT: phase_end=reads counters={}", serde_json::to_string(&metrics::get_all_counters()).unwrap());
}

fn hover_execute_op(
    table_id: usize,
    operation_name: &str,
    op_type: u8,
    block_count: u64,
    op_in_blk: u64,
    stride: u64,
    max_num: u64,
    test_generator: &mut TestGenV2,
    height: &mut i64,
    randsrc: &mut RandSrc,
    results: &mut BenchmarkResults,
    last_snapshot_time: &mut Instant,
    last_user_bytes: &mut u64,
    last_gc_bytes: &mut u64,
) {
    let bm_start = Instant::now();
    print!(
        "Benchmarking {} ({} ops, {} blocks, stride={})...",
        operation_name,
        block_count * op_in_blk,
        block_count,
        if stride == u64::MAX {
            "random".to_string()
        } else {
            format!("{}", stride)
        }
    );
    println!(
        "stride: {}, op_in_blk: {}, max_num: {}",
        stride, op_in_blk, max_num
    );
    for i in 0..block_count {
        let start: u64 = if stride == u64::MAX {
            rand_between(randsrc, 0, max_num as usize) as u64
        } else {
            (i * stride * op_in_blk) % max_num
        };
        // let start = (i * stride * op_in_blk) % max_num;
        run_single_hover_block(
            table_id,
            randsrc,
            test_generator,
            height,
            op_type,
            start,
            stride,
            max_num,
        );
        if i % 10 == 5 {
            println!("hover {} {} (partial)", operation_name, i);
        }

        if i % 2 == 0 {
        log_io_stat(
            *height,
            last_snapshot_time,
            last_user_bytes,
            last_gc_bytes,
        );
    }
    }
    results.record_throughput(operation_name, block_count * op_in_blk, bm_start.elapsed());
}

#[allow(dead_code)]
#[inline]
fn generate_read_key(randsrc: &mut RandSrc, max_num: u64) -> [u8; 32 + 20] {
    let num = rand_between(randsrc, 0, max_num as usize);
    let mut k = [0u8; 32 + 20];
    BigEndian::write_u64(&mut k[12..20], num as u64);
    let hash = hasher::hash(&k[12..20]);
    k[20..20 + 32].copy_from_slice(&hash[..]);
    for i in 0..12 {
        k[i] = k[20 + i] ^ k[32 + i];
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

fn measure_read_latency(
    table_id: usize,
    max_num: u64,
    randsrc: &mut RandSrc,
    results: &mut BenchmarkResults,
    num_read_latency_samples: u64,
) {
    db_backend::flush(table_id);
    print!("Benchmarking read latency...");
    let latency = results.latencies.get_mut("reads").unwrap();
    for _ in 0..num_read_latency_samples {
        let k = generate_read_key(randsrc, max_num);
        let start = Instant::now();
        db_backend::read_kv(table_id, &vec![k]);
        let duration = start.elapsed();
        latency.record(duration);
    }
    println!(); // <--- 添加一个换行符，使日志更清晰

    if let Some(stats) = latency.detailed_stats() {
        stats.print("Read", true);
    }
}

fn run_single_hover_block(
    table_id: usize,
    randsrc: &mut RandSrc,
    test_gen: &mut TestGenV2,
    height: &mut i64,
    op_type: u8,
    mut start_num: u64,
    stride: u64,
    max_num: u64,
) {
    let mut task_list = Vec::with_capacity(test_gen.task_in_block);
    for i in 0..test_gen.task_in_block {
        let mut cset_list = Vec::with_capacity(test_gen.cset_in_task);
        for _ in 0..test_gen.cset_in_task {
            let mut cset = ChangeSet::new();
            let mut k = [0u8; 32 + 20];
            let mut v = [0u8; 32];
            let n = test_gen.wr_op_in_cset + test_gen.delete_op_in_cset;
            for _ in 0..n {
                let mut x = start_num;
                if stride == u64::MAX {
                    x = rand_between(randsrc, 0, max_num as usize) as u64;
                } else {
                    start_num = (start_num + stride) % max_num;
                }
                let kh = test_gen.fill_kv(x, &mut k[..], &mut v[..]);
                let shard_id = byte0_to_shard_id(kh[0]) as u8;
                if op_type == OP_READ && i == 0 {
                    // we need a least one write
                    cset.add_op(OP_WRITE, shard_id, &kh, &k[..], &v[..], None);
                } else {
                    cset.add_op(op_type, shard_id, &kh, &k[..], &v[..], None);
                }
            }
            cset.sort();
            cset_list.push(cset);
        }
        let task = SimpleTask::new(cset_list);
        task_list.push(RwLock::new(Some(task)));
    }

    match op_type {
        OP_CREATE => {
            db_backend::create_kv(table_id, *height, task_list);
        }
        OP_WRITE | OP_DELETE | OP_READ => {
            db_backend::update_kv(table_id, *height, task_list);
        }
        _ => {
            panic!("invalid op_type");
        }
    }

    *height += 1;
}

#[inline]
fn rand_between(randsrc: &mut RandSrc, min: usize, max: usize) -> usize {
    let span = max - min;
    min + randsrc.get_uint32() as usize % span
}