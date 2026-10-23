#!/bin/bash
# run_nomt_benchmark.sh
set -euo pipefail

# 0. Core Linux Level Monitor Library
source /users/sijiec/os_monitor/bench_core.sh
command -v cargo >/dev/null 2>&1 || { echo "❌ cargo not found in PATH."; exit 1; }

# 1. 自动检测 NUMA / UMA
if [ -d /sys/devices/system/node/node1 ] && command -v numactl >/dev/null 2>&1; then
    IS_NUMA=true
    FIXED_CORE=40          # NUMA0 物理核心数
    HT_OFFSET=80           # UMA 模式下 HT 偏移（NUMA 模式不使用）
    DISK_DEVICE="sdb1"
    MODE_DIR="NUMA"
    NUMA_NODE=0
    echo "🔍 Detected: NUMA machine (FIXED_CORE=${FIXED_CORE}, NUMA_NODE=${NUMA_NODE})"
else
    IS_NUMA=false
    FIXED_CORE=32          # UMA 物理核心数
    HT_OFFSET=16
    DISK_DEVICE="md0p2"
    MODE_DIR="UMA"
    NUMA_NODE=0
    echo "🔍 Detected: UMA machine (FIXED_CORE=${FIXED_CORE}, HT_OFFSET=${HT_OFFSET})"
fi

# 2. 全局配置
USE_ETH=true
START_FROM_EXISTING=false

DB_NAME="NOMT"
ADAPTER_FILE="./bench/src/speed/nomt_adapter.rs"
BINARY_PATH="./target/release/speed"
ETH_TRACE_DIR="/mnt/ssd/real_eth_workload/kv_decompressed/"

# NOMT 参数扫描基准值
DEF_BUCKETS=39073369
DEF_PC_SIZE=4096
DEF_LC_SIZE=0
DEF_COMMIT=8
DEF_IO=32

SWEEP_BUCKETS=()
SWEEP_PC_SIZE=(2 32 256 1024 2048)
SWEEP_LC_SIZE=(128 1024 4096 8192 16384 32768 57344)
SWEEP_COMMIT=()
SWEEP_IO=()

BENCH_ARGS_BASE=(
  --db-dir /mnt/ssd/
  --entry-count 300000000
  --ops-per-block 100000
  --hover-recreate-block 50
  --hover-write-block 50
  --hover-interval 1000
  --tps-blocks 0
)

if [ "$USE_ETH" = true ]; then
    BENCH_ARGS_BASE+=(--eth-trace-dir "$ETH_TRACE_DIR")
fi

if [ "$START_FROM_EXISTING" = true ]; then
    BENCH_ARGS_BASE+=(--start-from-existing)
fi

CPU_LIST_NODE0_ALL=(
0 4 8 12 16 20 24 28 32 36
40 44 48 52 56 60 64 68 72 76
80 84 88 92 96 100 104 108 112 116
120 124 128 132 136 140 144 148 152 156
)

# 3. 辅助函数 & 清理
gen_cpu_list() {
    local n=$1
    local max=${#CPU_LIST_NODE0_ALL[@]}
    if (( n > max )); then n=$max; fi
    local slice=("${CPU_LIST_NODE0_ALL[@]:0:$n}")
    local IFS=,
    echo "${slice[*]}"
}

analyze_logs() {
    local log_file="$1"
    local time_stats="$2"
    local gc_stats="$3"
    grep "GC_SNAPSHOT" "$log_file" > "$gc_stats" || true
    grep "Timestamp" "$log_file" | while read -r line; do
        if [[ "$line" =~ ([0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}[^\ ]*) ]]; then
            ts="${BASH_REMATCH[1]}"
            formatted_date=$(TZ=America/New_York date -d "$ts" '+%F %T %Z' 2>/dev/null) || formatted_date="$ts"
            echo "$line => $formatted_date" >> "$time_stats"
        fi
    done || true
}

DB_PID=""
on_exit() {
    local rc=$?
    if declare -f stop_monitors >/dev/null 2>&1; then stop_monitors || true; fi
    if [[ -n "$DB_PID" ]] && kill -0 "$DB_PID" >/dev/null 2>&1; then
        kill -9 "$DB_PID" >/dev/null 2>&1 || true
    fi
    exit $rc
}
trap on_exit EXIT

# 4. 核心执行函数
run_experiment() {
    local buckets=$1
    local pc_size=$2
    local lc_size=$3
    local commit=$4
    local io=$5
    local exp_tag=$6

    echo "============================================================"
    echo "🧪 RUN $DB_NAME: $exp_tag (MODE: $MODE_DIR | ETH: $USE_ETH | START_FROM_EXISTING: $START_FROM_EXISTING)"
    echo "============================================================"

    sed -i "s/opts.hashtable_buckets([0-9_]\+)/opts.hashtable_buckets(${buckets})/" "$ADAPTER_FILE"
    sed -i "s/opts.page_cache_size([0-9_]\+)/opts.page_cache_size(${pc_size})/" "$ADAPTER_FILE"
    sed -i "s/opts.leaf_cache_size([0-9_]\+)/opts.leaf_cache_size(${lc_size})/" "$ADAPTER_FILE"
    sed -i "s/opts.commit_concurrency([0-9_]\+)/opts.commit_concurrency(${commit})/" "$ADAPTER_FILE"
    sed -i "s/opts.io_workers([0-9_]\+)/opts.io_workers(${io})/" "$ADAPTER_FILE"

    cargo build --bin speed --release --features 'use_nomt' || exit 1

    EXP_DIR="results/${DB_NAME}/${MODE_DIR}/${exp_tag}"
    mkdir -p "$EXP_DIR"

    MAIN_LOG="${EXP_DIR}/benchmark_output.log"
    TIMESTATS="${EXP_DIR}/timestats.log"
    GCSTATS="${EXP_DIR}/gcstats.log"
    : > "$TIMESTATS"; : > "$GCSTATS"

    if declare -f drop_caches >/dev/null 2>&1; then drop_caches; fi

    if [ "$IS_NUMA" = true ]; then
        CPU_LIST=$(gen_cpu_list "$FIXED_CORE")
        CMD=(numactl --physcpubind="${CPU_LIST}" --membind="${NUMA_NODE}" "$BINARY_PATH")
    else
        CORE_RANGE=$(calc_cpu_range "$FIXED_CORE" "$HT_OFFSET")
        CMD=(taskset -c "${CORE_RANGE}" "$BINARY_PATH")
    fi

    for a in "${BENCH_ARGS_BASE[@]}"; do CMD+=("$a"); done

    echo "🚀 Running Command: ${CMD[*]}"
    "${CMD[@]}" > "$MAIN_LOG" 2>&1 &
    DB_PID=$!

    if declare -f start_monitors >/dev/null 2>&1; then start_monitors "$DB_PID" "$DISK_DEVICE" "$EXP_DIR"; fi
    local exit_code=0
    wait "$DB_PID" || exit_code=$?

    if declare -f stop_monitors >/dev/null 2>&1; then stop_monitors; fi
    analyze_logs "$MAIN_LOG" "$TIMESTATS" "$GCSTATS"

    echo "✅ Finished: $EXP_DIR (Exit Code: $exit_code)"
    DB_PID=""
    sleep 5
}

# 5. 主执行逻辑
[[ -f "$ADAPTER_FILE" ]] || { echo "❌ Adapter file not found: $ADAPTER_FILE"; exit 1; }

run_experiment $DEF_BUCKETS $DEF_PC_SIZE $DEF_LC_SIZE $DEF_COMMIT $DEF_IO "BASELINE_Defaults"

if [[ ${#SWEEP_BUCKETS[@]} -gt 0 ]]; then
    for val in "${SWEEP_BUCKETS[@]}"; do run_experiment $val $DEF_PC_SIZE $DEF_LC_SIZE $DEF_COMMIT $DEF_IO "Bucket_${val}"; done
fi
if [[ ${#SWEEP_PC_SIZE[@]} -gt 0 ]]; then
    for val in "${SWEEP_PC_SIZE[@]}"; do run_experiment $DEF_BUCKETS $val $DEF_LC_SIZE $DEF_COMMIT $DEF_IO "PageCache_${val}"; done
fi
if [[ ${#SWEEP_LC_SIZE[@]} -gt 0 ]]; then
    for val in "${SWEEP_LC_SIZE[@]}"; do run_experiment $DEF_BUCKETS $DEF_PC_SIZE $val $DEF_COMMIT $DEF_IO "LeafCache_${val}"; done
fi
if [[ ${#SWEEP_COMMIT[@]} -gt 0 ]]; then
    for val in "${SWEEP_COMMIT[@]}"; do run_experiment $DEF_BUCKETS $DEF_PC_SIZE $DEF_LC_SIZE $val $DEF_IO "Commit_${val}"; done
fi
if [[ ${#SWEEP_IO[@]} -gt 0 ]]; then
    for val in "${SWEEP_IO[@]}"; do run_experiment $DEF_BUCKETS $DEF_PC_SIZE $DEF_LC_SIZE $DEF_COMMIT $val "IOWorker_${val}"; done
fi

# 恢复默认设置
sed -i "s/opts.hashtable_buckets([0-9_]\+)/opts.hashtable_buckets(${DEF_BUCKETS})/" "$ADAPTER_FILE"
sed -i "s/opts.page_cache_size([0-9_]\+)/opts.page_cache_size(${DEF_PC_SIZE})/" "$ADAPTER_FILE"
sed -i "s/opts.leaf_cache_size([0-9_]\+)/opts.leaf_cache_size(${DEF_LC_SIZE})/" "$ADAPTER_FILE"
sed -i "s/opts.commit_concurrency([0-9_]\+)/opts.commit_concurrency(${DEF_COMMIT})/" "$ADAPTER_FILE"
sed -i "s/opts.io_workers([0-9_]\+)/opts.io_workers(${DEF_IO})/" "$ADAPTER_FILE"

trap - EXIT
echo "🎉 ALL $DB_NAME BENCHMARKS COMPLETED."
