# Benchmarking QMDB

The benchmark utility `speed` is used to benchmark QMDB. It is a command line tool that runs the benchmark and outputs
the results to a JSON file.

**Quick start**: run the benchmark with `./run.sh`, which contains default parameters and assumes your SSD is mounted at
`/mnt/nvme`.

## Initial setup

QMDB requires a source of entropy to generate the workload. We recommend generating this once for ease of debugging and
reproduction. You can use any large file, or generate it from `/dev/urandom`:

```bash
head -c 10M < /dev/urandom > randsrc.dat
```

## Running the benchmark (speed)

Assuming your SSD is mounted at `/mnt/nvme`:

```bash
ulimit -n 65535
# Runs the benchmark with 4 million entries in /tmp/QMDB
cargo run --bin speed -- --entry-count 4000000

# Runs the benchmark with 7 billion entries
cargo run --bin speed --release -- \
    --db-dir /mnt/nvme/QMDB \
    --entry-count 7000000000 \
    --ops-per-block 1000000 \
    --hover-recreate-block 100 \
    --hover-write-block 100 \
    --hover-interval 1000 \
    --tps-blocks 500
```

## Arguments for speed

We document key arguments to the benchmarking utility `speed` here. Refer to `src/cli.rs` for all arguments.

### Essential arguments to provide values for

- **`entry_count`**: Number of entries to populate the database with. Default is 500 million.
- **`db_dir`**: Directory to store the database's persistent files. You should set this to a directory on your SSD,
  e.g., `/mnt/nvme/QMDB`. Default is `/tmp/QMDB`.
- **`output_filename`**: Output file for benchmark results. Default is `results.json`.
- **`randsrc_filename`**: Source of randomness for workload generation. Default is `./randsrc.dat`.

### Benchmarking workload

These argments control the workload and can be adjusted accordingly or left as the default. QMDB supports pack multiple
transactions into one task, although its clients usually only have one transaction in a task. "Hover tasks" in the code
refers to benchmarking tasks run periodically during the population of the database.

- **`ops_per_block`**: Target number of operations per block. Default is 100,000.
- **`hover_interval`**: Interval (in blocks) between benchmarking tasks. Default is 10,000 blocks in release mode.
- **`hover_recreate_block`**: Number of blocks to insert during benchmarking. Default is 50 blocks in release mode.
- **`hover_write_block`**: Number of blocks to write during benchmarking. Default is 50 blocks in release mode.
- **`tps_blocks`**: When benchmarking transactions per second (TPS), this sets the number of blocks to run. Default is
  50 in release mode.
- **`changesets_per_task`**: Number of changesets per task. Each changeset corresponds to a transaction. Default is 2.

## Troubleshooting

- Ensure `ulimit` is set high enough to handle file descriptors.
- Verify the SSD is correctly mounted at `/mnt/nvme`.
- Ensure the `randsrc.dat` file is present
