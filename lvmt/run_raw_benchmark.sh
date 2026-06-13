#!/bin/bash
# run_raw_benchmark.sh
# 使用 LVMT benchmark 框架测试纯 RocksDB (RAW) 性能，作为 LVMT 的基线对比。
# 参数与 run_sweep_lvmt.sh 保持一致，只替换 -a raw 并去掉 --shards。

set -euo pipefail

source /users/sijiec/os_monitor/bench_core.sh
command -v cargo >/dev/null 2>&1 || { echo "❌ cargo not found in PATH."; exit 1; }

# ==========================================
# 1. 自动检测 NUMA / UMA
# ==========================================
if [ -d /sys/devices/system/node/node1 ] && command -v numactl >/dev/null 2>&1; then
    IS_NUMA=true
    FIXED_CORE=40
    DISK_DEVICE="sdb1"
    MODE_DIR="NUMA"
    NUMA_NODE=0
    echo "🔍 Detected: NUMA machine"
else
    IS_NUMA=false
    FIXED_CORE=32
    HT_OFFSET=16
    DISK_DEVICE="nvme0n1"
    MODE_DIR="UMA"
    NUMA_NODE=0
    echo "🔍 Detected: UMA machine"
fi

DB_NAME="RAW_Sweep"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

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

run_benchmark() {
    local db_size=$1
    echo "============================================================"
    echo "🧪 RUN $DB_NAME - Size: ${db_size} (MODE: $MODE_DIR)"
    echo "============================================================"

    EXP_DIR="results/${DB_NAME}/${MODE_DIR}/run_${db_size}"
    mkdir -p "$EXP_DIR"
    MAIN_LOG="${EXP_DIR}/benchmark_output.log"
    : > "$MAIN_LOG"

    if declare -f drop_caches >/dev/null 2>&1; then drop_caches; fi

    local TARGET_ARGS=(
        -a raw
        -k "${db_size}"
        -b rocksdb
        --epoch-size 10000
        --cache-size 2048
        --no-stat
        --max-epoch 300
        --db /mnt/ssd/RAW
    )

    local PREFIX_CMD=()
    if [ "$IS_NUMA" = true ]; then
        CPU_LIST=$(gen_cpu_list "$FIXED_CORE")
        PREFIX_CMD=(numactl --physcpubind="${CPU_LIST}" --membind="${NUMA_NODE}")
    else
        CORE_RANGE=$(calc_cpu_range "$FIXED_CORE" "$HT_OFFSET")
        PREFIX_CMD=(taskset -c "${CORE_RANGE}")
    fi

    echo "🚀 Running: ${PREFIX_CMD[*]} cargo run --release -- ${TARGET_ARGS[*]}"
    "${PREFIX_CMD[@]}" cargo run --release -- "${TARGET_ARGS[@]}" > "$MAIN_LOG" 2>&1 &
    DB_PID=$!

    if declare -f start_monitors >/dev/null 2>&1; then
        start_monitors "$DB_PID" "$DISK_DEVICE" "$EXP_DIR"
    fi

    local exit_code=0
    wait "$DB_PID" || exit_code=$?

    if declare -f stop_monitors >/dev/null 2>&1; then stop_monitors; fi

    echo "✅ Finished: $EXP_DIR (Exit Code: $exit_code)"
    echo "📄 Log: $MAIN_LOG"
    DB_PID=""
    sleep 10
}

echo "🔨 Pre-compiling..."
cargo build --release || { echo "❌ Compilation failed."; exit 1; }

DB_SIZES=("100m")

for size in "${DB_SIZES[@]}"; do
    run_benchmark "$size"
done

trap - EXIT
echo "🎉 ALL RAW BENCHMARKS COMPLETED."
