#!/bin/bash
# run_lvmt_benchmark.sh
# 使用 QMDB benchmark 框架 + use_lvmt feature 测试 LVMT 性能。
# 与 run_qmdb_benchmark.sh 保持相同的结构。
set -euo pipefail

# 0. Core Linux Level Monitor Library
source /users/sijiec/os_monitor/bench_core.sh
command -v cargo >/dev/null 2>&1 || { echo "❌ cargo not found in PATH."; exit 1; }

# 16 RocksDB instances each open hundreds of fds; raise the limit.
ulimit -n 65535 2>/dev/null || echo "⚠️  Could not raise fd limit (ulimit -n 65535). Run as root or set in /etc/security/limits.conf."

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

# 切换这两个 flag
# USE_ETH=true  → 使用真实 Ethereum workload（需要 ETH_TRACE_DIR 存在）
# USE_ETH=false → 使用合成 workload（entry-count / ops-per-block 参数生效）
USE_ETH=true
ETH_TRACE_DIR="/mnt/ssd/real_eth_workload/kv_decompressed/"

DB_NAME="LVMT"
BINARY_PATH="./target/release/speed"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# 合成 workload 参数（USE_ETH=false 时生效）
# LVMT commit 是严格串行的 BLS12-381 G1 加法，单核约 7000 ops/s。
# ops_per_block=10000 → 每个 block commit 约 1.4s。
BENCH_ARGS_BASE=(
  --db-dir /mnt/ssd/
  --ops-per-block 10000
  --hover-recreate-block 150
  --hover-write-block 150
  --hover-interval 100000
  --tps-blocks 0
)

# 合成 workload: database size sweep（USE_ETH=false 时使用）
ENTRY_COUNTS=(200000000)

if [ "$USE_ETH" = true ]; then
    BENCH_ARGS_BASE+=(--eth-trace-dir "$ETH_TRACE_DIR")
fi

# 3. 辅助函数 & 清理
analyze_logs() {
    local log_file="$1"
    local time_stats="$2"
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
    local exp_tag=$1
    local entry_count=${2:-""}   # 仅合成模式使用

    echo "============================================================"
    echo "🧪 RUN $DB_NAME: $exp_tag (MODE: $MODE_DIR | ETH: $USE_ETH)"
    echo "============================================================"

    # --- 检查 pp/ 目录（adapter 硬编码为 ../lvmt/pp）---
    PP_DIR="${SCRIPT_DIR}/../lvmt/pp"
    if [[ ! -d "$PP_DIR" ]]; then
        echo "❌ LVMT pp/ directory not found: $PP_DIR"
        echo "   Please ensure lvmt/pp/ (BLS12-381 precomputed params) exists."
        exit 1
    fi

    # --- 检查 eth trace 目录（USE_ETH=true 时）---
    if [ "$USE_ETH" = true ] && [[ ! -d "$ETH_TRACE_DIR" ]]; then
        echo "❌ ETH_TRACE_DIR not found: $ETH_TRACE_DIR"
        exit 1
    fi

    # --- 编译（use_lvmt feature）---
    bash bench/switch_backend.sh lvmt
    echo "🔨 Compiling with --features use_lvmt ..."
    cargo build --bin speed --release --features 'use_lvmt' || exit 1

    # --- 准备结果目录 ---
    EXP_DIR="results/${DB_NAME}/${MODE_DIR}/${exp_tag}"
    mkdir -p "$EXP_DIR"

    MAIN_LOG="${EXP_DIR}/benchmark_output.log"
    TIMESTATS="${EXP_DIR}/timestats.log"
    : > "$TIMESTATS"

    if declare -f drop_caches >/dev/null 2>&1; then drop_caches; fi

    # --- 构建执行命令 ---
    local BASE_CMD=()
    if [ "$IS_NUMA" = true ]; then
        CPU_LIST=$(gen_cpu_list "$FIXED_CORE")
        BASE_CMD=(numactl --physcpubind="${CPU_LIST}" --membind="${NUMA_NODE}" "$BINARY_PATH")
    else
        CORE_RANGE=$(calc_cpu_range "$FIXED_CORE" "$HT_OFFSET")
        BASE_CMD=(taskset -c "${CORE_RANGE}" "$BINARY_PATH")
    fi

    for a in "${BENCH_ARGS_BASE[@]}"; do BASE_CMD+=("$a"); done

    # 合成模式额外传 entry-count
    if [ "$USE_ETH" = false ] && [[ -n "$entry_count" ]]; then
        BASE_CMD+=(--entry-count "$entry_count")
    fi

    echo "🚀 Running Command: ${BASE_CMD[*]}"
    "${BASE_CMD[@]}" > "$MAIN_LOG" 2>&1 &
    DB_PID=$!

    if declare -f start_monitors >/dev/null 2>&1; then start_monitors "$DB_PID" "$DISK_DEVICE" "$EXP_DIR"; fi
    local exit_code=0
    wait "$DB_PID" || exit_code=$?

    if declare -f stop_monitors >/dev/null 2>&1; then stop_monitors; fi
    analyze_logs "$MAIN_LOG" "$TIMESTATS" || true

    echo "✅ Finished: $EXP_DIR (Exit Code: $exit_code)"
    echo "📄 Log: $MAIN_LOG"
    DB_PID=""
    sleep 5
}

# 5. 执行实验
echo "🚀 Starting $DB_NAME Benchmark Series (ETH=$USE_ETH, MODE=$MODE_DIR)..."

# Cargo.toml has two templates (LVMT-mode default, Reth-mode separate) because
# cfx-kvdb-rocksdb (LVMT) and reth-primitives both declare links="zstd".
# switch_backend.sh ensures the LVMT-mode template is active before building.
bash bench/switch_backend.sh lvmt
# 编译放到最外层，避免每次 run_experiment 重复编译
echo "🔨 Pre-compiling with --features use_lvmt ..."
cargo build --bin speed --release --features 'use_lvmt' || { echo "❌ Compilation failed."; exit 1; }

if [ "$USE_ETH" = true ]; then
    run_experiment "eth_workload"
else
    for ec in "${ENTRY_COUNTS[@]}"; do
        label="${ec%000000}M"   # e.g. 200000000 -> 200M
        run_experiment "entry_${label}" "$ec"
    done
fi

trap - EXIT
echo "🎉 ALL $DB_NAME BENCHMARKS COMPLETED."
