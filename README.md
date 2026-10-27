# QMDB: Breaking the DRAM Barrier in Authenticated Data Structures — PVLDB Artifact

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![PVLDB Vol. 20](https://img.shields.io/badge/PVLDB-Vol.%2020-informational)

This repository is the artifact of the PVLDB paper *QMDB: Breaking the DRAM Barrier in Authenticated Data
Structures* (Sijie Cui, Ziyue Qiu, Isaac Zhang, Ryan Zarick, Daniel Wong, Thomas Kim, Mignon Li, Kelvin Wong,
George Amvrosiadis; Carnegie Mellon University and LayerZero Labs).

It contains:
- **`speed`**, a benchmark driver that runs QMDB and the three baselines (NOMT, LVMT, Reth) through one shared
  workload generator, so all systems see the same operations; it also replays the Ethereum mainnet trace (`bench/`),
- the **QMDB engine** used in the paper's evaluation (`crates/`); the production implementation is maintained at
  [LayerZero-Labs/qmdb](https://github.com/LayerZero-Labs/qmdb), and
  [Section 8](#8-baselines-patches-and-differences-from-upstream) lists every difference between that code and the copy here,
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
8. [Baselines, patches and differences from upstream](#8-baselines-patches-and-differences-from-upstream)
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
│   └── Cargo.toml.{lvmt,reth}, Cargo.lock.{lvmt,reth}, switch_backend.sh   manifest switch between the LVMT and Reth builds (Section 3)
├── nomt/  reth/             vendored baselines (upstream commits and the NOMT patch in Section 8)
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
git clone --recursive https://github.com/CMU-SijieC/QMDB-Benchmark.git
cd QMDB-Benchmark
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
`./target/release/speed`, so copy each binary out.

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
```

---

## 4. Kick the tires

These runs exercise every backend end to end on a small state (1 million entries). Run them from `bench/`, because
the LVMT adapter keeps its public parameters in `../lvmt/pp`; LVMT generates them on its first run.

```bash
cd bench
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
Hover Test updates (500000 ops, 5 blocks, stride=random)...
TEST_REPORT: Phase=updates | Ops=500000
  > OS Physical Write = 288620544 bytes (275.25 MB)
  > OS Physical Read  = 2364030976 bytes (2254.52 MB)
...
MEM(entries=2000000) Memory usage: physical=1.02GB virtual=11.06GB; physical/entry=547.45bytes
SSD(entries=2000000) Usage: 0.49GB, 0.26bytes/entry
...
Results summary
	entry_count: 2000000
	db_backend: qmdb
	...
	deletes: 244889.148/s
	inserts: 560151.804/s
	reads: 1019287.822/s
	updates: 1015923.789/s
```

A run is healthy if it prints `TEST_REPORT` lines for `updates`, `reads`, `inserts` and `deletes` and no `panicked` message.
Delete the smoke databases afterwards: `rm -rf /mnt/ssd/*_smoke`.

---

## 5. Workloads and datasets

### 5.1 Synthetic CRUD workload

`speed` first **populates** `--entry-count` entries in blocks of `--ops-per-block` operations (default 100,000).
Every `--hover-interval` blocks it pauses population and runs **hover** phases that time Update and Read throughput;
after population a final hover also times Insert and Delete. Each Update/Read phase is `--hover-write-block` blocks and
each Insert/Delete phase is `--hover-recreate-block` blocks, so with 100,000 operations per block a value of 50 gives
5M-operation phases and 500 gives 50M. Keys are uniform by default; `--hover-zipf-theta θ` draws them from a Zipfian
distribution (0.99 is YCSB-style skew; θ > 1 is extreme skew). All randomness comes from `--randsrc-filename`.

`--entry-count` must be a multiple of `--ops-per-block`. All flags are documented in `bench/src/cli.rs`.

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

- **Machine:** `c7gd.metal` unless noted; the RAID-0 array mounted at `/mnt/ssd`.
- **Memory:** a 32 GiB cgroup-v2 cap (`MemoryMax=32G`, `MemoryHigh=32G`) that limits allocations and the page cache:
  `sudo systemd-run --scope -p MemoryMax=32G -p MemoryHigh=32G ...`.
- **Before each run:** delete the database directory and drop the page cache:
  `rm -rf /mnt/ssd/<db> && sync && echo 3 | sudo tee /proc/sys/vm/drop_caches`.
- **CPUs:** all 64 cores (the commands below pin them with `taskset -c 0-63`). File descriptors: ≥ 65,535 (1,048,576 for NOMT),
  set with `prlimit` inside the scope.
- **Repetitions:** QMDB in Fig. 5 is the mean of 3 runs with the standard deviation as error bars; other points are single runs.
- **QMDB configuration:** source defaults, i.e. 16 shards and compaction threshold 0.7 (`SHARD_COUNT`, `UTILIZATION_RATIO` in `crates/qmdb-common/src/def.rs`).

Absolute throughput depends on the SSDs and CPU of your machine; the ordering between systems and the trends across
state sizes should hold on comparable hardware.

### 6.2 Database-size sweep (Fig. 5 and Fig. 8)

Uniform CRUD at 100M, 300M, 600M, 900M, 1.2B and 1.5B entries, 32 GiB cap. Fig. 8 uses the memory, SSD and CPU
lines printed by the same runs.

| System | Settings |
|---|---|
| QMDB | 16 shards, threshold 0.7, `--rayon-threads 128`; run the sweep three times |
| NOMT | page cache 16 GiB, leaf cache 10 GiB, commit concurrency 8, 32 IO workers, 2 upper levels prepopulated; buckets 39,073,369 (100M), 93,329,983 (300M), 248,354,236 (600M), 575,546,222 (≥ 900M); `--rayon-threads 64` |
| LVMT | RocksDB memory budget 2 GiB, 16 shards |
| Reth | default MDBX settings; 100M–600M only |

QMDB (from `bench/`):
```bash
cd bench
for n in 100000000 300000000 600000000 900000000 1200000000 1500000000; do
  for r in 1 2 3; do
    rm -rf /mnt/ssd/QMDB && sync && echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null
    sudo systemd-run --scope -p MemoryMax=32G -p MemoryHigh=32G \
      prlimit --nofile=65535:65535 taskset -c 0-63 \
      ~/bin/speed_qmdb --db-dir /mnt/ssd/QMDB --entry-count $n --ops-per-block 100000 \
        --hover-interval 30000 --rayon-threads 128 --randsrc-filename /mnt/ssd/randsrc.dat \
      2>&1 | tee qmdb_${n}_r${r}.log
  done
done
```

Baselines (example: NOMT at 1.5B; replace the binary for LVMT and Reth):
```bash
cd bench
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

Each system loads the InitDbBundle and replays the window; the result is the replay's wall-clock time.

```bash
# from bench/; the same flags for speed_qmdb, speed_lvmt and speed_reth
sudo systemd-run --scope -p MemoryMax=32G -p MemoryHigh=32G \
  prlimit --nofile=1048576:1048576 taskset -c 0-63 \
  ~/bin/speed_nomt --db-dir /mnt/ssd/NOMT --entry-count 1500000000 --ops-per-block 100000 \
    --hover-write-block 0 --hover-recreate-block 0 --hover-interval 1000000 \
    --eth-trace-dir /mnt/ssd/real_eth_workload/kv_decompressed \
    --init-db-path /mnt/ssd/init_db_output/init_db_bundle_15012300_20407400.bin \
    --eth-replay-start-height 15012300 --eth-replay-end-height 15312299 \
    --randsrc-filename /mnt/ssd/randsrc.dat 2>&1 | tee nomt_eth.log
```

The run ends with `Benchmark Finished.`, `Total Ops: N` and `Duration: Ts`. Throughput is N / T operations per second;
the paper converts it to transactions per second with 17.91 state operations per transaction (1,002,507,459 / 55,965,021).

On `r6gd.2xlarge`, pin the run to the eight vCPUs (`taskset -c 0-7`) and use the same 32 GiB cap.

### 6.4 QMDB-only sweeps (Fig. 7, Fig. 9, Fig. 10)

All at 1.5B entries, with the QMDB command of 6.2 (`--entry-count 1500000000`). The paper's runs for these figures
used 500-block phases; add `--hover-write-block 500 --hover-recreate-block 500` to match.

| Figure | What varies | How |
|---|---|---|
| Fig. 7 | Zipfian θ = 0, 0.2, 0.4, 0.6, 0.8, 1.01, 1.2, 1.4, 1.5 | `--hover-zipf-theta θ` |
| Fig. 9 | compaction threshold 0.4 – 1.0 | `UTILIZATION_RATIO` = 4 … 10 in `crates/qmdb-common/src/def.rs`, then rebuild |
| Fig. 10 | shards 1, 2, 4, 8, 16, 32 | `SHARD_COUNT` in `crates/qmdb-common/src/def.rs`, then rebuild |
| Fig. 10, pipelining off | shards as above | build with `--features qmdb/pipeline_off` (3.4) in addition to `SHARD_COUNT` |

The two compile-time constants can be set with `sed`; restore the defaults (7 and 16) afterwards:
```bash
sed -E -i 's/pub const UTILIZATION_RATIO: i64 = [0-9]+;/pub const UTILIZATION_RATIO: i64 = 4;/' crates/qmdb-common/src/def.rs
sed -E -i 's/pub const SHARD_COUNT: usize = [0-9]+;/pub const SHARD_COUNT: usize = 1;/' crates/qmdb-common/src/def.rs
```

`pipeline_off` makes the engine wait until the previous block has finished before it starts the next one
(pipeline depth 1).

### 6.5 Results stated in the text

| Result | Section | How to reproduce |
|---|---|---|
| x86 `c6id.16xlarge` within 1–2% of `c7gd.metal` | §5 setup | The QMDB 1.5B run of 6.2 on `c6id.16xlarge`, 3 runs |
| Entry-level `r6gd.2xlarge` | §5.1, Fig. 6 | 6.3 on `r6gd.2xlarge` |
| Read/Update mix: raising the Read fraction from 0 to 1 raises throughput by only about 8% | §5.1 | Run the QMDB 1.5B command of 6.2 with `env HOVER_RW_RATIOS=0.0,0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8,0.9,1.0` inside the scope. The final hover skips Insert and Delete and runs one mixed phase per ratio (each operation is a Read with that probability, otherwise an Update), reported as `rw_read00` … `rw_read10` |
| Hybrid indexer: 10.7 → 1.5 bytes per entry; throughput under 32 GiB and 8 GiB caps; queue depth | §6 | Build with the hybrid indexer (3.4) and vary `MemoryMax`/`MemoryHigh` |
| Recovery estimate | §6 | Analytical, from the 5 GB/s sequential read throughput of `c7gd.metal` |

---

## 7. Output and metrics

All results are printed to stdout.

| Output | Meaning |
|---|---|
| `TEST_REPORT: Phase=<phase> \| Ops=<n>` | one hover phase (`updates`, `reads`, `inserts`, `deletes`) finished |
| `> OS Physical Write/Read = N bytes` | bytes the phase wrote and read on the device that holds `--db-dir` (from `/proc/diskstats`, whole RAID array; override the device with `--monitor-disk`) |
| `<phase>@<entries> entries: N in Ts (X/s), total: ...` | throughput of one phase, and of all phases of that kind so far |
| `MEM(entries=N) Memory usage: physical=X GB` | resident memory of the process after N entries |
| `SSD(entries=N) Usage: X GB` | on-disk size of the database |
| `CPU(entries=N) User time ... Utilization: X %` | CPU time used by the process and its average utilization |
| `Results summary` … `<phase>: X/s` | throughput of the last phase of each kind, at the end of the run |
| `Replay [x%] Block N \| TPS: ...` | Ethereum replay progress |
| `Benchmark Finished.`, `Total Ops: N`, `Duration: Ts` | end of an Ethereum replay |

QMDB's Read phases stop their timer at the prefetcher's last SSD read (no new read for 40 ms), not when `flush()`
returns: the updater treats Reads as no-ops, so `flush()` returns while the io_uring prefetcher is still reading.

---

## 8. Baselines, patches and differences from upstream

| System | Upstream | Commit | License | In this repository |
|---|---|---|---|---|
| NOMT | [thrumdev/nomt](https://github.com/thrumdev/nomt) | `5fa289a` (2025-12-07) | MIT or Apache-2.0 | `nomt/`, one patch |
| LVMT | [ChenxingLi/authenticated-storage-benchmarks](https://github.com/ChenxingLi/authenticated-storage-benchmarks) | `c22ff82` (2023-07-06) | see upstream | `lvmt/`, git submodule, unmodified |
| Reth | [paradigmxyz/reth](https://github.com/paradigmxyz/reth) | `1ba631b` (2024-10-10) | MIT or Apache-2.0 | `reth/`, unmodified |

`nomt/` and `reth/` are identical to their upstream commits except for Reth's `book/cli/reth/debug/` documentation
(excluded by `.gitignore`) and one patch:
- **NOMT, `nomt/nomt/src/beatree/leaf_cache.rs`:** `insert()` evicts least-recently-used leaves whenever the cache
  exceeds its capacity. Upstream trims the cache only after a sync, so leaves loaded by Reads between syncs grow it
  without bound and exceed the memory cap. The patch changes caching only, not values or roots.

The adapters apply one write per key per block: when a block writes a key several times, NOMT and LVMT receive only
the last write (LVMT's upstream code panics when one key is written many times in an epoch, which the Ethereum trace
does). The harness builds only `nomt/nomt`, `nomt/core`, `lvmt/asb-authdb/lvmt-db` and
`lvmt/asb-backend/cfx-kvdb-rocksdb`.

**QMDB here vs LayerZero-Labs/qmdb `f14a2a0`.** The engine in `crates/` differs from the public code in:
- prefetcher job pool of 40,000 jobs instead of 4,000 (`crates/qmdb/src/prefetcher/mod.rs`);
- io_uring completion waiting: when work is queued but every buffer is busy, a worker waits for one completion
  instead of up to 32 (`crates/qmdb/src/prefetcher/dioprefetcher.rs`);
- pruned entry-file segments are released from the io_uring registered-file tables, so their disk space is freed
  (`PruneCmd` from the flusher to the prefetcher workers);
- the `pipeline_off` feature for the pipelining ablation (off by default);
- debug instrumentation, commented-out code and LayerZero's RocksDB reference-model fuzz example were removed.
The first two change performance only; none of the changes affects Merkle roots, proofs or durability.

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
| `crates/` | MIT OR Apache-2.0 | QMDB engine from LayerZero-Labs/qmdb, with the changes in Section 8 |
| `bench/` | Apache-2.0 (`bench/LICENSE`) | benchmark driver from LayerZero-Labs/qmdb, extended with the baseline adapters |
| `nomt/`, `reth/`, `lvmt/` | their own licenses | Section 8 |

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
