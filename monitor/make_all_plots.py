#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
make_all_plots.py — plot hardware resource usage from the unified hwmonitor CSV.

Reads exactly ONE file: hardware_usage.csv (produced by hwmonitor.sh). Columns:
  timestamp, cpu_util_pct, cpu_user_pct, cpu_sys_pct, cpu_iowait_pct,
  proc_cpu_pct, threads, mem_anon_mb, mem_file_mb, mem_current_mb,
  ssd_rps, ssd_wps, ssd_read_MBps, ssd_write_MBps, ssd_qdepth, ssd_util_pct

Produces 3 figures: cpu_usage.png, ssd_usage.png, memory_usage.png.

It does NOT read any benchmark output log. If a timestats.log happens to be in
the source dir, phase boundaries are overlaid as dashed lines (optional, auto-
detected) — but it is never required.

usage: make_all_plots.py -s <dir-with-hardware_usage.csv> [-t <out-dir>]
"""
import argparse
import os
import re
from math import ceil

import pandas as pd
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

try:
    plt.style.use("seaborn-v0_8-darkgrid")
except Exception:
    pass

DEFAULT_DPI = 200
CSV_NAME = "hardware_usage.csv"


def ensure_dir(d):
    os.makedirs(d, exist_ok=True)


def load_hwcsv(source_dir):
    fname = os.path.join(source_dir, CSV_NAME)
    if not os.path.exists(fname):
        raise SystemExit(f"[error] {CSV_NAME} not found in {source_dir}")
    df = pd.read_csv(fname, comment="#", on_bad_lines="skip")
    df["timestamp"] = pd.to_datetime(df["timestamp"], errors="coerce")
    for c in [c for c in df.columns if c != "timestamp"]:
        df[c] = pd.to_numeric(df[c], errors="coerce")
        df.loc[df[c] < 0, c] = float("nan")  # clip /proc counter-reset glitches (-> gap, not spike)
    df.dropna(subset=["timestamp"], inplace=True)
    # drop post-exit sentinel rows (process gone -> all-zero mem/threads)
    if "mem_anon_mb" in df.columns:
        df = df[df["mem_anon_mb"] > 0].copy()
    df.sort_values("timestamp", inplace=True)
    df.reset_index(drop=True, inplace=True)
    t0 = df["timestamp"].min()
    df["t_min"] = (df["timestamp"] - t0).dt.total_seconds() / 60.0
    return df, t0


def adaptive_step(max_min):
    for step in (1, 2, 5, 10, 15, 30, 60, 120):
        if max_min / step <= 12:
            return step
    return 240


def build_ticks(max_min):
    if not max_min or max_min <= 0:
        return [0]
    step = adaptive_step(max_min)
    top = ceil(max_min / step) * step
    return list(range(0, int(top) + 1, step))


def load_event_minutes(source_dir, t0, timestats_name="timestats.log"):
    """Optional phase-boundary lines from timestats.log if it exists; else []."""
    path = os.path.join(source_dir, timestats_name)
    if not os.path.exists(path):
        return []
    pat = re.compile(r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2}))")
    evs = []
    with open(path) as f:
        for line in f:
            m = pat.search(line)
            if not m:
                continue
            try:
                ts = pd.to_datetime(m.group(1), utc=True).tz_localize(None)
                evs.append((ts - t0).total_seconds() / 60.0)
            except Exception:
                continue
    return sorted(set(evs))


def vlines(ax, events, max_min):
    for m in events:
        if -1 <= m <= max_min + 1:
            ax.axvline(m, color="gray", linestyle="--", linewidth=0.8, alpha=0.7)


def finish_x(ax, max_min):
    ticks = build_ticks(max_min)
    ax.set_xticks(ticks)
    ax.set_xlim(0, max(ticks))
    ax.set_xlabel("Minutes from start")


def plot_cpu(df, target_dir, events):
    max_min = df["t_min"].max()
    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(12, 8), sharex=True)

    ax1.plot(df.t_min, df.cpu_util_pct, color="tab:red", lw=1.5, label="Total util")
    ax1.plot(df.t_min, df.cpu_user_pct, color="tab:blue", lw=1.0, label="User")
    ax1.plot(df.t_min, df.cpu_sys_pct, color="tab:green", lw=1.0, label="System")
    ax1.plot(df.t_min, df.cpu_iowait_pct, color="tab:orange", lw=1.0, label="IOwait")
    ax1.set_ylabel("System CPU %")
    ax1.set_ylim(0, 105)
    ax1.set_title("System-wide CPU Utilization")
    ax1.legend(loc="upper left", ncol=4, fontsize=8)
    vlines(ax1, events, max_min)

    ax2.plot(df.t_min, df.proc_cpu_pct, color="tab:purple", lw=1.5,
             label="Process CPU % (100% = 1 core)")
    ax2.set_ylabel("Process CPU %", color="tab:purple")
    ax2b = ax2.twinx()
    ax2b.plot(df.t_min, df.threads, color="tab:gray", lw=1.0, ls="--", label="Threads")
    ax2b.set_ylabel("Threads", color="tab:gray")
    ax2.set_title("Benchmark Process CPU & Thread Count")
    ax2.legend(loc="upper left", fontsize=8)
    ax2b.legend(loc="upper right", fontsize=8)
    vlines(ax2, events, max_min)

    finish_x(ax2, max_min)
    fig.tight_layout()
    out = os.path.join(target_dir, "cpu_usage.png")
    fig.savefig(out, bbox_inches="tight", dpi=DEFAULT_DPI)
    plt.close(fig)
    print(f"[ok] {out}")


def plot_ssd(df, target_dir, events):
    max_min = df["t_min"].max()
    fig, axes = plt.subplots(4, 1, figsize=(12, 11), sharex=True)

    ax = axes[0]
    ax.plot(df.t_min, df.ssd_rps, color="tab:blue", lw=1.0)
    ax.set_ylabel("Read IOPS", color="tab:blue")
    axb = ax.twinx()
    axb.plot(df.t_min, df.ssd_read_MBps, color="tab:green", lw=1.0, ls="--")
    axb.set_ylabel("Read MB/s", color="tab:green")
    ax.set_title("SSD Read")
    vlines(ax, events, max_min)

    ax = axes[1]
    ax.plot(df.t_min, df.ssd_wps, color="tab:red", lw=1.0)
    ax.set_ylabel("Write IOPS", color="tab:red")
    axb = ax.twinx()
    axb.plot(df.t_min, df.ssd_write_MBps, color="tab:purple", lw=1.0, ls="--")
    axb.set_ylabel("Write MB/s", color="tab:purple")
    ax.set_title("SSD Write")
    vlines(ax, events, max_min)

    ax = axes[2]
    ax.plot(df.t_min, df.ssd_util_pct, color="tab:orange", lw=1.0)
    ax.set_ylabel("Util %")
    ax.set_ylim(0, 105)
    ax.set_title("SSD Utilization")
    vlines(ax, events, max_min)

    ax = axes[3]
    ax.plot(df.t_min, df.ssd_qdepth, color="tab:brown", lw=1.0)
    ax.set_ylabel("Queue depth")
    ax.set_title("SSD Avg Queue Depth")
    vlines(ax, events, max_min)

    finish_x(axes[-1], max_min)
    fig.tight_layout()
    out = os.path.join(target_dir, "ssd_usage.png")
    fig.savefig(out, bbox_inches="tight", dpi=DEFAULT_DPI)
    plt.close(fig)
    print(f"[ok] {out}")


def plot_mem(df, target_dir, events):
    max_min = df["t_min"].max()
    fig, ax = plt.subplots(figsize=(12, 6))
    gb = 1024.0
    ax.plot(df.t_min, df.mem_anon_mb / gb, color="tab:blue", lw=2.0,
            label="Anonymous ≈ RSS (process)")
    ax.plot(df.t_min, df.mem_file_mb / gb, color="tab:green", lw=1.5, ls="--",
            label="Page cache (file)")
    ax.plot(df.t_min, df.mem_current_mb / gb, color="tab:red", lw=1.5,
            label="Total (memory.current)")
    ax.set_ylabel("Memory (GB)")
    ax.set_title("Memory Usage (cgroup counters) Over Time")
    ax.legend(loc="upper left")
    vlines(ax, events, max_min)
    finish_x(ax, max_min)
    fig.tight_layout()
    out = os.path.join(target_dir, "memory_usage.png")
    fig.savefig(out, bbox_inches="tight", dpi=DEFAULT_DPI)
    plt.close(fig)
    print(f"[ok] {out}")


def main():
    ap = argparse.ArgumentParser(
        description="Plot CPU / SSD / Memory from hardware_usage.csv (hwmonitor).")
    ap.add_argument("--source", "-s", required=True,
                    help=f"Directory containing {CSV_NAME}")
    ap.add_argument("--target", "-t", default=None,
                    help="Output dir for PNGs (default: source dir)")
    ap.add_argument("--timestats", default="timestats.log",
                    help="Optional phase-marker file, auto-detected in source dir")
    args = ap.parse_args()

    source_dir = os.path.abspath(args.source)
    target_dir = os.path.abspath(args.target) if args.target else source_dir
    ensure_dir(target_dir)

    df, t0 = load_hwcsv(source_dir)
    if df.empty:
        raise SystemExit("[error] hardware_usage.csv has no usable rows")
    events = load_event_minutes(source_dir, t0, args.timestats)
    print(f"rows={len(df)}  span={df['t_min'].max():.1f} min  events={len(events)}")

    plot_cpu(df, target_dir, events)
    plot_ssd(df, target_dir, events)
    plot_mem(df, target_dir, events)


if __name__ == "__main__":
    main()
