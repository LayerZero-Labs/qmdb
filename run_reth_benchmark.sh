#!/bin/bash
# run_reth_benchmark.sh
set -euo pipefail

# ==========================================
# 0. Core Linux Level Monitor Library
# ==========================================
source /users/sijiec/os_monitor/bench_core.sh
command -v cargo >/dev/null 2>&1 || { echo "❌ cargo not found in PATH."; exit 1; }
command -v systemd-run >/dev/null 2>&1 || { echo "❌ systemd-run not found. Required for cgroup memory limits."; exit 1; }

# ==========================================
# 1. 自动检测 NUMA / UMA
# ==========================================
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

# ==========================================
# 2. 全局配置
# ==========================================
USE_ETH=false
START_FROM_EXISTING=false

DB_NAME="Reth"
BINARY_PATH="./target/release/speed"
ETH_TRACE_DIR="/mnt/ssd/real_eth_workload/kv_decompressed/"

SNAPSHOT_DIR="/mnt/ssd/RETH_400M_SNAPSHOT"
TARGET_DB_DIR="/mnt/ssd/RETH"

# 内存限制扫描列表
# - 留空 ()         → 不限制内存，结果放到 BASELINE_Defaults/
# - 填写如 (32G 64G) → 依次扫描，结果放到对应的 Memory_32G/ Memory_64G/ 等
MEMORY_LIMITS=(32G 16G 12G 8G 4G)

BENCH_ARGS_BASE=(
  --db-dir /mnt/ssd/
  --entry-count 400000000
  --ops-per-block 100000
  --hover-recreate-block 40
  --hover-write-block 40
  --hover-interval 1000000
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

# ==========================================
# 3. 辅助函数 & 清理
# ==========================================
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

# ==========================================
# 4. 核心执行函数
# ==========================================
# 参数 1: exp_tag  (例如 "BASELINE_Defaults" 或 "Memory_64G")
# 参数 2: mem_limit (可选，例如 "64G"；留空表示不限制)
run_experiment() {
    local exp_tag=$1
    local mem_limit=${2:-""}

    echo "============================================================"
    echo "🧪 RUN $DB_NAME: $exp_tag (MODE: $MODE_DIR | ETH: $USE_ETH | MEM_LIMIT: ${mem_limit:-NONE} | START_FROM_EXISTING: $START_FROM_EXISTING)"
    echo "============================================================"

    # --- 4.1 准备数据 ---
    if [ "$START_FROM_EXISTING" = true ]; then
        echo "🧹 Resetting Database Data (START_FROM_EXISTING=true)..."
        echo "   -> Deleting old $TARGET_DB_DIR"
        rm -rf "$TARGET_DB_DIR"
        echo "   -> Copying from $SNAPSHOT_DIR to $TARGET_DB_DIR"
        cp -a "$SNAPSHOT_DIR" "$TARGET_DB_DIR"
        sync
    else
        echo "⏭️  Starting from scratch (START_FROM_EXISTING=false)..."
        echo "   -> Cleaning up old $TARGET_DB_DIR"
        rm -rf "$TARGET_DB_DIR"
    fi

    # --- 4.2 编译 ---
    # Cargo.toml has two templates (LVMT-mode default, Reth-mode for this script)
    # because cfx-kvdb-rocksdb (LVMT) and reth-primitives both declare
    # links="zstd". switch_backend.sh swaps Cargo.toml between the two.
    echo "🔨 Compiling..."
    bash bench/switch_backend.sh reth
    cargo build --bin speed --release --features 'use_reth' || exit 1

    # --- 4.3 准备目录 ---
    EXP_DIR="results/${DB_NAME}/${MODE_DIR}/${exp_tag}"
    mkdir -p "$EXP_DIR"

    MAIN_LOG="${EXP_DIR}/benchmark_output.log"
    TIMESTATS="${EXP_DIR}/timestats.log"
    GCSTATS="${EXP_DIR}/gcstats.log"
    : > "$TIMESTATS"; : > "$GCSTATS"

    if declare -f drop_caches >/dev/null 2>&1; then drop_caches; fi

    # --- 4.4 构建执行命令 ---
    local BASE_CMD=()

    if [ "$IS_NUMA" = true ]; then
        CPU_LIST=$(gen_cpu_list "$FIXED_CORE")
        BASE_CMD=(numactl --physcpubind="${CPU_LIST}" --membind="${NUMA_NODE}" "$BINARY_PATH")
    else
        CORE_RANGE=$(calc_cpu_range "$FIXED_CORE" "$HT_OFFSET")
        BASE_CMD=(taskset -c "${CORE_RANGE}" "$BINARY_PATH")
    fi

    for a in "${BENCH_ARGS_BASE[@]}"; do BASE_CMD+=("$a"); done

    # 如果有内存限制，套上 systemd-run
    local FINAL_CMD=()
    if [[ -n "$mem_limit" ]]; then
        FINAL_CMD=(systemd-run --scope -p MemoryMax="${mem_limit}" -p MemoryHigh="${mem_limit}" "${BASE_CMD[@]}")
    else
        FINAL_CMD=("${BASE_CMD[@]}")
    fi

    echo "🚀 Running Command: ${FINAL_CMD[*]}"
    "${FINAL_CMD[@]}" > "$MAIN_LOG" 2>&1 &
    DB_PID=$!

    if declare -f start_monitors >/dev/null 2>&1; then start_monitors "$DB_PID" "$DISK_DEVICE" "$EXP_DIR"; fi
    local exit_code=0
    wait "$DB_PID" || exit_code=$?

    if declare -f stop_monitors >/dev/null 2>&1; then stop_monitors; fi
    analyze_logs "$MAIN_LOG" "$TIMESTATS" "$GCSTATS"

    echo "✅ Finished: $EXP_DIR (Exit Code: $exit_code)"
    DB_PID=""
    sleep 10
}

# ==========================================
# 5. 主执行逻辑
# ==========================================
echo "🚀 Starting $DB_NAME Benchmark Series..."

if [[ ${#MEMORY_LIMITS[@]} -eq 0 ]]; then
    # 无内存限制 → Baseline
    run_experiment "BASELINE_Defaults" ""
else
    # 扫描内存限制列表
    for MEM in "${MEMORY_LIMITS[@]}"; do
        run_experiment "Memory_${MEM}" "$MEM"
    done
fi

trap - EXIT
echo "🎉 ALL $DB_NAME BENCHMARKS COMPLETED."