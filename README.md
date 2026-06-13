# QMDB-Benchmark

A unified benchmark harness for authenticated key-value stores / blockchain state
databases. It drives several backends through **one shared workload generator and
one `speed` binary**, so results are directly comparable across systems.

Currently supported backends: **QMDB** (default), **NOMT**, **LVMT**, **Reth (MDBX trie)**,
and **MDBX** (plain). The harness is designed to be extended — adding a new system
means writing one adapter and registering it (see [Adding a new system](#5-adding-a-new-system)).

---

## 1. How it works (architecture)

```
                ┌─────────────────────────────────────────────┐
 workload  ───► │ bench/src/bin/speed.rs   (the driver)        │
 generator      │   • populate N entries, block by block      │
 (test_gen.rs)  │   • run "hover" CRUD benchmark               │
                │   • (optional) replay a real Ethereum trace │
                └───────────────────┬─────────────────────────┘
                                     │  calls a uniform interface
                                     ▼
                ┌─────────────────────────────────────────────┐
                │ bench/src/speed/db_backend.rs                │
                │   cfg-gated dispatch: picks ONE backend at   │
                │   COMPILE time based on the --features flag  │
                └───────────────────┬─────────────────────────┘
          ┌──────────────┬──────────┼───────────┬──────────────┐
          ▼              ▼          ▼           ▼              ▼
       qmdb.rs      nomt_adapter  lvmt_adapter reth_adapter   mdbx.rs
      (default)     (use_nomt)    (use_lvmt)   (use_reth)    (use_mdbx)
```

- The **backend is selected at compile time** via a Cargo feature. No feature = QMDB.
- Every backend implements the same small interface (`init / create_kv / update_kv /
  delete_kv / read_kv / flush`), so `speed.rs` never knows which DB it is talking to.
- Each adapter writes its DB into its own subdirectory under `--db-dir`
  (`ADS-0/`, `NOMT/`, `RETH/`, `LVMT/`, `MDBX/`), so the same `--db-dir` is safe for all.

### Repository layout

```
crates/                      QMDB engine workspace (qmdb, qmdb-common, hpfile) = the default backend
bench/
  src/bin/speed.rs           benchmark driver (populate + hover + ETH replay)
  src/cli.rs                 all command-line flags
  src/speed/
    db_backend.rs            ★ compile-time dispatch to the selected backend
    mod.rs                   module wiring (cfg-gated `pub mod <sys>_adapter`)
    qmdb.rs / mdbx.rs        backend adapters …
    nomt_adapter.rs / reth_adapter.rs / lvmt_adapter.rs
    test_gen.rs              workload key/value generator (deterministic)
    hover_gen.rs             the CRUD "hover" benchmark generator
    results.rs / result_log.rs   metrics + JSON output
  Cargo.toml                 ★ backend features + path-deps to backend crates
  Cargo.toml.lvmt            Cargo.toml variant for qmdb/nomt/lvmt/mdbx builds
  Cargo.toml.reth            Cargo.toml variant for reth builds
  switch_backend.sh          swaps Cargo.toml between the two variants (see §6)
run_<sys>_benchmark.sh       per-system reference run scripts (qmdb/nomt/lvmt/reth)
run_bench.sh                 generic sweep engine (+ systems/<name>.sh plugins)
monitor/                     hwmonitor.sh + make_all_plots.py (cgroup mem/CPU/IO sampling)
```

> **Important:** the backend *libraries* (nomt, reth, lvmt, …) are **NOT** vendored in
> this repo. `bench/Cargo.toml` references them as **path dependencies to sibling
> directories** (`../../nomt`, `../../reth`, `../../lvmt`, `../../eth-replay-kv`).
> Lay the tree out like this so the paths resolve with no edits:
> ```
> qmdb_project/
>   QMDB-Benchmark/   ← this repo
>   nomt/  reth/  lvmt/  eth-replay-kv/
> ```

---

## 2. Prerequisites

- Rust (pinned by `rust-toolchain.toml`) and a C/C++ toolchain (RocksDB builds C++).
- A fast SSD with enough free space for the target DB size.
- `ulimit -n 65535` (or higher) — QMDB's io_uring prefetcher opens many files.
- Sibling backend crates present (see the note above) for any non-QMDB backend.

---

## 3. Building a backend

Because Reth and LVMT pull conflicting native libs (`zstd-sys` ^1 vs ^2, both
`links="zstd"`), the two cannot be compiled into the same `Cargo.toml`. `switch_backend.sh`
physically swaps `Cargo.toml` between two variants. Pick the variant, then build with
the feature flag:

| Backend | Build commands (run from `bench/`) |
|---|---|
| **QMDB** (default) | `./switch_backend.sh lvmt && cargo build --release --bin speed` |
| **NOMT** | `./switch_backend.sh lvmt && cargo build --release --bin speed --features use_nomt` |
| **MDBX** | `./switch_backend.sh lvmt && cargo build --release --bin speed --features use_mdbx` |
| **LVMT** | `./switch_backend.sh lvmt && CXXFLAGS="-fpermissive -std=c++17 -include cstdint" cargo build --release --bin speed --features use_lvmt` |
| **Reth** | `./switch_backend.sh reth && cargo build --release --bin speed --features use_reth` |

Notes:
- The `CXXFLAGS=...` for LVMT is **required** (RocksDB + GCC ≥13), otherwise the
  `cfx-kvdb-rocksdb` build script fails.
- `./switch_backend.sh status` shows the active variant.
- Building `use_reth` after a `lvmt` build (or vice-versa) recompiles from the swapped
  `Cargo.toml`; that is expected.

The build always produces `target/release/speed`. Each backend overwrites it, so if you
want several backends side by side, copy the binary out after each build
(e.g. `cp target/release/speed ~/bin/speed_nomt`).

---

## 4. Running a benchmark

```bash
# from the repo root, after building the backend you want:
[ -f randsrc.dat ] || head -c 10M </dev/urandom > randsrc.dat   # randomness source

./bench/target/release/speed \
  --db-dir        /mnt/ssd/bench \      # DB goes in /mnt/ssd/bench/<BACKEND>/
  --entry-count   100000000 \           # populate 100M entries (must divide ops-per-block)
  --ops-per-block 100000 \
  --hover-interval        30000 \       # run a hover benchmark every 30k blocks
  --hover-write-block     50 \          # 50 blocks of writes per hover
  --hover-recreate-block  50 \          # 50 blocks of insert+delete per (final) hover
  --hover-zipf-theta      0.0 \         # 0=uniform, 0.99=YCSB skew, >1=extreme
  --rayon-threads 32 \
  --randsrc-filename ./randsrc.dat \
  --result-log    metrics.log \         # KEY metrics appended here (empty = stdout)
  --output-filename results.json
```

### What gets measured (the "hover" workload)

1. **Population** — create `--entry-count` entries, block by block.
2. **Hover** — periodically (every `--hover-interval` blocks) the harness pauses and
   times CRUD throughput:
   - mid-population hovers measure **read + update** only;
   - the **final** post-population hover runs all four: **read / update / insert / delete**.
3. Per-phase ops/sec, OS-level physical read/write bytes (from `/proc/diskstats`, disk
   auto-derived from `--db-dir`), and a MEM/CPU/SSD summary are written to `--result-log`
   and `results.json`.

### Key parameters (full list in `bench/src/cli.rs`)

| Flag | Meaning |
|---|---|
| `--entry-count` | DB size (entries to populate). Must be a multiple of `--ops-per-block`. |
| `--db-dir` | Parent dir for the DB; also the disk used for OS R/W counters. **Must exist / be writable.** |
| `--ops-per-block` | Operations per block (default 100000). |
| `--changesets-per-task` | Changesets (transactions) grouping (default 2). |
| `--hover-interval` | Blocks between hover benchmarks. If `> entry_count/ops_per_block`, hover never runs. |
| `--hover-write-block` / `--hover-recreate-block` | Size (in blocks) of the write / insert+delete portion of a hover. |
| `--hover-zipf-theta` | Read/write key skew. `0.0` uniform; `0.99` YCSB; `>1.0` extreme (not exactly 1.0). |
| `--rayon-threads` | Worker threads (default 64). |
| `--randsrc-filename` | File of random bytes used to seed the deterministic workload. |
| `--result-log` | Append key metrics here instead of stdout. |
| `--monitor-disk` | Block device for `/proc/diskstats` (empty = auto-derive from `--db-dir`). |
| `--start-from-existing` | Skip population; run a hover against an already-populated DB. **See caveat in §6.** |

### Reference scripts & memory-capped runs

- `run_<sys>_benchmark.sh` — copy-paste starting points per backend.
- `run_bench.sh` — generic sweep engine (`--sweep entry_count=100M,300M,…`) + the unified
  `monitor/`; currently wired for QMDB via `systems/qmdb.sh`.
- To cap a run's memory (e.g. compare systems under a 32 GB budget) run the binary inside a
  cgroup, e.g. `sudo systemd-run --scope -p MemoryMax=32G -p MemorySwapMax=0 ./speed …`.

### ETH real-workload mode

Instead of the synthetic workload, replay real Ethereum block operations:

```bash
./speed --db-dir /mnt/ssd/bench \
  --eth-trace-dir /path/to/kv_decompressed \      # decoded per-block traces
  --eth-replay-start-height 15012300 \
  --eth-replay-end-height   15312300 \
  [--init-db-path bundle.bin]                      # optional: load a prebuilt init-state bundle
```

`--init-db-path` loads a pre-computed `InitDbBundle` (init state + boundary ops) so you skip
re-deriving the base state from traces each run. See `eth-replay-kv` (sibling crate) for how
bundles are produced (`gen-init-db`).

---

## 5. Adding a new system

This is the common student task. A new backend touches **one new file plus 2–3 small
edits**, all under `bench/`. Use the simplest existing adapter, **`mdbx.rs`**, as a template
(or `nomt_adapter.rs` if your system is a Merkle/trie DB).

### Step 1 — write the adapter: `bench/src/speed/<sys>_adapter.rs`

Implement these functions (mirror an existing adapter for exact signatures):

```rust
// open/create the DB at `db_dir`; if start_from_existing, do NOT wipe it.
pub fn init(db_dir: &str, start_from_existing: bool) { … }

// apply one block of operations. `task_list` is the block; iterate it like:
fn apply(height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    for lock in &task_list {
        if let Some(task) = lock.read().as_ref() {
            for cset in task.get_change_sets() {
                cset.for_each_op(|op_type, _shard, key_hash: &[u8;32], key, value, _old, _rec| {
                    match op_type {
                        OP_CREATE | OP_WRITE => { /* db.put(key_hash/key, value) */ }
                        OP_DELETE           => { /* db.delete(...) */ }
                        OP_READ             => { /* let _ = db.get(...); */ }
                        _ => {}
                    }
                });
            }
        }
    }
    // commit / flush this block, advancing the version to `height`
}

pub fn create_kv(height: i64, task_list: …) { apply(height, task_list) }
pub fn update_kv(height: i64, task_list: …) { apply(height, task_list) }
pub fn delete_kv(height: i64, task_list: …) { apply(height, task_list) }
pub fn read_kv (…)                          { /* read-only path */ }
pub fn flush() { /* fsync / wait for compaction if needed */ }
```

Keys arrive as a 32-byte `key_hash` (and the original `key` bytes); values are `&[u8]`.
Pick whichever your store wants. `OP_*` constants come from `qmdb_common::def`.

### Step 2 — register the module: `bench/src/speed/mod.rs`

```rust
#[cfg(feature = "use_<sys>")]
pub mod <sys>_adapter;
```

### Step 3 — add the dispatch arm: `bench/src/speed/db_backend.rs`

Add one `#[cfg(feature = "use_<sys>")]` block mirroring the others — a `NAME` const and a
thin wrapper for each of `init / create_kv / update_kv / delete_kv / read_kv / flush` that
calls into your adapter (it joins `db_dir`+`"<SYS>"` for the subdir). Copy the MDBX block
and rename.

### Step 4 — declare the feature & deps: `bench/Cargo.toml`

```toml
[features]
use_<sys> = ["dep:<sys>-crate"]          # or = [] if it needs no extra deps

[dependencies]
<sys>-crate = { path = "../../<sys>", optional = true }   # sibling dir (see §1 note)
```
If your system's native deps clash with Reth's or LVMT's (zstd / rocksdb `links=`),
add it to `Cargo.toml.lvmt` (and/or `.reth`) and extend `switch_backend.sh` accordingly.

### Step 5 — build & run

```bash
cd bench && ./switch_backend.sh lvmt           # or a new mode if you added one
cargo build --release --bin speed --features use_<sys>
./target/release/speed --db-dir /mnt/ssd/bench --entry-count 1000000 --ops-per-block 100000 …
```
Then add a `run_<sys>_benchmark.sh` (copy an existing one) so others can reproduce your runs.

### Checklist

- [ ] `bench/src/speed/<sys>_adapter.rs` (new)
- [ ] `bench/src/speed/mod.rs`        (+1 cfg line)
- [ ] `bench/src/speed/db_backend.rs` (+1 cfg block: NAME + 6 wrappers)
- [ ] `bench/Cargo.toml`              (+ feature, + path dep)
- [ ] a `run_<sys>_benchmark.sh`      (optional but expected)

---

## 6. Gotchas

- **Reth vs LVMT can't coexist** in one `Cargo.toml` (zstd-sys conflict) → always
  `switch_backend.sh {lvmt,reth}` before building.
- **LVMT build** needs `CXXFLAGS="-fpermissive -std=c++17 -include cstdint"`.
- **`Too many open files`** → raise `ulimit -n` (65535+). Under `systemd-run`, set it via an
  inner `ulimit -n 1048576` in the launched shell — `-p LimitNOFILE=…` is rejected on scopes.
- **`--start-from-existing`** currently works for **NOMT and Reth** (their `init` reopens the
  DB when the flag is set). **QMDB** does not support it yet, and **LVMT / MDBX** currently
  wipe their DB on `init` regardless of the flag — don't rely on it for those until fixed.
- **`--entry-count` must divide `--ops-per-block`**, or the run panics early.
