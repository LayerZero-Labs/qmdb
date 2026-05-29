use bench::{
    cli::Cli,
    speed::results::BenchmarkResults,
    speed::{db_backend, test_gen::TestGenV2},
    RandSrc, SimpleTask, COUNTERS,
};
use byteorder::BigEndian;
use byteorder::ByteOrder;
use clap::Parser;
use log::{info, warn};
use parking_lot::RwLock;
use qmdb_common::def::{OP_CREATE, OP_DELETE, OP_READ, OP_WRITE};
use qmdb_common::entryfile::entry::EntryBz;
use qmdb_common::task::taskid::join_task_id;
use qmdb_common::utils::changeset::ChangeSet;
use std::panic;
use std::time::{Duration, Instant};

#[cfg(feature = "qmdb_bench_correctness")]
use bench::speed::correctness::{build_key_and_expected_value, BitmapModel};
#[cfg(all(
    feature = "qmdb_bench_correctness",
    not(feature = "use_mdbx"),
    not(feature = "use_rocksdb")
))]
use bench::speed::qmdb;

const PRIME1: u64 = 1299827; // Used for stride for hover_recreate_block

const N_TABLES: usize = 1;

struct RunConfig<'a> {
    table_id: usize,
    test_gen: &'a mut TestGenV2,
    randsrc: &'a mut RandSrc,
    results: &'a mut BenchmarkResults,
    tps_blocks: u64,
    hover_interval: u64,
    hover_recreate_block: u64,
    hover_write_block: u64,
    output_filename: &'a str,

    #[cfg(feature = "qmdb_bench_correctness")]
    check_correctness: bool,
}

struct HoverTaskConfig<'a> {
    table_id: usize,
    randsrc: &'a mut RandSrc,
    height: &'a mut i64,
    test_gen: &'a mut TestGenV2,
    hover_recreate_block: u64,
    hover_write_block: u64,
    results: &'a mut BenchmarkResults,
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
    test_generator: &'a mut TestGenV2,
    height: &'a mut i64,
    randsrc: &'a mut RandSrc,
    results: &'a mut BenchmarkResults,
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
}

fn main() {
    // TODO: Dev
    // need use unique keys for each changeset

    rayon::ThreadPoolBuilder::new()
        .num_threads(128)
        .build_global()
        .unwrap();

    let args: Cli = Cli::parse();
    let mut results = BenchmarkResults::new(&args);

    // Print config
    println!("config: {args:?}");

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

    // task_ids encoded the local task_id index in the first IN_BLOCK_IDX_BITS bits of a i64 and
    // the height in the remaining bits. This just makes sure we don't overflow during testing.
    if tasks_per_block >= join_task_id(1, 0, false) as u64 {
        panic!("tasks_per_block {tasks_per_block} is too large");
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
    println!("blocks_for_db_population: {blocks_for_db_population}");

    println!("Workload configuration:");
    println!(
        "  Changesets per task/transaction: {}",
        args.changesets_per_task
    );
    println!("  Operations per changeset: {ops_per_changeset}");
    println!("  Tasks (transactions) per block: {tasks_per_block}");
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
    let mut randsrc: RandSrc =
        RandSrc::new(args.randsrc_filename.as_str(), &format!("qmdb-{table_id}"));
    // Create test generator with calculated parameters
    let mut test_gen_ = TestGenV2::new(
        &mut randsrc,
        args.entry_count,
        args.changesets_per_task as usize,
        tasks_per_block as usize,
    );

    #[cfg(feature = "qmdb_bench_correctness")]
    let correctness_model = if args.check_correctness {
        let entry_count_wo_del = args.entry_count * 9 / 10;
        Some(BitmapModel::new(args.entry_count, entry_count_wo_del))
    } else {
        None
    };

    #[cfg(not(feature = "qmdb_bench_correctness"))]
    let correctness_model: Option<()> = None;

    run(
        RunConfig {
            table_id,
            test_gen: &mut test_gen_,
            randsrc: &mut randsrc,
            results: &mut results,
            tps_blocks: args.tps_blocks,
            hover_interval: args.hover.hover_interval,
            hover_recreate_block: args.hover.hover_recreate_block,
            hover_write_block: args.hover.hover_write_block,
            output_filename: args.output_filename.as_str(),
            #[cfg(feature = "qmdb_bench_correctness")]
            check_correctness: args.check_correctness,
        },
        correctness_model,
    );
}

fn run(
    config: RunConfig,
    #[cfg(feature = "qmdb_bench_correctness")] mut correctness_model: Option<BitmapModel>,
    #[cfg(not(feature = "qmdb_bench_correctness"))] _correctness_model: Option<()>,
) {
    let RunConfig {
        table_id,
        test_gen,
        randsrc,
        results,
        tps_blocks,
        hover_interval,
        hover_recreate_block,
        hover_write_block,
        output_filename,
        #[cfg(feature = "qmdb_bench_correctness")]
        check_correctness,
    } = config;

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
        #[cfg(feature = "qmdb_bench_correctness")]
        if check_correctness {
            if let Some(model) = correctness_model.as_mut() {
                // During population we only update the bitmap model; sampling
                // and DB comparisons are performed during TPS benchmarking.
                model.apply_block(&task_list);
            }
        }

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
            run_hover_tasks(HoverTaskConfig {
                table_id,
                randsrc,
                height: &mut height,
                test_gen,
                hover_recreate_block,
                hover_write_block,
                results,
                #[cfg(feature = "qmdb_bench_correctness")]
                check_correctness,
                #[cfg(feature = "qmdb_bench_correctness")]
                correctness_model: &mut correctness_model,
            });

            println!("Writing partial results to file: {output_filename}");
            results.write_to_file(output_filename);
            hover_time_since_last_print += hover_start.elapsed();
        }

        if height % 10 == 0 {
            // Print hybrid cache counters
            COUNTERS.print();
        }
    }

    println!("Block population complete. Writing partial results to file: {output_filename}");
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
    for blk_idx in 0..tps_blocks {
        // Each transaction is a task
        // task_count is the number of transactions
        let task_list = test_gen.gen_block();
        let task_count = task_list.len();

        #[cfg(feature = "qmdb_bench_correctness")]
        if check_correctness {
            if let Some(model) = correctness_model.as_mut() {
                // During TPS benchmarking we continue to maintain bitmap state.
                model.apply_block(&task_list);
            }
        }

        db_backend::update_kv(table_id, height, task_list);
        height += 1;
        transactions_performed += task_count as u64;

        #[cfg(all(
            feature = "qmdb_bench_correctness",
            not(feature = "use_mdbx"),
            not(feature = "use_rocksdb")
        ))]
        if check_correctness {
            if let Some(model) = correctness_model.as_mut() {
                db_backend::flush(table_id);
                // During TPS rounds, also perform lightweight sampling+comparison
                // against the QMDB backend, using the full configured entry_count
                // as the upper bound for logical key numbers.
                let max_num = test_gen.sp.entry_count;
                run_hover_correctness_sampling(
                    table_id, max_num, test_gen, model, randsrc, results,
                );
            }
        }
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

    println!("Writing results to file: {output_filename}");
    results.write_to_file(output_filename);
}

fn run_hover_tasks(config: HoverTaskConfig) {
    let HoverTaskConfig {
        table_id,
        randsrc,
        height,
        test_gen,
        hover_recreate_block,
        hover_write_block,
        results,
        #[cfg(feature = "qmdb_bench_correctness")]
        check_correctness,
        #[cfg(feature = "qmdb_bench_correctness")]
        correctness_model,
    } = config;

    let op_in_blk = test_gen.num_op_in_blk();

    let max_num = if test_gen.cur_round == 0 {
        test_gen.cur_num
    } else {
        test_gen.sp.entry_count
    };

    if hover_recreate_block * op_in_blk < max_num {
        let stride: u64 = (max_num / (op_in_blk * hover_recreate_block)) * PRIME1;
        hover_execute_op(HoverExecuteConfig {
            table_id,
            operation_name: "deletes",
            op_type: OP_DELETE,
            block_count: hover_recreate_block,
            op_in_blk,
            stride,
            max_num,
            test_generator: test_gen,
            height,
            randsrc,
            results,
        });
        hover_execute_op(HoverExecuteConfig {
            table_id,
            operation_name: "inserts",
            op_type: OP_CREATE,
            block_count: hover_recreate_block,
            op_in_blk,
            stride,
            max_num,
            test_generator: test_gen,
            height,
            randsrc,
            results,
        });
    } else {
        println!("skip hover delete and recreate blocks");
    }
    hover_execute_op(HoverExecuteConfig {
        table_id,
        operation_name: "updates",
        op_type: OP_WRITE,
        block_count: hover_write_block,
        op_in_blk,
        stride: u64::MAX,
        max_num,
        test_generator: test_gen,
        height,
        randsrc,
        results,
    });

    hover_execute_op(HoverExecuteConfig {
        table_id,
        operation_name: "reads",
        op_type: OP_READ,
        block_count: hover_write_block,
        op_in_blk,
        stride: u64::MAX,
        max_num,
        test_generator: test_gen,
        height,
        randsrc,
        results,
    });

    #[cfg(all(
        feature = "qmdb_bench_correctness",
        not(feature = "use_mdbx"),
        not(feature = "use_rocksdb")
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
        test_generator,
        height,
        randsrc,
        results,
    } = config;

    let bm_start = Instant::now();
    print!(
        "Benchmarking {} ({} ops, {} blocks, stride={})...",
        operation_name,
        block_count * op_in_blk,
        block_count,
        if stride == u64::MAX {
            "random".to_string()
        } else {
            format!("{stride}")
        }
    );
    println!("stride: {stride}, op_in_blk: {op_in_blk}, max_num: {max_num}");
    for i in 0..block_count {
        let start: u64 = if stride == u64::MAX {
            rand_between(randsrc, 0, max_num as usize) as u64
        } else {
            (i * stride * op_in_blk) % max_num
        };
        // let start = (i * stride * op_in_blk) % max_num;
        run_single_hover_block(SingleHoverBlockConfig {
            table_id,
            randsrc,
            test_gen: test_generator,
            height,
            op_type,
            start_num: start,
            stride,
            max_num,
        });
        if i % 10 == 5 {
            println!("hover {operation_name} {i} (partial)");
        }
    }
    results.record_throughput(operation_name, block_count * op_in_blk, bm_start.elapsed());
}

/// Perform lightweight correctness sampling during hover reads.
///
/// This is only enabled for the QMDB backend (no `use_mdbx` / `use_rocksdb`)
/// and when the `qmdb_bench_correctness` feature is active.
#[cfg(all(
    feature = "qmdb_bench_correctness",
    not(feature = "use_mdbx"),
    not(feature = "use_rocksdb")
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

    let mut key_list: Vec<[u8; 52]> = Vec::new();
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
fn generate_read_key(randsrc: &mut RandSrc, max_num: u64) -> [u8; 32 + 20] {
    let num = rand_between(randsrc, 0, max_num as usize);
    let mut k = [0u8; 32 + 20];
    BigEndian::write_u64(&mut k[12..20], num as u64);
    let hash = qmdb_common::entryfile::entry::entry_key_hash(&k[12..20]);
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

fn run_single_hover_block(config: SingleHoverBlockConfig) {
    let SingleHoverBlockConfig {
        table_id,
        randsrc,
        test_gen,
        height,
        op_type,
        mut start_num,
        stride,
        max_num,
    } = config;

    let mut task_list = Vec::with_capacity(test_gen.task_in_block);
    for _i in 0..test_gen.task_in_block {
        let mut cset_list = Vec::with_capacity(test_gen.cset_in_task);
        for i in 0..test_gen.cset_in_task {
            let mut cset = ChangeSet::new();
            let mut k = [0u8; 32 + 20];
            let mut v = [0u8; 32];
            let n = test_gen.wr_op_in_cset + test_gen.delete_op_in_cset;
            for j in 0..n {
                let mut x = start_num;
                if stride == u64::MAX {
                    x = rand_between_seg_idx(randsrc, 0, max_num as usize, n, j) as u64;
                } else {
                    start_num = (start_num + stride) % max_num;
                }
                let kh = test_gen.fill_kv(x, &mut k[..], &mut v[..]);
                if op_type == OP_READ && i == 0 {
                    // we need a least one write
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
