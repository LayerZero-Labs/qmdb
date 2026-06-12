#!/bin/bash
# hwmonitor.sh — unified hardware-usage monitor: CPU + MEMORY + SSD in ONE csv.
# usage: hwmonitor.sh <TARGET_PID> <RUN_FLAG> <DISK_DEV>
#
# Replaces pid_monitor.sh + diskmonitor.sh. Samples every INTERVAL seconds while
# RUN_FLAG exists; exits when the flag is removed or the target dies.
#
# MEMORY is read from the target's cgroup v2 counters (memory.stat anon/file +
# memory.current) — O(1) counter reads, NO smaps_rollup page-table walk. The old
# pid_monitor.sh read smaps_rollup 3x/sec, which walked ~4M PTEs under mmap_lock
# and depressed write throughput ~10% (reads immune). Falls back to
# /proc/PID/status VmRSS (still a plain counter, no walk) if no cgroup is found.
#
# Columns (hardware_usage.csv):
#   timestamp,
#   cpu_util_pct,cpu_user_pct,cpu_sys_pct,cpu_iowait_pct   (system-wide, /proc/stat)
#   proc_cpu_pct,threads                                   (target process, /proc/PID/stat)
#   mem_anon_mb,mem_file_mb,mem_current_mb                 (cgroup; anon ~= process RSS)
#   ssd_rps,ssd_wps,ssd_read_MBps,ssd_write_MBps,ssd_qdepth,ssd_util_pct  (/proc/diskstats)

TARGET_PID=$1
RUN_FLAG=$2
DEV=$3
INTERVAL=5

# --- walk from a sudo/systemd-run wrapper down to the real speed PID ---
if [ -e "/proc/$TARGET_PID/comm" ]; then
    c=$(cat "/proc/$TARGET_PID/comm" 2>/dev/null)
    if [[ "$c" == "sudo" || "$c" == "systemd-run" ]]; then
        echo "# wrapper ($c, pid=$TARGET_PID); finding real speed PID..." >&2
        for _ in $(seq 1 60); do
            real=$(pgrep -x speed | head -1)
            [ -n "$real" ] && break
            sleep 1
        done
        [ -n "$real" ] && { echo "# target: $TARGET_PID -> $real (release/speed)" >&2; TARGET_PID=$real; }
    fi
fi

# --- resolve cgroup v2 memory dir from /proc/PID/cgroup (unified line "0::<path>") ---
resolve_cgroup() {
    local path
    path=$(awk -F: '/^0::/{print $3}' "/proc/$TARGET_PID/cgroup" 2>/dev/null)
    if [ -n "$path" ] && [ -r "/sys/fs/cgroup${path}/memory.stat" ]; then
        echo "/sys/fs/cgroup${path}"
    fi
}
CG=$(resolve_cgroup)
if [ -n "$CG" ]; then echo "# memory source: cgroup $CG" >&2
else echo "# memory source: /proc/$TARGET_PID/status VmRSS (no isolated cgroup found)" >&2; fi

# --- disk sector size ---
SECTOR_SIZE=$(cat "/sys/block/$DEV/queue/hw_sector_size" 2>/dev/null || echo 512)

echo "timestamp,cpu_util_pct,cpu_user_pct,cpu_sys_pct,cpu_iowait_pct,proc_cpu_pct,threads,mem_anon_mb,mem_file_mb,mem_current_mb,ssd_rps,ssd_wps,ssd_read_MBps,ssd_write_MBps,ssd_qdepth,ssd_util_pct"

NPROC=$(nproc)

# --- initial baselines (deltas computed against these) ---
read -r _ cu1 cn1 cs1 ci1 cw1 cq1 cz1 ck1 _ _ < <(awk '/^cpu /{print}' /proc/stat)
cpu_idle1=$(( ci1 + cw1 )); cpu_busy1=$(( cu1 + cn1 + cs1 + cq1 + cz1 + ck1 )); cpu_tot1=$(( cpu_idle1 + cpu_busy1 ))
read -r pu1 ps1 _ < <(awk '{print $14,$15,$20}' "/proc/$TARGET_PID/stat" 2>/dev/null)
read -r drio1 drsec1 dwio1 dwsec1 diot1 dwt1 < <(awk -v d="$DEV" '$3==d{print $4,$6,$8,$10,$13,$14}' /proc/diskstats)

while [ -e "$RUN_FLAG" ] && [ -e "/proc/$TARGET_PID" ]; do
    sleep "$INTERVAL"
    ts=$(date '+%Y-%m-%d %H:%M:%S')

    # ---- system CPU (/proc/stat aggregate line) ----
    read -r _ cu2 cn2 cs2 ci2 cw2 cq2 cz2 ck2 _ _ < <(awk '/^cpu /{print}' /proc/stat)
    cpu_idle2=$(( ci2 + cw2 )); cpu_busy2=$(( cu2 + cn2 + cs2 + cq2 + cz2 + ck2 )); cpu_tot2=$(( cpu_idle2 + cpu_busy2 ))
    dtot=$(( cpu_tot2 - cpu_tot1 ))
    if [ "$dtot" -gt 0 ]; then
        cpu_util=$(awk -v t=$dtot -v i=$(( cpu_idle2 - cpu_idle1 )) 'BEGIN{printf "%.2f",(t-i)/t*100}')
        cpu_user=$(awk -v v=$(( cu2 - cu1 )) -v t=$dtot 'BEGIN{printf "%.2f",v/t*100}')
        cpu_sys=$(awk -v v=$(( cs2 - cs1 )) -v t=$dtot 'BEGIN{printf "%.2f",v/t*100}')
        cpu_iow=$(awk -v v=$(( cw2 - cw1 )) -v t=$dtot 'BEGIN{printf "%.2f",v/t*100}')
    else cpu_util=0.00; cpu_user=0.00; cpu_sys=0.00; cpu_iow=0.00; fi

    # ---- process CPU% + thread count (/proc/PID/stat: utime=14 stime=15 threads=20) ----
    if [ -r "/proc/$TARGET_PID/stat" ]; then
        read -r pu2 ps2 threads < <(awk '{print $14,$15,$20}' "/proc/$TARGET_PID/stat")
        if [ "$dtot" -gt 0 ]; then
            proc_cpu=$(awk -v p=$(( (pu2+ps2)-(pu1+ps1) )) -v t=$dtot -v c=$NPROC 'BEGIN{printf "%.2f",(p/t)*c*100}')
        else proc_cpu=0.00; fi
        pu1=$pu2; ps1=$ps2
    else proc_cpu=0.00; threads=0; fi

    # ---- memory via cgroup counters (NO smaps walk); re-resolve if scope vanished ----
    if [ -z "$CG" ] || [ ! -r "$CG/memory.stat" ]; then CG=$(resolve_cgroup); fi
    if [ -n "$CG" ] && [ -r "$CG/memory.stat" ]; then
        a=$(awk '/^anon /{print $2}' "$CG/memory.stat" 2>/dev/null)
        f=$(awk '/^file /{print $2}' "$CG/memory.stat" 2>/dev/null)
        cur=$(cat "$CG/memory.current" 2>/dev/null)
        mem_anon=$(( ${a:-0} / 1048576 )); mem_file=$(( ${f:-0} / 1048576 )); mem_cur=$(( ${cur:-0} / 1048576 ))
    else
        r=$(awk '/^VmRSS:/{print $2}' "/proc/$TARGET_PID/status" 2>/dev/null)
        mem_anon=$(( ${r:-0} / 1024 )); mem_file=0; mem_cur=$mem_anon
    fi

    # ---- SSD (/proc/diskstats: reads=4 sect_r=6 writes=8 sect_w=10 io_ticks=13 weighted=14) ----
    read -r drio2 drsec2 dwio2 dwsec2 diot2 dwt2 < <(awk -v d="$DEV" '$3==d{print $4,$6,$8,$10,$13,$14}' /proc/diskstats)
    rps=$(awk  -v x=$(( drio2-drio1 ))  -v dt=$INTERVAL 'BEGIN{if(x<0)x=0; printf "%.2f",x/dt}')
    wps=$(awk  -v x=$(( dwio2-dwio1 ))  -v dt=$INTERVAL 'BEGIN{if(x<0)x=0; printf "%.2f",x/dt}')
    rmb=$(awk  -v b=$(( (drsec2-drsec1)*SECTOR_SIZE )) -v dt=$INTERVAL 'BEGIN{if(b<0)b=0; printf "%.2f",(b/dt)/1048576}')
    wmb=$(awk  -v b=$(( (dwsec2-dwsec1)*SECTOR_SIZE )) -v dt=$INTERVAL 'BEGIN{if(b<0)b=0; printf "%.2f",(b/dt)/1048576}')
    qd=$(awk   -v w=$(( dwt2-dwt1 ))    -v d=$(( INTERVAL*1000 )) 'BEGIN{if(w<0)w=0; printf "%.2f",w/d}')
    util=$(awk -v t=$(( diot2-diot1 ))  -v d=$(( INTERVAL*1000 )) 'BEGIN{if(t<0)t=0; printf "%.2f",(t/d)*100}')

    echo "$ts,$cpu_util,$cpu_user,$cpu_sys,$cpu_iow,$proc_cpu,$threads,$mem_anon,$mem_file,$mem_cur,$rps,$wps,$rmb,$wmb,$qd,$util"

    # roll baselines
    cpu_idle1=$cpu_idle2; cpu_busy1=$cpu_busy2; cpu_tot1=$cpu_tot2
    cu1=$cu2; cn1=$cn2; cs1=$cs2; ci1=$ci2; cw1=$cw2; cq1=$cq2; cz1=$cz2; ck1=$ck2
    drio1=$drio2; drsec1=$drsec2; dwio1=$dwio2; dwsec1=$dwsec2; diot1=$diot2; dwt1=$dwt2
done

echo "# hwmonitor exiting $(date '+%F %T')" >&2
exit 0
