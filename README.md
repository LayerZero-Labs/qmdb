# QMDB: Breaking the DRAM Barrier in Authenticated Data Structures — PVLDB Artifact

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![PVLDB Vol. 20](https://img.shields.io/badge/PVLDB-Vol.%2020-informational)

This repository is the artifact of the PVLDB paper *QMDB: Breaking the DRAM Barrier in Authenticated Data
Structures* (Sijie Cui, Ziyue Qiu, Isaac Zhang, Ryan Zarick, Daniel Wong, Thomas Kim, Mignon Li, Kelvin Wong,
George Amvrosiadis; Carnegie Mellon University and LayerZero Labs).

> [!WARNING]
> **This repository exists only to reproduce the paper's results. It is not for production use.**
> To keep the measurements reproducible, the code and its dependencies, including the vendored baselines, are pinned
> to the exact versions used in the evaluation. Some of these versions have known critical vulnerabilities. We keep
> them on purpose and will not update or patch them here, because changing them would change what the paper
> measured. Security findings against these pinned versions will not be addressed in this repository.

It contains:
- **`speed`**, a benchmark driver that runs QMDB and the three baselines (NOMT, LVMT, Reth) through one shared
  workload generator, so all systems see the same operations; it also replays the Ethereum mainnet trace (`bench/`),
- the **QMDB engine** used in the paper's evaluation (`crates/`); the production implementation is maintained at
  [LayerZero-Labs/qmdb](https://github.com/LayerZero-Labs/qmdb), and
  [Section 8.2](#82-qmdb) lists every difference between that code and the copy here,
- the **baselines** at the exact upstream commits we measured: NOMT and Reth vendored, NOMT with one documented patch,
  and LVMT as an unmodified git submodule (`nomt/`, `reth/`, `lvmt/`).

**Paper:** Proceedings of the VLDB Endowment, Vol. 20, 2027.

---

## Contents
1. [Repository layout](#1-repository-layout)
2. [Requirements](#2-requirements)
3. [Setup and build](#3-setup-and-build)
4. [Kick the tires](#4-kick-the-tires)
5. [Workloads and datasets](#5-workloads-and-datasets)
6. [Reproducing the paper's results](#6-reproducing-the-papers-results)
7. [Output and metrics](#7-output-and-metrics)
8. [Differences from upstream](#8-differences-from-upstream)
9. [Extending the harness](#9-extending-the-harness)
10. [Troubleshooting](#10-troubleshooting)
11. [License and citation](#11-license-and-citation)

---

## 1. Repository layout

```
.
├── crates/                  QMDB engine (Cargo workspace members)
│   ├── qmdb/                Merkle tree (twigs), indexers, entry files, prefetcher, updater, flusher, compactor
│   ├── qmdb-common/         shared types, change sets, compile-time constants (src/def.rs)
│   └── hpfile/              head-prunable files that back the entry and twig logs
├── bench/                   benchmark driver `speed` and one adapter per backend
│   ├── src/bin/speed.rs     populate -> CRUD "hover" phases -> optional Ethereum replay
│   ├── src/cli.rs           every command-line flag
│   ├── src/speed/           qmdb.rs, nomt_adapter.rs, lvmt_adapter.rs, reth_adapter.rs, eth_replay.rs, generators, metrics
│   └── Cargo.toml.{lvmt,reth}, Cargo.lock.{lvmt,reth}, switch_backend.sh   manifest switch between the LVMT and Reth builds (Section 3.4)
├── nomt/  reth/             vendored baselines (upstream commits and the NOMT patch in Section 8.1)
└── lvmt/                    git submodule: upstream LVMT, unmodified
```

---

## 2. Requirements

### Hardware

We recommend the machine used in the paper, AWS `c7gd.metal`:
- **CPU:** 64-core Graviton3 at 2.6 GHz
- **Memory:** 128 GiB DDR5
- **Storage:** 2 × 1.7 TiB local NVMe in RAID-0, ext4
- **OS:** Ubuntu 24.04, Linux 6.17 (io_uring and cgroup v2)

### Software

- Rust **1.94.0**, pinned by `rust-toolchain.toml` (rustup installs it on first build).
- A C/C++ toolchain for the native dependencies (RocksDB in LVMT, MDBX in Reth, jemalloc): GCC/G++ ≥ 13, clang, cmake, pkg-config.
- `sudo` (memory caps through `systemd-run`, dropping the page cache), systemd with cgroup v2.
- `git` with submodule support, `mdadm` for the RAID-0 array, and `python3-venv` for the Hugging Face CLI that downloads the Ethereum dataset.

```bash
sudo apt-get update
sudo apt-get install -y build-essential clang libclang-dev cmake pkg-config git mdadm python3-venv
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
```

---

## 3. Setup and build

### 3.1 Source

LVMT is a git submodule, so clone recursively (or run `git submodule update --init` in an existing clone):
```bash
git clone --recursive https://github.com/LayerZero-Labs/qmdb.git
cd qmdb
```

### 3.2 Storage

These commands set up the two local NVMe drives of `c7gd.metal` (`/dev/nvme1n1`, `/dev/nvme2n1`) as a RAID-0 array
mounted at `/mnt/ssd`.

```bash
sudo mdadm --create /dev/md0 --level=0 --raid-devices=2 /dev/nvme1n1 /dev/nvme2n1
sudo mkfs.ext4 -F /dev/md0
sudo mkdir -p /mnt/ssd && sudo mount /dev/md0 /mnt/ssd && sudo chown "$USER" /mnt/ssd
```

### 3.3 Workload seed

The synthetic workload is generated deterministically from a file of random bytes. Use the **same file for every
system** so that all of them see the same key stream.

```bash
head -c 10M </dev/urandom > /mnt/ssd/randsrc.dat
```

### 3.4 Building the backends

The backend is selected at **compile time** by a Cargo feature; without a feature, `speed` runs QMDB. LVMT and Reth
link incompatible versions of `zstd-sys`, so `bench/switch_backend.sh` swaps `bench/Cargo.toml` and the workspace
`Cargo.lock` between two variants before building (details in `bench/BACKEND_SWITCH.md`). Every build writes
`target/release/speed` at the repository root, so copy each binary out.

```bash
mkdir -p ~/bin
cd bench
# QMDB, NOMT and LVMT build with the LVMT manifest
./switch_backend.sh lvmt && cargo build --release --bin speed && cp ../target/release/speed ~/bin/speed_qmdb
cargo build --release --bin speed --features qmdb/use_hybridindexer && cp ../target/release/speed ~/bin/speed_qmdb_hybrid
cargo build --release --bin speed --features qmdb/pipeline_off && cp ../target/release/speed ~/bin/speed_qmdb_pipeoff
cargo build --release --bin speed --features use_nomt && cp ../target/release/speed ~/bin/speed_nomt
# RocksDB in LVMT needs these flags with GCC >= 13
CXXFLAGS="-fpermissive -std=c++17 -include cstdint" \
  cargo build --release --bin speed --features use_lvmt && cp ../target/release/speed ~/bin/speed_lvmt
# Reth needs its own manifest; switch back afterwards
./switch_backend.sh reth && cargo build --release --bin speed --features use_reth && cp ../target/release/speed ~/bin/speed_reth
./switch_backend.sh lvmt
cd ..
```

---

## 4. Kick the tires

These runs exercise every backend end to end on a small state (1 million entries). Run them from `bench/`, because
the LVMT adapter keeps its public parameters in `../lvmt/pp`; LVMT generates them on its first run.

```bash
# QMDB keeps more files open than the default limit of 1,024
ulimit -n 65535
ARGS="--ops-per-block 100000 --rayon-threads 16 --randsrc-filename /mnt/ssd/randsrc.dat"
# NOMT preallocates 4 KiB of SSD per bucket: 16 GB here
export NOMT_BUCKETS=4000000
# One run per backend: 1M entries, then 2-block Update, Read, Insert and Delete phases
for b in qmdb nomt lvmt reth; do
  mkdir -p /mnt/ssd/${b}_smoke
  ~/bin/speed_$b --db-dir /mnt/ssd/${b}_smoke --entry-count 1000000 --hover-interval 5 --hover-write-block 2 --hover-recreate-block 2 $ARGS
done
```

Each run ends with the phase reports and a results summary. The QMDB run prints,
for example (numbers will differ):

```
Hover Test updates (200000 ops, 2 blocks, stride=random)...updates@1000000 entries: 200000 in 0.26s (781397.95/s), total: 400000 in 0.50s (806517.80/s)
...
TEST_REPORT: Phase=updates | Ops=200000
  > Total KV Size (Pure)  = 10400000 bytes (9.92 MB)
  [OS-level Disk IO]
  > OS Physical Write = 25120768 bytes (23.96 MB)
  > OS Physical Read  = 253952 bytes (0.24 MB)
...
MEM(entries=1000000) Memory usage: physical=0.93GB virtual=10.53GB; physical/entry=996.09bytes
SSD(entries=1000000) Usage: 0.33GB, 352.18bytes/entry
...
Results summary
	entry_count: 1000000
	db_backend: QMDB
	...
	deletes: 766338.719/s
	inserts: 772736.210/s
	reads: 871625.182/s
	updates: 781397.947/s
```

A run is healthy if it prints `TEST_REPORT` lines for `updates`, `reads`, `inserts` and `deletes` and no `panicked` message.
Delete the smoke databases afterwards: `rm -rf /mnt/ssd/*_smoke`.

---

## 5. Workloads and datasets

### 5.1 Synthetic CRUD workload

`speed` first **populates** the database block by block. Every `--hover-interval` blocks it pauses population and
runs **hover** phases that time Update and Read throughput; after population a final hover also times Insert and Delete.

| Flag | Meaning | Default |
|---|---|---|
| `--entry-count` | entries to populate; must be a multiple of `--ops-per-block` | 500,000,000 |
| `--ops-per-block` | operations per block | 100,000 |
| `--hover-interval` | blocks of population between hovers | 10,000 |
| `--hover-write-block` | blocks in each Update and Read phase | 50 (5M operations) |
| `--hover-recreate-block` | blocks in each Insert and Delete phase | 50 (5M operations) |
| `--hover-zipf-theta` | key distribution: 0 is uniform, 0.99 is YCSB-style skew, above 1 is extreme skew; not exactly 1.0 | 0 |
| `--randsrc-filename` | file of random bytes from which all workload randomness is drawn | `./randsrc.dat` |

All flags are documented in `bench/src/cli.rs`.

### 5.2 Ethereum mainnet replay

The replay uses the state operations of Ethereum mainnet blocks 15,012,300 – 15,312,299 (2022-06-23 to 2022-08-10):
1,002,507,459 state operations, 60.2% of them Reads. Each block's operations are stored as
`{height}_operation_kv.bin` (bincode). The initial state at block 15,012,300 is packed into an **InitDbBundle**
that `speed` streams in before the replay starts.

The dataset is hosted on Hugging Face at
[usiie/qmdb-eth-replay](https://huggingface.co/datasets/usiie/qmdb-eth-replay):

| File | Size | SHA-256 | Contents |
|---|---|---|---|
| `traces_op_300k.tar` | 77,031,680,000 bytes | `cfd54b95900d9847856e23a9dbf7f77cf904eb0a2791e6ea55da1b4ac4243f94` | 265,397 per-block `{height}_operation_kv.bin` files (73 GiB) |
| `init_db_bundle_15012300_20407400.bin` | 99,862,323,012 bytes | `a31560b3353fdd9e94e9576bba3feaa5e6fcb98576abcebac2ff1e0553de92a3` | InitDbBundle, 63,830,024 initial entries |

Download, verify and extract (≈ 260 GB of disk at the peak; `hf` comes with the `huggingface_hub` Python package,
which on Ubuntu 24.04 goes into a virtual environment):
```bash
python3 -m venv ~/hfenv && ~/hfenv/bin/pip install -U huggingface_hub
~/hfenv/bin/hf download usiie/qmdb-eth-replay --repo-type dataset --local-dir /mnt/ssd/init_db_output
cd /mnt/ssd/init_db_output && sha256sum -c SHA256SUMS
mkdir -p /mnt/ssd/real_eth_workload/kv_decompressed
tar xf traces_op_300k.tar -C /mnt/ssd/real_eth_workload/kv_decompressed && rm traces_op_300k.tar
```
The commands in Section 6.3 expect these two paths.

---

## 6. Reproducing the paper's results

### 6.1 Measurement protocol

- **Machine:** `c7gd.metal`; the RAID-0 array mounted at `/mnt/ssd`. The Ethereum replay also runs on `c6id.16xlarge`
  and `r6gd.2xlarge` (Section 6.3).
- **Memory:** a 32 GiB cgroup-v2 cap (`MemoryMax=32G`, `MemoryHigh=32G`) that limits allocations and the page cache:
  `sudo systemd-run --scope -p MemoryMax=32G -p MemoryHigh=32G ...`. The Ethereum replay sets the cap only after the
  initial state is loaded (Section 6.3).
- **CPUs:** all 64 cores (the commands below pin them with `taskset -c 0-63`; Section 6.3 lists the CPU sets of the
  other two instances). File descriptors: ≥ 65,535 (1,048,576 for NOMT in Section 6.2), set with `prlimit` inside the
  scope.
- **Before each run:** delete the database directory and drop the page cache:
  `rm -rf /mnt/ssd/<db> && sync && echo 3 | sudo tee /proc/sys/vm/drop_caches`.

### 6.2 Database-size sweep (Fig. 5 and Fig. 8)

Uniform CRUD at 100M, 300M, 600M, 900M, 1.2B and 1.5B entries, 32 GiB cap. Fig. 8 uses the memory, SSD and CPU
lines printed by the same runs.

| System | Settings |
|---|---|
| QMDB | 16 shards and compaction threshold 0.7 (source defaults `SHARD_COUNT = 16` and `UTILIZATION_RATIO = 7` in `crates/qmdb-common/src/def.rs`); `--rayon-threads 128` |
| NOMT | page cache 16 GiB, leaf cache 10 GiB, commit concurrency 8, 32 IO workers, 2 upper levels prepopulated; buckets 39,073,369 (100M), 93,329,983 (300M), 248,354,236 (600M), 575,546,222 (≥ 900M); `--rayon-threads 64` |
| LVMT | RocksDB memory budget 2 GiB, 16 shards |
| Reth | default MDBX settings; 100M–600M only |

QMDB (from `bench/`):
```bash
for n in 100000000 300000000 600000000 900000000 1200000000 1500000000; do
  rm -rf /mnt/ssd/QMDB && sync && echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null
  sudo systemd-run --scope -p MemoryMax=32G -p MemoryHigh=32G \
    prlimit --nofile=65535:65535 taskset -c 0-63 \
    ~/bin/speed_qmdb --db-dir /mnt/ssd/QMDB --entry-count $n --ops-per-block 100000 \
      --hover-write-block 500 --hover-recreate-block 500 --hover-interval 30000 \
      --rayon-threads 128 --randsrc-filename /mnt/ssd/randsrc.dat \
    2>&1 | tee qmdb_${n}.log
done
```

Baselines (from `bench/`; example: NOMT at 1.5B; replace the binary for LVMT and Reth):
```bash
rm -rf /mnt/ssd/NOMT && sync && echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null
sudo systemd-run --scope -p MemoryMax=32G -p MemoryHigh=32G \
  env NOMT_BUCKETS=575546222 prlimit --nofile=1048576:1048576 taskset -c 0-63 \
  ~/bin/speed_nomt --db-dir /mnt/ssd/NOMT --entry-count 1500000000 --ops-per-block 100000 \
    --hover-write-block 500 --hover-recreate-block 500 --hover-interval 30000 \
    --rayon-threads 64 --randsrc-filename /mnt/ssd/randsrc.dat \
  2>&1 | tee nomt_1500M.log
```
NOMT reads its bucket count from `NOMT_BUCKETS` (values in the table above; the default is 575,546,222) and preallocates
4 KiB of SSD per bucket, about 2.4 TB at 575,546,222 buckets. Its page and leaf caches are fixed at 16 GiB and 10 GiB in
`bench/src/speed/nomt_adapter.rs`.

### 6.3 Ethereum mainnet replay (Fig. 6)

Each system loads the InitDbBundle without a memory cap and then replays the window under the 32 GiB cap; the result
is the replay's wall-clock time. The cap is set on the run's scope with `systemctl set-property` when `speed` prints
`=== Phase 2`, before the timed replay starts. All four systems run with `--rayon-threads 128`,
`prlimit --nofile=65535:65535` and these environment variables:
- `QMDB_BIGBUF_MEMSET=1` (QMDB only): zero QMDB's large buffers with memset instead of `calloc`; for `calloc`, jemalloc
  zeroes recycled memory with `madvise(MADV_DONTNEED)`, whose TLB-shootdown interrupts stall the other threads;
- `ETH_PREFETCH=256`: number of trace files loaded concurrently (default 100);
- `ETH_PAR_LOAD=1`: read and decode the trace files on Tokio's blocking thread pool, so that the loads run in parallel;
- `ETH_PAR_PACK=8`: pack the operations into DB blocks on 8 threads and deliver them in order; the blocks are the same
  as with the serial packer (variable unset);
- `NOMT_BUCKETS=93329983` (NOMT only): a hashtable of about 382 GB, preallocated on the SSD.

The `ETH_*` variables configure the benchmark's trace front end, which is shared by all systems. `QMDB_BIGBUF_MEMSET`
and `ETH_PAR_LOAD` take effect when set to any value; leave them unset to disable them.

```bash
# from bench/; SYS is qmdb, nomt, lvmt or reth
SYS=qmdb
sudo rm -rf /mnt/ssd/eth_db* && sync && echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null
sudo systemd-run --scope --collect --unit=eth-$SYS -p MemoryAccounting=yes \
  env QMDB_BIGBUF_MEMSET=1 ETH_PAR_PACK=8 ETH_PAR_LOAD=1 ETH_PREFETCH=256 NOMT_BUCKETS=93329983 \
  prlimit --nofile=65535:65535 taskset -c 0-63 \
  ~/bin/speed_$SYS --db-dir /mnt/ssd/eth_db --entry-count 1500000000 --ops-per-block 100000 \
    --hover-zipf-theta 0 --hover-write-block 50 --hover-recreate-block 50 --hover-interval 30000 \
    --rayon-threads 128 --randsrc-filename /mnt/ssd/randsrc.dat \
    --eth-trace-dir /mnt/ssd/real_eth_workload/kv_decompressed \
    --init-db-path /mnt/ssd/init_db_output/init_db_bundle_15012300_20407400.bin \
    --eth-replay-start-height 15012300 --eth-replay-end-height 15312300 \
  > ${SYS}_eth.log 2>&1 &
PID=$!
# cap the scope at 32 GiB once the initial state is loaded, before the timed replay starts
until grep -q "=== Phase 2" ${SYS}_eth.log || [ ! -d /proc/$PID ]; do sleep 0.5; done
sudo systemctl set-property --runtime eth-$SYS.scope MemoryMax=32G MemoryHigh=32G
wait $PID
```

`--eth-replay-end-height` is inclusive; block 15,312,300 adds no operations, so the replay covers the 1,002,507,459
operations of Section 5.2. The replay does not use `--entry-count`, `--ops-per-block`, `--randsrc-filename` or the
hover flags.

The run ends with `Benchmark Finished.`, `Total Ops: N` and `Duration: Ts`. Throughput is N / T operations per second;
the paper converts it to transactions per second with 17.91 state operations per transaction (1,002,507,459 / 55,965,021).
Every system prints `Total Ops: 1002507459`. QMDB then flushes, outside the timed region, and prints its 16 shard roots
(`root shard=i ...`) and their digest, which is the same on every machine:
```
state_root_digest height=8933 45d8631819a032b4f4f25826ca2414d6bac62046f54ae2a63b915bfd456a59d3
```

Fig. 6 uses three instances with the same command except for the CPU list (and, on `r6gd.2xlarge`, the input paths):
- `c7gd.metal` (Graviton3): `taskset -c 0-63`;
- `c6id.16xlarge` (x86): `taskset -c 0-31`, one hyperthread per physical core (SMT unused);
- `r6gd.2xlarge` (Graviton2): `taskset -c 0-7`. It has a single NVMe drive, formatted as ext4 and mounted at `/mnt/ssd`
  without RAID (`sudo mkfs.ext4 -F /dev/nvme1n1 && sudo mount /dev/nvme1n1 /mnt/ssd`). This 441 GiB drive holds the
  databases but cannot also hold NOMT's 382 GB hashtable and the 177 GB of inputs, so for all four systems the bundle
  and the traces are read from a separate EBS gp3 volume (point `--init-db-path` and `--eth-trace-dir` at it).

### 6.4 QMDB-only sweeps (Fig. 7, Fig. 9, Fig. 10)

All at 1.5B entries, with the QMDB command of 6.2 (`--entry-count 1500000000`).

| Figure | What varies | How |
|---|---|---|
| Fig. 7 | Zipfian θ = 0, 0.2, 0.4, 0.6, 0.8, 1.01, 1.2, 1.4, 1.5 | `--hover-zipf-theta θ` |
| Fig. 9 | compaction threshold 0.4 – 1.0 | `UTILIZATION_RATIO` = 4 … 10 in `crates/qmdb-common/src/def.rs`, then rebuild |
| Fig. 10 | shards 1, 2, 4, 8, 16, 32 | `SHARD_COUNT` in `crates/qmdb-common/src/def.rs`, then rebuild |
| Fig. 10, pipelining off | shards as above | build with `--features qmdb/pipeline_off` (Section 3.4) in addition to `SHARD_COUNT` |

The two compile-time constants can be set with `sed`; restore the defaults (7 and 16) afterwards:
```bash
sed -E -i 's/pub const UTILIZATION_RATIO: i64 = [0-9]+;/pub const UTILIZATION_RATIO: i64 = 4;/' crates/qmdb-common/src/def.rs
sed -E -i 's/pub const SHARD_COUNT: usize = [0-9]+;/pub const SHARD_COUNT: usize = 1;/' crates/qmdb-common/src/def.rs
```

`pipeline_off` makes the engine wait until the previous block has finished before it starts the next one
(pipeline depth 1).

---

## 7. Output and metrics

All results are printed to stdout.

| Output | Meaning |
|---|---|
| `TEST_REPORT: Phase=<phase> \| Ops=<n>` | one hover phase (`updates`, `reads`, `inserts`, `deletes`) finished |
| `> OS Physical Write/Read = N bytes` | bytes the phase wrote and read on the device that holds `--db-dir` (from `/proc/diskstats`, whole RAID array; override the device with `--monitor-disk`) |
| `<phase>@<entries> entries: N in Ts (X/s), total: ...` | throughput of one phase, and of all phases of that kind so far |
| `MEM(entries=N) Memory usage: physical=X GB` | resident memory of the process after N entries |
| `SSD(entries=N) Usage: X GB, Y bytes/entry` | on-disk size of the database, total and per entry |
| `CPU(entries=N) User time ... Utilization: X %` | CPU time used by the process and its average utilization |
| `Results summary` … `<phase>: X/s` | throughput of the last phase of each kind, at the end of the run |
| `Replay [x%] Block N \| TPS: ...` | Ethereum replay progress |
| `Benchmark Finished.`, `Total Ops: N`, `Duration: Ts` | end of an Ethereum replay |
| `root shard=i <hash>`, `state_root_digest height=H <hash>` | QMDB only: the shard Merkle roots after an Ethereum replay and their BLAKE3 digest (not timed) |

---

## 8. Differences from upstream

### 8.1 Baselines

| System | Upstream | Commit | License | In this repository |
|---|---|---|---|---|
| NOMT | [thrumdev/nomt](https://github.com/thrumdev/nomt) | `5fa289a` (2025-12-07) | MIT OR Apache-2.0 | `nomt/`, one patch |
| LVMT | [ChenxingLi/authenticated-storage-benchmarks](https://github.com/ChenxingLi/authenticated-storage-benchmarks) | `c22ff82` (2023-07-06) | see upstream | `lvmt/`, git submodule, unmodified |
| Reth | [paradigmxyz/reth](https://github.com/paradigmxyz/reth) | `1ba631b` (2024-10-10) | MIT OR Apache-2.0 | `reth/`, unmodified |

- **NOMT patch (`nomt/nomt/src/beatree/leaf_cache.rs`):** `insert()` evicts least-recently-used leaves whenever the cache
  exceeds its capacity. Upstream trims the cache only after a sync, so leaves loaded by Reads between syncs grow it
  without bound and exceed the memory cap. The patch changes caching only, not values or roots.
- **One write per key per block:** when a block writes a key several times, the adapters pass only the last write to
  NOMT and LVMT (LVMT's upstream code panics when one key is written many times in an epoch, which the Ethereum trace
  does). The harness builds only `nomt/nomt`, `nomt/core`, `lvmt/asb-authdb/lvmt-db` and
  `lvmt/asb-backend/cfx-kvdb-rocksdb`.

### 8.2 QMDB

The engine in `crates/` differs from [LayerZero-Labs/qmdb](https://github.com/LayerZero-Labs/qmdb) at `f14a2a0` in:
- prefetcher job pool of 40,000 jobs instead of 4,000 (`crates/qmdb/src/prefetcher/mod.rs`);
- io_uring completion waiting: when work is queued but every buffer is busy, a worker waits for one completion
  instead of up to 32 (`crates/qmdb/src/prefetcher/dioprefetcher.rs`);
- with `QMDB_BIGBUF_MEMSET` set, large buffers are zeroed with memset instead of `calloc`
  (`crates/qmdb-common/src/utils/mod.rs`; off by default, Section 6.3);
- pruned entry-file segments are released from the io_uring registered-file tables, so their disk space is freed
  (`PruneCmd` from the flusher to the prefetcher workers);
- the `pipeline_off` feature for the pipelining ablation (off by default);
- debug instrumentation, commented-out code and LayerZero's RocksDB reference-model fuzz example were removed.

The first three change performance only; none of the changes affects Merkle roots, proofs or durability.

---

## 9. Extending the harness

A backend implements six functions, and `speed` calls them through `bench/src/speed/db_backend.rs`:

```rust
pub fn init(db_dir: &str);
pub fn create_kv(height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>);
pub fn update_kv(height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>);
pub fn delete_kv(height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>);
pub fn read_kv(/* ... */);
pub fn flush();
```

Each task holds change sets; iterate them with `for_each_op(|op_type, _shard, key_hash, key, value, _old, _rec| ...)`
and dispatch on `OP_CREATE`, `OP_WRITE`, `OP_DELETE` and `OP_READ` from `qmdb_common::def`. To add a system:
1. write `bench/src/speed/<sys>_adapter.rs` (`nomt_adapter.rs` is a template for a Merkle trie);
2. register it in `bench/src/speed/mod.rs` under `#[cfg(feature = "use_<sys>")]`;
3. add a `NAME` constant and the six wrappers to `bench/src/speed/db_backend.rs`;
4. add the feature and a path dependency to `bench/Cargo.toml` (and to `Cargo.toml.lvmt`/`.reth` if native libraries clash).

---

## 10. Troubleshooting

- **`Too many open files`:** raise the limit (`ulimit -n 65535`). Under `systemd-run`, set it inside the scope
  (`prlimit --nofile=...`); `-p LimitNOFILE` is rejected for scopes.
- **`lvmt/` is empty:** the submodule was not fetched; run `git submodule update --init`.
- **LVMT fails to build:** set `CXXFLAGS="-fpermissive -std=c++17 -include cstdint"` (RocksDB with GCC ≥ 13).
- **LVMT cannot find its parameters:** run `speed_lvmt` from `bench/` so that `../lvmt/pp` resolves.
- **Cargo resolution error mentioning `links = "zstd"`:** run `bench/switch_backend.sh lvmt` or `reth` before building.
- **Panic at start-up:** `--entry-count` must be a multiple of `--ops-per-block`.
- **Known build failure:** the QMDB feature `tee_cipher` does not compile; it is not used in the paper.

---

## 11. License and citation

| Path | License | Origin |
|---|---|---|
| repository root (`LICENSE`) | MIT | as in LayerZero-Labs/qmdb |
| `crates/` | MIT OR Apache-2.0 | QMDB engine from LayerZero-Labs/qmdb, with the changes in Section 8.2 |
| `bench/` | Apache-2.0 (`bench/LICENSE`) | benchmark driver from LayerZero-Labs/qmdb, extended with the baseline adapters |
| `nomt/`, `reth/`, `lvmt/` | their own licenses | Section 8.1 |

```bibtex
@article{cui2027qmdb,
  author  = {Sijie Cui and Ziyue Qiu and Isaac Zhang and Ryan Zarick and Daniel Wong and Thomas Kim and Mignon Li and Kelvin Wong and George Amvrosiadis},
  title   = {{QMDB}: Breaking the {DRAM} Barrier in Authenticated Data Structures},
  journal = {Proceedings of the VLDB Endowment},
  volume  = {20},
  year    = {2027}
}
```

Questions and problems: please open a GitHub issue.
