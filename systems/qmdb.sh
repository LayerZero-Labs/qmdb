#!/bin/bash
# systems/qmdb.sh — QMDB plugin for run_bench.sh.
# Only system-specific knowledge; the engine calls the SYS_* vars + sys_* funcs.

SYS_NAME="QMDB"                       # results/<SYS_NAME>/... + label
SYS_DB_SUBDIR="QMDB"                  # db_dir = <base>/QMDB
SYS_FEATURES=""                       # default backend (no cargo feature)
SYS_SWITCH_BACKEND=""                 # no bench/switch_backend.sh needed
SYS_ULIMIT_NOFILE=65535               # req(2)
SYS_NEEDS_RANDSRC=1                   # QMDB workload reads randsrc.dat
SYS_DEFAULT_ENTRY_COUNT=1500000000

# Fixed bench args for QMDB (overridable via engine --ops-per-block etc.).
SYS_BASE_ARGS=(
  --ops-per-block 100000
  --hover-recreate-block 50
  --hover-write-block 50
  --hover-interval 30000
  --rayon-threads 128
)

# QMDB compile-time constants live here.
QMDB_DEF="./crates/qmdb-common/src/def.rs"

# Build ./target/release/speed for QMDB.
sys_build() {
  [[ -n "$SYS_SWITCH_BACKEND" ]] && bash bench/switch_backend.sh "$SYS_SWITCH_BACKEND"
  if [[ -n "$SYS_FEATURES" ]]; then
    cargo build --release --bin speed --features "$SYS_FEATURES"
  else
    cargo build --release --bin speed
  fi
}

# Which knobs require a source edit + rebuild (vs runtime args).
sys_is_compiletime() {
  case "$1" in
    shard|ratio|util_ratio) return 0 ;;
    *) return 1 ;;
  esac
}

# Realize one QMDB system knob by editing the source const.
sys_apply_param() {
  local knob="$1" val="$2"
  case "$knob" in
    shard)
      sed -E -i "s/pub const SHARD_COUNT: usize = [0-9]+;/pub const SHARD_COUNT: usize = ${val};/g" "$QMDB_DEF" ;;
    ratio|util_ratio)
      sed -E -i "s/pub const UTILIZATION_RATIO: i64 = [0-9]+;/pub const UTILIZATION_RATIO: i64 = ${val};/g" "$QMDB_DEF" ;;
    *) echo "qmdb: unknown system knob '$knob'"; return 1 ;;
  esac
}

# Canonical defaults applied before the first build (a non-system sweep uses these).
sys_set_defaults() {
  sed -E -i "s/pub const UTILIZATION_RATIO: i64 = [0-9]+;/pub const UTILIZATION_RATIO: i64 = 7;/g" "$QMDB_DEF"
  sed -E -i "s/pub const SHARD_COUNT: usize = [0-9]+;/pub const SHARD_COUNT: usize = 16;/g" "$QMDB_DEF"
}
