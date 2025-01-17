use std::panic;
// use std::thread;
use std::time::{Duration, Instant};

use bench::{
    cli::Cli,
    speed::results::BenchmarkResults,
    speed::{db_backend, test_gen::TestGenV2},
};

use qmdb::def::OP_CREATE;
use qmdb::def::OP_DELETE;
use qmdb::def::OP_READ;
use qmdb::def::OP_WRITE;
use qmdb::indexer::hybrid::index_cache::COUNTERS;
use qmdb::test_helper::RandSrc;
use qmdb::test_helper::SimpleTask;
use qmdb::utils::{byte0_to_shard_id, changeset::ChangeSet, hasher};

use byteorder::BigEndian;
use byteorder::ByteOrder;
use clap::Parser;
use log::{info, warn};
use parking_lot::RwLock;

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

    // Calculate tasks (transactions) per block based on desired ops per block
    // Each task contains changesets_per_task changesets
    // Each changeset has ~10 operations (9 writes + 1 delete typically)
    let ops_per_changeset = 10; //test_gen.num_ops_in_cset() as usize;
    let tasks_per_block = args.ops_per_block / (args.changesets_per_task * ops_per_changeset);
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
        let task_list = test_gen.gen_block();
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
        db_backend::create_kv(table_id, height, task_list);
        height += 1;

        results.update_entries_in_db((produced_blocks + 1) * test_gen.num_op_in_blk());

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
            run_hover_tasks(
                table_id,
                randsrc,
                &mut height,
                test_gen,
                hover_recreate_block,
                hover_write_block,
                num_read_latency_samples,
                results,
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

fn run_hover_tasks(
    table_id: usize,
    randsrc: &mut RandSrc,
    height: &mut i64,
    test_gen: &mut TestGenV2,
    hover_recreate_block: u64,
    hover_write_block: u64,
    num_read_latency_samples: u64,
    results: &mut BenchmarkResults,
) {
    let op_in_blk = test_gen.num_op_in_blk();

    let max_num = if test_gen.cur_round == 0 {
        test_gen.cur_num
    } else {
        test_gen.sp.entry_count
    };

    if hover_recreate_block * op_in_blk < max_num {
        let stride: u64 = (max_num / (op_in_blk * hover_recreate_block)) * PRIME1;
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
        );
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
        );
    } else {
        println!("skip hover delete and recreate blocks");
    }
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
    );

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
    );

    //measure_read_latency(
    //    table_id,
    //    max_num,
    //    randsrc,
    //    results,
    //    num_read_latency_samples,
    //);
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
