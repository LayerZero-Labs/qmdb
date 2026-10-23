#!/bin/bash
# bench_core.sh
# 包含通用函数：清理、监控控制、缓存清理、CPU掩码计算

# 监控脚本路径 (假设都在同级目录或指定目录)
MONITOR_DIR=$(dirname "${BASH_SOURCE[0]}")
HW_MONITOR="${MONITOR_DIR}/hwmonitor.sh"

# 全局变量，用于 cleanup
G_HW_MON_PID=""
G_TARGET_PID=""
G_RUN_FLAG=""

# 1. 信号捕获与清理
core_cleanup() {
    echo ""
    echo "🧹 [Core] Cleaning up..."

    # 停止监控脚本
    [[ -n "$G_RUN_FLAG" ]] && rm -f "$G_RUN_FLAG"

    # 杀掉监控进程
    if [[ -n "$G_HW_MON_PID" ]]; then kill "$G_HW_MON_PID" 2>/dev/null; fi

    # 如果目标程序还在运行，也杀掉 (可选，防止 benchmark 没跑完僵死)
    if [[ -n "$G_TARGET_PID" ]]; then
        kill "$G_TARGET_PID" 2>/dev/null || true
    fi

    echo "✅ [Core] Cleanup finished."
}

# 注册 Trap
trap core_cleanup EXIT INT TERM

# 2. 缓存清理 (需要 sudo 权限)
drop_caches() {
    echo "Start Dropping caches..."
    sudo sync
    # 既清 PageCache 也清 dentries/inodes
    sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches'
    echo "Caches dropped."
}

# 3. 计算 CPU 绑核掩码 (Taskset)
# 输入: LIMIT_CORES, HT_OFFSET
# 输出: 打印 core range 字符串 (例如: 0-9,80-89)
calc_cpu_range() {
    local cores=$1
    local offset=$2

    # No-SMT hosts (e.g., AWS Graviton metal) signal "no HT siblings"
    # by passing offset >= cores. Emit a single contiguous 0-(N-1) range.
    if [ "$offset" -ge "$cores" ]; then
        if [ "$cores" -le 1 ]; then
            echo "0"
        else
            echo "0-$(( cores - 1 ))"
        fi
        return
    fi

    # SMT host (Intel/AMD UMA): split the LIMIT_CORES across two physical
    # blocks: 0..(N/2-1) and offset..(offset+N/2-1).
    local num_physical=$(( cores / 2 ))
    if [ "$num_physical" -lt 1 ]; then
        echo "0"
    else
        local end_p1=$(( num_physical - 1 ))
        local start_p2=$offset
        local end_p2=$(( offset + num_physical - 1 ))
        echo "0-${end_p1},${start_p2}-${end_p2}"
    fi
}

# 4. 启动监控 (统一: CPU + MEM(cgroup) + SSD -> hardware_usage.csv)
# 参数1: 目标进程 PID
# 参数2: 磁盘设备名 (e.g. md0)
# 参数3: 日志输出目录
start_monitors() {
    local target_pid=$1
    local disk_dev=$2
    local log_dir=$3

    G_TARGET_PID=$target_pid
    G_RUN_FLAG="${log_dir}/running.flag"
    touch "$G_RUN_FLAG"

    echo "🔍 Starting unified hardware monitor for PID: $target_pid, Device: $disk_dev (5s interval)"

    # 单一监控: 进程/系统 CPU + cgroup 内存 + SSD, 每 5s 一行 -> hardware_usage.csv
    bash "$HW_MONITOR" "$target_pid" "$G_RUN_FLAG" "$disk_dev" > "${log_dir}/hardware_usage.csv" 2>"${log_dir}/hwmonitor.err" &
    G_HW_MON_PID=$!
}

# 5. 停止监控
stop_monitors() {
    rm -f "$G_RUN_FLAG"
    wait "$G_HW_MON_PID" 2>/dev/null || true
    G_HW_MON_PID=""
    G_TARGET_PID="" # 任务结束，不再追踪
    echo "🛑 Monitors stopped."
}
