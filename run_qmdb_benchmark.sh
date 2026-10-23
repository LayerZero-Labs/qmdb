#!/bin/bash
# run_qmdb_benchmark.sh
set -euo pipefail

# 0. Core Linux Level Monitor Library
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/monitor/bench_core.sh"
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
    # Auto-detect physical/logical cores so the script works on both
    # x86_64 hyperthreading hosts (32+16 layout) and ARM Graviton metal
    # (64 physical, no SMT) without manual edits.
    PHYS_CORES=$(lscpu -p=core 2>/dev/null | grep -v '^#' | sort -u | wc -l | tr -d ' ')
    [[ -z "$PHYS_CORES" || "$PHYS_CORES" -eq 0 ]] && PHYS_CORES=$(nproc)
    LOGICAL_CORES=$(nproc)
    FIXED_CORE=$PHYS_CORES
    if [ "$LOGICAL_CORES" -gt "$PHYS_CORES" ]; then
        # SMT enabled (Intel/AMD): HT siblings start at PHYS_CORES.
        HT_OFFSET=$PHYS_CORES
    else
        # No SMT (e.g., AWS Graviton): set offset == LOGICAL_CORES so
        # calc_cpu_range in bench_core.sh emits a single 0-(N-1) range.
        HT_OFFSET=$LOGICAL_CORES
    fi
    DISK_DEVICE="md0"
    MODE_DIR="UMA"
    NUMA_NODE=0
    echo "🔍 Detected: UMA machine (FIXED_CORE=${FIXED_CORE}, HT_OFFSET=${HT_OFFSET}, logical=${LOGICAL_CORES})"
fi

# 2. 全局配置
USE_ETH=false
START_FROM_EXISTING=false

ulimit -n 65535
DB_NAME="QMDB"
TARGET_FILE="./crates/qmdb-common/src/def.rs"
BINARY_PATH="./target/release/speed"
ETH_TRACE_DIR="/mnt/ssd/real_eth_workload/kv_decompressed/"

# 内存限制扫描列表
# - 留空 ()         → 不限制内存，结果放到 BASELINE_Defaults/
# - 填写如 (32G 64G) → 依次扫描，结果放到对应的 Memory_32G/ Memory_64G/ 等
MEMORY_LIMITS=()

# 扫描参数（每组 RATIO / CORE / SHARD 一一对应）
RATIO_VALUES=(7)
CORE_COUNTS=($FIXED_CORE)
SHARD_COUNTS=(16)

# Zipf exponent 扫描列表
# - 留空 ()              → 不传该参数，使用默认值（均匀随机）
# - 填写如 (0 0.99 1.5)  → 依次扫描，结果目录名含 Zipf_0 / Zipf_0.99 / Zipf_1.5
SWEEP_ZIPF=(0)
ENTRY_COUNTS=(1500000000)

BENCH_ARGS_BASE=(
  --db-dir /mnt/ssd/QMDB_script
  --ops-per-block 100000
  --hover-recreate-block 50
  --hover-write-block 50
  --hover-interval 30000
  --tps-blocks 0
  --rayon-threads 128
  --randsrc-filename /mnt/ssd/randsrc.dat
)

if [ "$USE_ETH" = true ]; then
    BENCH_ARGS_BASE+=(
        --eth-trace-dir "$ETH_TRACE_DIR"
        --eth-end-height 20407400
        --eth-replay-end-height 15500000
    )
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
    done
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
# 参数 1: mem_tag   (例如 "BASELINE_Defaults" 或 "Memory_64G")
# 参数 2: mem_limit (可选，例如 "64G"；留空表示不限制)
# 参数 3: zipf_val  (可选，例如 "0.99"；留空表示不传参数，使用默认均匀随机)
run_experiment_set() {
    local mem_tag=$1
    local mem_limit=${2:-""}
    local zipf_val=${3:-""}

    [[ -f "$TARGET_FILE" ]] || { echo "❌ Source file not found: $TARGET_FILE"; exit 1; }

    for i in "${!RATIO_VALUES[@]}"; do
        LIMIT_CORES=${CORE_COUNTS[$i]}
        CURRENT_RATIO=${RATIO_VALUES[$i]}
        CURRENT_SHARD=${SHARD_COUNTS[$i]}
        local zipf_tag=""
        if [[ -n "$zipf_val" ]]; then
            zipf_tag="_Zipf_${zipf_val}"
        fi
        local CURRENT_EC=${ENTRY_COUNTS[$i]}
        local ec_label="$((CURRENT_EC / 1000000))M"
        EXP_TAG="RATIO_${CURRENT_RATIO}_CORE_${LIMIT_CORES}_SHARD_${CURRENT_SHARD}_ENTRY_${ec_label}${zipf_tag}"

        echo "============================================================"
        echo "🧪 RUN $DB_NAME: $EXP_TAG (MODE: $MODE_DIR | MEM: ${mem_limit:-NONE} | ETH: $USE_ETH | START_FROM_EXISTING: $START_FROM_EXISTING)"
        echo "============================================================"

        sed -E -i \
            -e "s/pub const UTILIZATION_RATIO: i64 = [0-9]+;/pub const UTILIZATION_RATIO: i64 = ${CURRENT_RATIO};/g" \
            -e "s/pub const SHARD_COUNT: usize = [0-9]+;/pub const SHARD_COUNT: usize = ${CURRENT_SHARD};/g" \
            "$TARGET_FILE"

        cargo build --bin speed --release || exit 1

        EXP_DIR="results/${DB_NAME}/${MODE_DIR}/${mem_tag}/${EXP_TAG}"
        mkdir -p "$EXP_DIR"

        MAIN_LOG="${EXP_DIR}/benchmark_output.log"
        TIMESTATS="${EXP_DIR}/timestats.log"
        GCSTATS="${EXP_DIR}/gcstats.log"
        : > "$TIMESTATS"; : > "$GCSTATS"

        if declare -f drop_caches >/dev/null 2>&1; then drop_caches; fi

        local BASE_CMD=()
        if [ "$IS_NUMA" = true ]; then
            CPU_LIST=$(gen_cpu_list "$LIMIT_CORES")
            BASE_CMD=(prlimit --nofile=65535:65535 numactl --physcpubind="${CPU_LIST}" --membind="${NUMA_NODE}" "$BINARY_PATH")
        else
            CORE_RANGE=$(calc_cpu_range "$LIMIT_CORES" "$HT_OFFSET")
            BASE_CMD=(prlimit --nofile=65535:65535 taskset -c "${CORE_RANGE}" "$BINARY_PATH")
        fi

        for a in "${BENCH_ARGS_BASE[@]}"; do BASE_CMD+=("$a"); done
        BASE_CMD+=(--entry-count "$CURRENT_EC")
        if [[ -n "$zipf_val" ]]; then
            BASE_CMD+=(--hover-zipf-theta "$zipf_val")
        fi

        local FINAL_CMD=()
        if [[ -n "$mem_limit" ]]; then
            FINAL_CMD=(sudo systemd-run --scope -p MemoryAccounting=yes -p MemoryMax="${mem_limit}" -p MemoryHigh="${mem_limit}" "${BASE_CMD[@]}")
        else
            FINAL_CMD=(sudo systemd-run --scope -p MemoryAccounting=yes "${BASE_CMD[@]}")
        fi

        echo "🚀 Running Command: ${FINAL_CMD[*]}"
        "${FINAL_CMD[@]}" > "$MAIN_LOG" 2>&1 &
        DB_PID=$!

        if declare -f start_monitors >/dev/null 2>&1; then start_monitors "$DB_PID" "$DISK_DEVICE" "$EXP_DIR"; fi
        local exit_code=0
        wait "$DB_PID" || exit_code=$?

        if declare -f stop_monitors >/dev/null 2>&1; then stop_monitors; fi
        analyze_logs "$MAIN_LOG" "$TIMESTATS" "$GCSTATS" || true

        echo "✅ Finished: $EXP_DIR (Exit Code: $exit_code)"
        DB_PID=""
        sleep 5
    done
}

# 5. 主执行逻辑
run_with_mem() {
    local mem_tag=$1
    local mem_limit=$2

    if [[ ${#SWEEP_ZIPF[@]} -eq 0 ]]; then
        # 不扫描 Zipf，使用默认均匀随机
        run_experiment_set "$mem_tag" "$mem_limit" ""
    else
        for zipf_val in "${SWEEP_ZIPF[@]}"; do
            run_experiment_set "$mem_tag" "$mem_limit" "$zipf_val"
        done
    fi
}

if [[ ${#MEMORY_LIMITS[@]} -eq 0 ]]; then
    run_with_mem "BASELINE_Defaults" ""
else
    for MEM in "${MEMORY_LIMITS[@]}"; do
        run_with_mem "Memory_${MEM}" "$MEM"
    done
fi

trap - EXIT
echo "🎉 ALL $DB_NAME BENCHMARKS COMPLETED."
