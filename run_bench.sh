#!/bin/bash
# run_bench.sh — generic benchmark ENGINE.
#
# Selects a system (plugin in systems/<name>.sh), builds the shared `speed`
# binary, runs it under the unified hardware monitor + a transient cgroup scope,
# names/archives results (timestamped, never overwritten), and auto-plots
# CPU/SSD/Memory. All system-specific knowledge lives in the plugin; the swept
# dimension is passed via --sweep. New system = add one systems/<name>.sh.
#
# Universal knobs (handled by the engine, work for every system):
#   entry_count (db size) | cores | zipf | mem
# System knobs (handled by the plugin's sys_apply_param, e.g. qmdb: shard, ratio):
#   anything else -> plugin sed's the source and the engine rebuilds if needed.
#
# Examples:
#   ./run_bench.sh --system qmdb --entry-count 1500000000
#   ./run_bench.sh --system qmdb --sweep shard=1,2,4,8,16
#   ./run_bench.sh --system qmdb --sweep entry_count=100000000,1500000000
#   ./run_bench.sh --system nomt --sweep leaf_cache=128,1024,4096 --entry-count 300000000
set -uo pipefail

ENGINE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$ENGINE_DIR/monitor/bench_core.sh"   # start_monitors/stop_monitors/drop_caches/calc_cpu_range

# ----------------------------- defaults -----------------------------
SYSTEM=""
BASE_DIR="/mnt/ssd"
ENTRY_COUNT=""            # empty => plugin default
CORES=""                 # empty => all physical cores
ZIPF="0"
MEM=""                   # empty => transient scope WITHOUT a memory cap (pure accounting)
WORKLOAD="synthetic"     # synthetic | eth
ETH_TRACE_DIR=""
SWEEP=""                 # KNOB=v1,v2,...
EXTRA=""                 # raw passthrough args to speed
DO_PLOT=1
declare -A OVERRIDE      # ops_per_block / hover_interval / hover_recreate_block / hover_write_block

usage() {
  sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

# ----------------------------- parse args -----------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --system)               SYSTEM="$2"; shift 2;;
    --base-dir)             BASE_DIR="$2"; shift 2;;
    --entry-count)          ENTRY_COUNT="$2"; shift 2;;
    --cores)                CORES="$2"; shift 2;;
    --zipf)                 ZIPF="$2"; shift 2;;
    --mem)                  MEM="$2"; shift 2;;
    --workload)             WORKLOAD="$2"; shift 2;;
    --eth-trace-dir)        ETH_TRACE_DIR="$2"; shift 2;;
    --sweep)                SWEEP="$2"; shift 2;;
    --extra)                EXTRA="$2"; shift 2;;
    --no-plot)              DO_PLOT=0; shift;;
    --ops-per-block)        OVERRIDE[ops_per_block]="$2"; shift 2;;
    --hover-interval)       OVERRIDE[hover_interval]="$2"; shift 2;;
    --hover-recreate-block) OVERRIDE[hover_recreate_block]="$2"; shift 2;;
    --hover-write-block)    OVERRIDE[hover_write_block]="$2"; shift 2;;
    -h|--help)              usage; exit 0;;
    *) echo "❌ unknown arg: $1"; usage; exit 1;;
  esac
done

[[ -n "$SYSTEM" ]] || { echo "❌ --system required"; usage; exit 1; }

# ----------------------------- load plugin -----------------------------
PLUGIN="$ENGINE_DIR/systems/${SYSTEM}.sh"
[[ -f "$PLUGIN" ]] || { echo "❌ no plugin: $PLUGIN"; exit 1; }
# shellcheck source=/dev/null
source "$PLUGIN"

# ----------------------------- preflight -----------------------------
command -v cargo >/dev/null 2>&1 || { echo "❌ cargo not in PATH"; exit 1; }
[[ -d "$BASE_DIR" ]] || { echo "❌ base dir '$BASE_DIR' does not exist"; exit 1; }   # req(1)
cd "$ENGINE_DIR"                                # build + run from repo root
ulimit -n "${SYS_ULIMIT_NOFILE:-65535}"        # req(2)

# NUMA / UMA topology
if [ -d /sys/devices/system/node/node1 ] && command -v numactl >/dev/null 2>&1; then
  IS_NUMA=true;  FIXED_CORE=40; HT_OFFSET=80; MODE_DIR="NUMA"; NUMA_NODE=0
else
  IS_NUMA=false
  PHYS_CORES=$(lscpu -p=core 2>/dev/null | grep -v '^#' | sort -u | wc -l | tr -d ' ')
  [[ -z "$PHYS_CORES" || "$PHYS_CORES" -eq 0 ]] && PHYS_CORES=$(nproc)
  LOGICAL=$(nproc); FIXED_CORE=$PHYS_CORES
  if [ "$LOGICAL" -gt "$PHYS_CORES" ]; then HT_OFFSET=$PHYS_CORES; else HT_OFFSET=$LOGICAL; fi
  MODE_DIR="UMA"; NUMA_NODE=0
fi
[[ -n "$CORES" ]] || CORES="$FIXED_CORE"
echo "🔍 topology: ${MODE_DIR} phys=${FIXED_CORE} ht_offset=${HT_OFFSET}"

CPU_LIST_NODE0_ALL=(0 4 8 12 16 20 24 28 32 36 40 44 48 52 56 60 64 68 72 76
                    80 84 88 92 96 100 104 108 112 116 120 124 128 132 136 140 144 148 152 156)
gen_cpu_list() { local n=$1 m=${#CPU_LIST_NODE0_ALL[@]}; ((n>m)) && n=$m
  local s=("${CPU_LIST_NODE0_ALL[@]:0:$n}"); local IFS=,; echo "${s[*]}"; }

# disk device for the monitor (derived from base dir; the binary derives its own from db_dir)
DISK_DEVICE="$(findmnt -n -o SOURCE --target "$BASE_DIR" 2>/dev/null | sed 's/\[.*//' | sed 's#.*/##')"
[[ -n "$DISK_DEVICE" ]] || DISK_DEVICE="md0"
echo "🔍 monitor disk: $DISK_DEVICE"

# randsrc (req 3)
RANDSRC="$BASE_DIR/randsrc.dat"
if [[ "${SYS_NEEDS_RANDSRC:-0}" == "1" && ! -f "$RANDSRC" ]]; then
  echo "🎲 randsrc missing -> head -c 10M </dev/urandom > $RANDSRC"
  head -c 10M </dev/urandom > "$RANDSRC"
fi

# plotting venv (req 4 + 7) — install once, best-effort
VENV="$BASE_DIR/.plotvenv"
PLOT_OK=0
ensure_plot_env() {
  [[ -x "$VENV/bin/python" ]] && return 0
  command -v python3 >/dev/null 2>&1 || return 1
  echo "🐍 creating plotting venv at $VENV (one-time)..."
  if ! python3 -m venv "$VENV" 2>/dev/null; then
    echo "   installing python3-venv/pip via apt..."
    sudo apt-get update -qq >/dev/null 2>&1
    sudo apt-get install -y python3-venv python3-pip >/dev/null 2>&1
    python3 -m venv "$VENV" 2>/dev/null || return 1
  fi
  "$VENV/bin/pip" install -q --upgrade pip >/dev/null 2>&1
  "$VENV/bin/pip" install -q pandas matplotlib || return 1
}
if [[ $DO_PLOT == 1 ]]; then
  if ensure_plot_env; then PLOT_OK=1; else echo "⚠ plotting unavailable (will skip figures)"; fi
fi

# ----------------------------- expand sweep -----------------------------
if [[ -n "$SWEEP" ]]; then
  SWEEP_KNOB="${SWEEP%%=*}"; IFS=',' read -ra SWEEP_VALUES <<< "${SWEEP#*=}"
else
  SWEEP_KNOB="single"; SWEEP_VALUES=("default")
fi
is_universal() { case " entry_count cores zipf mem " in *" $1 "*) return 0;; *) return 1;; esac; }

# Rebuild every iter only when sweeping a compile-time SYSTEM knob.
REBUILD_PER_ITER=0
if [[ "$SWEEP_KNOB" != "single" ]] && ! is_universal "$SWEEP_KNOB"; then
  if sys_is_compiletime "$SWEEP_KNOB"; then REBUILD_PER_ITER=1; fi
fi

# Apply plugin defaults (e.g. qmdb ratio=7 shard=16) before the first build.
declare -f sys_set_defaults >/dev/null 2>&1 && sys_set_defaults
[[ $REBUILD_PER_ITER == 0 ]] && { echo "🔨 building $SYS_NAME once..."; sys_build || exit 1; }

# ----------------------------- run loop -----------------------------
DB_DIR="$BASE_DIR/${SYS_DB_SUBDIR}"
RUN_PID=""
trap 'stop_monitors 2>/dev/null || true; [[ -n "$RUN_PID" ]] && kill "$RUN_PID" 2>/dev/null || true' EXIT INT TERM

for val in "${SWEEP_VALUES[@]}"; do
  EC="${ENTRY_COUNT:-${SYS_DEFAULT_ENTRY_COUNT:-1500000000}}"
  RUN_CORES="$CORES"; RUN_ZIPF="$ZIPF"; RUN_MEM="$MEM"; TAG="run"
  if [[ "$SWEEP_KNOB" != "single" ]]; then
    if is_universal "$SWEEP_KNOB"; then
      case "$SWEEP_KNOB" in
        entry_count) EC="$val";; cores) RUN_CORES="$val";; zipf) RUN_ZIPF="$val";; mem) RUN_MEM="$val";;
      esac
    else
      sys_apply_param "$SWEEP_KNOB" "$val" || exit 1
      [[ $REBUILD_PER_ITER == 1 ]] && { echo "🔨 rebuild for ${SWEEP_KNOB}=${val}"; sys_build || exit 1; }
    fi
    TAG="${SWEEP_KNOB}_${val}"
  fi

  TS="$(date +%Y%m%d-%H%M%S)"
  EXP_DIR="results/${SYS_NAME}/${MODE_DIR}/${TAG}_${TS}"     # req(6): timestamped, never overwritten
  mkdir -p "$EXP_DIR"
  MAIN_LOG="$EXP_DIR/benchmark_output.log"

  echo "============================================================"
  echo "🧪 $SYS_NAME | $TAG | EC=$EC cores=$RUN_CORES zipf=$RUN_ZIPF mem=${RUN_MEM:-none}"
  echo "   -> $EXP_DIR"
  echo "============================================================"

  sudo rm -rf "$DB_DIR"
  drop_caches

  # core binding
  if [[ "$IS_NUMA" == true ]]; then
    CORE_BIND=(numactl --physcpubind="$(gen_cpu_list "$RUN_CORES")" --membind="$NUMA_NODE")
  else
    CORE_BIND=(taskset -c "$(calc_cpu_range "$RUN_CORES" "$HT_OFFSET")")
  fi

  # transient cgroup scope (pure accounting unless --mem set)
  SCOPE=(sudo systemd-run --scope -p MemoryAccounting=yes)
  [[ -n "$RUN_MEM" ]] && SCOPE+=(-p MemoryMax="$RUN_MEM" -p MemoryHigh="$RUN_MEM")

  # speed args: single source db_dir (binary derives its own monitor disk from it)
  ARGS=(--db-dir "$DB_DIR" --result-log "$PWD/$EXP_DIR/metrics.log"
        --entry-count "$EC" --hover-zipf-theta "$RUN_ZIPF")
  # append SYS_BASE_ARGS, but DROP any flag the user overrode (avoid clap duplicates)
  SKIP_NEXT=0
  for tok in "${SYS_BASE_ARGS[@]}"; do
    if [[ $SKIP_NEXT == 1 ]]; then SKIP_NEXT=0; continue; fi
    if [[ "$tok" == --* ]]; then
      okey="${tok#--}"; okey="${okey//-/_}"
      if [[ -n "${OVERRIDE[$okey]+x}" ]]; then SKIP_NEXT=1; continue; fi
    fi
    ARGS+=("$tok")
  done
  [[ "${SYS_NEEDS_RANDSRC:-0}" == "1" ]] && ARGS+=(--randsrc-filename "$RANDSRC")
  for k in "${!OVERRIDE[@]}"; do ARGS+=("--${k//_/-}" "${OVERRIDE[$k]}"); done
  if [[ "$WORKLOAD" == "eth" ]]; then
    [[ -n "$ETH_TRACE_DIR" ]] || { echo "❌ --workload eth needs --eth-trace-dir"; exit 1; }
    ARGS+=(--eth-trace-dir "$ETH_TRACE_DIR")
  fi
  # shellcheck disable=SC2206
  [[ -n "$EXTRA" ]] && ARGS+=($EXTRA)

  CMD=("${SCOPE[@]}" prlimit --nofile="${SYS_ULIMIT_NOFILE:-65535}:${SYS_ULIMIT_NOFILE:-65535}"
       "${CORE_BIND[@]}" ./target/release/speed "${ARGS[@]}")
  echo "🚀 ${CMD[*]}"
  "${CMD[@]}" > "$MAIN_LOG" 2>&1 &
  RUN_PID=$!
  start_monitors "$RUN_PID" "$DISK_DEVICE" "$EXP_DIR"
  wait "$RUN_PID"; RC=$?
  stop_monitors
  RUN_PID=""
  echo "✅ exit=$RC -> $EXP_DIR"

  # auto-plot (req 4): CPU / SSD / Memory from hardware_usage.csv
  if [[ $PLOT_OK == 1 ]]; then
    "$VENV/bin/python" "$ENGINE_DIR/monitor/make_all_plots.py" -s "$EXP_DIR" || echo "⚠ plot failed"
  fi
  sleep 3
done

trap - EXIT
echo "🎉 ALL DONE ($SYS_NAME)."
