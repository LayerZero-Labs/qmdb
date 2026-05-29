//! Benchmark for TestGenV2::gen_block_as_subblocks
//!
//! This measures the maximum throughput of test vector generation.

use bench::speed::streaming_gen::{HoverConfig, StreamingGenConfig, StreamingTestGen};
use bench::speed::test_gen::TestGenV2;
use bench::RandSrc;
use std::time::Instant;

fn main() {
    println!("=== TestGenV2::gen_block_as_subblocks Benchmark ===\n");

    // Test configurations
    let configs = vec![
        // (entry_count, block_size, subblock_size, num_blocks, description)
        (
            10_000_000,
            10_000,
            1_000,
            50,
            "10M entries, 10K block, 1K subblock",
        ),
        (
            10_000_000,
            10_000,
            2_000,
            50,
            "10M entries, 10K block, 2K subblock",
        ),
        (
            50_000_000,
            10_000,
            1_000,
            50,
            "50M entries, 10K block, 1K subblock",
        ),
        (
            100_000_000,
            100_000,
            10_000,
            20,
            "100M entries, 100K block, 10K subblock",
        ),
        (
            1_000_000_000,
            100_000,
            10_000,
            20,
            "1B entries, 100K block, 10K subblock",
        ),
    ];

    for (entry_count, block_size, subblock_size, num_blocks, desc) in configs {
        run_benchmark(entry_count, block_size, subblock_size, num_blocks, desc);
    }

    // Round 0 vs Round 1 comparison (parallel vs serial)
    println!("\n=== Round 0 (parallel) vs Round 1 (serial) Comparison ===\n");
    run_round_comparison();
}

fn run_benchmark(
    entry_count: u64,
    block_size: usize,
    subblock_size: usize,
    num_blocks: usize,
    description: &str,
) {
    println!("--- {} ---", description);
    println!(
        "Config: entry_count={}, block_size={}, subblock_size={}, num_blocks={}",
        entry_count, block_size, subblock_size, num_blocks
    );

    // Create RandSrc and TestGenV2
    let mut randsrc = RandSrc::new("./randsrc.dat", "test-gen-bench");
    let cset_in_task = 2; // Standard: 2 changesets per task
    let mut test_gen = TestGenV2::new(&mut randsrc, entry_count, cset_in_task, block_size);

    // Warmup: generate 2 blocks
    for _ in 0..2 {
        let _ = test_gen.gen_block_as_subblocks(subblock_size);
    }

    // Benchmark
    let start = Instant::now();
    let mut total_tasks = 0usize;
    let mut total_subblocks = 0usize;
    let mut total_changesets = 0usize;

    for _ in 0..num_blocks {
        let subblocks = test_gen.gen_block_as_subblocks(subblock_size);
        total_subblocks += subblocks.len();
        for sb in &subblocks {
            for task in sb.iter() {
                total_tasks += 1;
                total_changesets += task.len();
            }
        }
    }

    let elapsed = start.elapsed();
    let elapsed_secs = elapsed.as_secs_f64();

    // Calculate throughput
    let blocks_per_sec = num_blocks as f64 / elapsed_secs;
    let tasks_per_sec = total_tasks as f64 / elapsed_secs;
    let changesets_per_sec = total_changesets as f64 / elapsed_secs;
    let subblocks_per_sec = total_subblocks as f64 / elapsed_secs;

    println!("Results:");
    println!("  Total time: {:.3}s", elapsed_secs);
    println!("  Blocks: {} ({:.1} blocks/s)", num_blocks, blocks_per_sec);
    println!(
        "  Subblocks: {} ({:.1} subblocks/s)",
        total_subblocks, subblocks_per_sec
    );
    println!(
        "  Tasks: {} ({:.1}K tasks/s)",
        total_tasks,
        tasks_per_sec / 1000.0
    );
    println!(
        "  ChangeSets: {} ({:.1}K csets/s)",
        total_changesets,
        changesets_per_sec / 1000.0
    );
    println!();
}

fn run_round_comparison() {
    // Simpler test: just measure Round 0 throughput with different configurations
    // (Round 1 test skipped because entering Round 1 requires generating entire Round 0 first)

    println!("Testing parallel generation (Round 0) with different worker counts simulation...\n");

    let entry_count = 100_000_000u64;
    let block_size = 100_000usize;
    let subblock_size = 10_000usize; // 10 subblocks per block
    let num_blocks = 10;

    println!(
        "Config: entry_count={}, block_size={}, subblock_size={}",
        entry_count, block_size, subblock_size
    );
    println!(
        "Blocks: {}, Subblocks per block: {}",
        num_blocks,
        block_size / subblock_size
    );
    println!();

    // Create generator
    let mut randsrc = RandSrc::new("./randsrc.dat", "round-comparison");
    let cset_in_task = 2;
    let mut test_gen = TestGenV2::new(&mut randsrc, entry_count, cset_in_task, block_size);

    // Warmup
    let _ = test_gen.gen_block_as_subblocks(subblock_size);

    // Benchmark
    let start = Instant::now();
    let mut total_tasks = 0usize;
    let mut total_subblocks = 0usize;

    for _ in 0..num_blocks {
        let subblocks = test_gen.gen_block_as_subblocks(subblock_size);
        total_subblocks += subblocks.len();
        for sb in &subblocks {
            total_tasks += sb.len();
        }
    }

    let elapsed = start.elapsed();
    let elapsed_secs = elapsed.as_secs_f64();
    let tasks_per_sec = total_tasks as f64 / elapsed_secs;

    println!("Results (Round 0, parallel with up to 8 workers):");
    println!("  Time: {:.3}s", elapsed_secs);
    println!(
        "  Tasks: {} ({:.1}K tasks/s)",
        total_tasks,
        tasks_per_sec / 1000.0
    );
    println!("  Subblocks: {}", total_subblocks);
    println!();

    // Summary
    println!("=== Summary ===");
    println!(
        "Maximum test generation throughput: ~{:.0}K tasks/s",
        tasks_per_sec / 1000.0
    );
    println!("This is the upper bound for top_speed when test gen is the bottleneck.");

    // Streaming generator comparison
    println!("\n=== Streaming Generator Benchmark ===\n");
    run_streaming_benchmark();
}

fn run_streaming_benchmark() {
    // Note: entry_count must be large enough for the number of blocks we test
    // With 10K block_size and 8 workers generating 50 blocks, we need at least
    // 50 * 8 * 10000 * 2 * 10 ≈ 80M entries to avoid create/delete collision
    // Using 100M to be safe.
    let configs = vec![
        (
            100_000_000u64,
            10_000usize,
            1_000usize,
            1,
            "100M, 10K block, 1 worker",
        ),
        (
            100_000_000u64,
            10_000usize,
            1_000usize,
            2,
            "100M, 10K block, 2 workers",
        ),
        (
            100_000_000u64,
            10_000usize,
            1_000usize,
            4,
            "100M, 10K block, 4 workers",
        ),
        (
            100_000_000u64,
            10_000usize,
            1_000usize,
            8,
            "100M, 10K block, 8 workers",
        ),
    ];

    for (entry_count, block_size, subblock_size, num_workers, desc) in configs {
        println!("--- {} ---", desc);

        let config = StreamingGenConfig {
            entry_count,
            cset_in_task: 2,
            block_size,
            subblock_size,
            num_workers,
            channel_capacity: 64,
            max_blocks: 50, // Limit to 50 blocks for benchmark
            hover: HoverConfig::default(),
        };

        let mut gen = StreamingTestGen::new("./randsrc.dat", config);
        let total_blocks = gen.total_pop_blocks().min(50); // Test first 50 blocks

        let start = Instant::now();
        let mut block_count = 0;
        let mut total_tasks = 0usize;

        while let Some(block) = gen.next_block() {
            block_count += 1;
            for sb in &block.subblocks {
                total_tasks += sb.len();
            }
            if block_count >= total_blocks {
                break;
            }
        }

        let elapsed = start.elapsed();
        let elapsed_secs = elapsed.as_secs_f64();
        let tasks_per_sec = total_tasks as f64 / elapsed_secs;
        let blocks_per_sec = block_count as f64 / elapsed_secs;

        println!("  Time: {:.3}s", elapsed_secs);
        println!("  Blocks: {} ({:.1} blocks/s)", block_count, blocks_per_sec);
        println!(
            "  Tasks: {} ({:.1}K tasks/s)",
            total_tasks,
            tasks_per_sec / 1000.0
        );
        println!();

        gen.shutdown();
    }
}
