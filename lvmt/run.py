#!/usr/bin/env python3
import subprocess
import sys
from functools import partial
import numpy as np

CARGO_RUN = "cargo run --release --".split(" ")
DRY_RUN = False
WARMUP = "./warmup/v4"
RESULT = "./paper_experiment/osdi23"
# CGRUN_PREFIX = "cgrun"


def to_amt_size(key):
    if key == "fresh":
        return 1e8
    if key == "real":
        return 2e6
    elif key[-1].lower() in "kmg":
        exp = 10 ** ("kmg".index(key[-1].lower()) * 3 + 3)
        base = float(key[:-1]) * exp
    else:
        base = float(key)
    return int(np.ceil(np.log2(base * 5)))


def run(commands, output=None):
    if type(commands) is str:
        commands = commands.split(" ")

    if output is None:
        message = " ".join(commands)
    else:
        message = " ".join(commands) + f" > {output}"

    if DRY_RUN:
        print(message)
        return

    print("")
    print(f">>>>>>>>>>> {message}")
    sys.stdout.flush()

    if output is not None:
        output = open(output, "w")

    subprocess.run(commands, stdout=output)
    print(f"<<<<<<<<<<< done")
    sys.stdout.flush()


def warmup(alg, key, shards=None):
    if key == "fresh":
        return

    if alg == "amt":
        amt_size = to_amt_size(key)
        if amt_size > 26:
            return
        alg = alg + f"{amt_size:d}"

    prefix = CARGO_RUN + ["--no-stat", "--warmup-to", WARMUP]
    run("rm -rf __benchmarks")

    if key == "real":
        prefix = prefix + ["--real-trace"]
    else:
        prefix = prefix + f"-k {key}".split(" ")

    if shards is None:
        run(prefix + f"-a {alg}".split(" "))
    else:
        run(prefix + f"-a {alg} --shards {shards}".split(" "))


def bench(task, alg, key, shards=None):
    if alg == "amt":
        amt_size = to_amt_size(key)
        if amt_size > 26:
            return
        alg = alg + f"{amt_size:d}"

    prefix = CARGO_RUN + f"--max-time 5400 -a {alg}".split(" ")

    if task == "time":
        prefix = prefix + ["--no-stat"]
        if "CGRUN_PREFIX" in globals():
            prefix = globals()["CGRUN_PREFIX"].split(" ") + prefix
            run("sudo sysctl -w vm.drop_caches=3")
    else:
        pass

    if key != "real":
        prefix = prefix + "--max-epoch 200".split(" ")

    if key == "fresh":
        prefix = prefix + ["--no-warmup"]
    else:
        prefix = prefix + f"--warmup-from {WARMUP}".split(" ")

    if key == "fresh":
        prefix = prefix + f"-k 10g".split(" ")
    elif key == "real":
        if alg in ["rain", "mpt"]:
            prefix = prefix + f"--real-trace --report-epoch 1".split(" ")
        else:
            prefix = prefix + f"--real-trace --report-epoch 25".split(" ")
    else:
        prefix = prefix + f"-k {key}".split(" ")

    if task == "stat":
        prefix = prefix + "--cache-size 8192".split(" ")
    elif alg in ["raw", "mpt"]:
        prefix = prefix + "--cache-size 4096".split(" ")
    else:
        prefix = prefix + "--cache-size 2048".split(" ")

    run("rm -rf __benchmarks")


    if shards is None:
        output = f"{RESULT}/{task}_{alg}_{key}.log"
        run(prefix, output)
    else:
        output = f"{RESULT}/{task}_{alg}{shards}_{key}.log"
        run(prefix + f"--shards {shards}".split(" "), output)


bench_time = partial(bench, "time")
bench_stat = partial(bench, "stat")


def warmup_all():
    for key in ["real", "1m", "1600k", "2500k", "4m", "6300k", "10m", "16m", "25m", "40m", "63m", "100m"]:
        warmup("raw", key)
        warmup("lvmt", key)
        warmup("rain", key)
        warmup("mpt", key)
        for shards in [64, 16, 1]:
            if shards == 1 and key in ["real", "16m", "25m", "40m", "63m", "100m"]:
                continue
            warmup("lvmt", key, shards)

def bench_all_time():
    for key in ["fresh", "real", "1m", "1600k", "2500k", "4m", "6300k", "10m", "16m", "25m", "40m", "63m", "100m"]:
        bench_time("raw", key)
        bench_time("lvmt", key)
        bench_time("rain", key)
        bench_time("mpt", key)
        for shards in [64, 16, 1]:
            if shards == 1 and key in ["real", "16m", "25m", "40m", "63m", "100m"]:
                continue
            bench_time("lvmt", key, shards)

def bench_all_stat():
    for key in ["fresh", "real", "1m", "10m", "100m"]:
        bench_stat("raw", key)
        bench_stat("lvmt", key)
        bench_stat("rain", key)
        bench_stat("mpt", key)
        for shards in [64, 16]:
            bench_stat("lvmt", key, shards)



run("rm -rf __reports __benchmarks")
run(f"mkdir -p {WARMUP}")
run(f"mkdir -p {RESULT}")

warmup_all()
bench_all_time()
bench_all_stat()
