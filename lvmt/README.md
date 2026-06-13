# Authenticated Storage Benchmarks

## Introduction

This program is a modular benchmarking tool for authenticated storage, designed to support various key-value based backends, authenticated storage, and tasks while collecting a wide range of metrics data. The currently supported backends are:

- An [in-memory database](https://github.com/openethereum/openethereum/tree/main/crates/db/memory-db) that organizes key-value pairs in a hashmap, implemented in [OpenEthereum](https://github.com/openethereum/openethereum)
- [RocksDB](https://rocksdb.org/), a popular choice for Rust-based public chains
- [MDBX](https://github.com/erthink/libmdbx), utilized by [Erigon](https://github.com/ledgerwatch/erigon)

The authenticated storage systems supported include:

- The [original Merkle Patricia Trie (MPT)](https://github.com/openethereum/openethereum/tree/main/crates/db/patricia-trie-ethereum) implementation from OpenEthereum
- The [Layered Merkle Patricia Tries (LMPTs)](https://github.com/Conflux-Chain/conflux-rust/tree/master/core/storage) [1] used in Conflux
- A modified version of [RainBlock's MPT](https://github.com/RainBlock/merkle-patricia-tree) [2], which stores the bottom layers locally on storage instead of using a distributed in-memory system as in the original work
- The [multi-Layer Versioned Multipoint Trie (LVMT)](https://github.com/ChenxingLi/authenticated-storage-benchmarks/tree/master/asb-authdb/lvmt-db)[3] in our new work.
- A single Authenticated Multipoint evaluation Tree (AMT)[4], which also serves as a building block within the multi-Layer Versioned Multipoint Trie (LVMT).

The available tasks are:

- Random workload, which accesses random keys
- Real traces from Ethereum, with a provided [evm-io-tracker](https://github.com/ChenxingLi/evm-io-tracker) to fetch traces from the [Ethereum trace API](https://openethereum.github.io/JSONRPC-trace-module) (supported in OpenEthereum and Erigon)

The collected metrics data includes:

- Execution time of tasks
- The average and percentiles of read and write amplifications to the backend database
- The percentiles of time cost for reading and writing to the backend database
- Memory usage of the program
- CPU profiling data caught by [pprof-rs](https://github.com/tikv/pprof-rs)

This comprehensive toolset is developed for tuning and evaluating LVMT. It enables users to benchmark and compare various authenticated storage solutions, providing valuable insights into their performance and resource utilization.

## Building the Project

The following steps outline how to build the project on Ubuntu 22.04.

### Prerequisites

Before building the project, ensure that you have the following prerequisites installed:

- Ubuntu 22.04
- Rust (version 1.67.0)
- Build tools: `build-essential`, `libssl-dev`, `pkg-config`, `libclang-dev`, `cmake`

### Steps

Follow the steps below to build the project:

1. Update the package list:
    
    ```bash
    sudo apt update
    ```
    
2. Install Rust and Cargo:
    
    ```bash
    sudo apt install rustc cargo
    ```
    
3. Install additional dependencies:
    
    ```bash
    sudo apt install build-essential libssl-dev pkg-config libclang-dev cmake
    ```

4. Install Python3 and Pip:

    ```bash
    sudo apt install python3 python3-pip
    ```

5. Install the required modules:
    ```bash
    pip3 install numpy
    ```

    
6. Clone the repository and navigate to the project directory:
    
    ```bash
    git clone https://github.com/ChenxingLi/authenticated-storage-benchmarks.git
    cd authenticated-storage-benchmarks
    ```
    
7. Build the project:
    
    ```bash
    cargo build --release
    ```
    
    Note: The build time may take about minutes to complete.

8. Before evaluating LVMT and AMT, create a designated folder named `pp` that will be used for storing all cryptography parameters.
    
    ```bash
    mkdir pp
    ```

    **Note:** When using AMT or LVMT for the first time, it may take anywhere from minutes to hours to initialize the cryptography parameters. Alternatively, you can [download the generated cryptography parameters](https://drive.google.com/file/d/1pHiHpZ4eNee17C63tSDEvmcEVtv23-jK/view?usp=sharing) and place the files in the folder `./pp`, but this option is only available for `lvmt` and `amt16`. (See the [section](#authenticated-storage-selection) below.)

9. Prepare the task files for real Ethereum traces. [Download trace data](https://1drv.ms/f/s!Au7Bejk2NtCskXmvzwgS2WgDvuGV?e=ESZ5na) or fetch traces with [evm-io-tracker](https://github.com/ChenxingLi/evm-io-tracker). Place the tasks files under the path `./trace`.

10. Now you can execute the preconfigured evaluation tasks by running the following command (requires 300GB free storage):

    ```bash
    python3 run.py
    ```

    This [instruction](#running-experiments-with-memory-constraints) can help you apply the memory limit constraint for specific tasks.

11. Use [asb-plotter](https://github.com/ChenxingLi/asb-plotter) to parse the experiment traces and plot figures.

## Conduct the Evaluation

If you want to customize the evaluation, you can modify the parameters of the `asb-main` program to suit your requirements. The following command is an example of how to run the program:

```bash
./target/release/asb-main --no-stat -k 1m -a mpt
```

This command will evaluate the performance of OpenEthereum's MPT by performing random read/write operations using 1 million distinct keys and outputting the benchmark metrics. 

By default, the program will request the merkle root from OpenEthereum's MPT every 10,000 operations (which we refer to as one epoch), and it will print evaluation metrics every 2 epochs.

Note: You can combine the compile step and running step into one command by replacing `build` with `run`, followed by `--`and the program parameters. For example:

```bash
cargo run --release -- --no-stat -k 1m -a mpt
```

This command will build the project and then run the result with the provided parameters. This is useful when running with different compile options.    

## Compile Features

To add features to `cargo run` and `cargo build`, use the syntax `cargo build --features --asb-authdb/light-hash`. Available features include:

- `asb-authdb/light-hash`: Replaces `keccak256` with the faster `blake2b` hash function.
- `asb-authdb/thread-safe`: Enable a thread-safe implementation for authenticated storage systems. (Currently, only RainBlock's MPT (Modified Patricia Trie) has different implementations between thread-safe and non-thread-safe modes. )

## Program Options

### Backend Selection

Specify the backend using `--backend <name>` or `-b <name>` from three key-value based databases:

- `rocksdb`: RocksDB, the default option.
- `memory`: In-memory database.
- `mdbx`: MDBX (not fully tested).

For non-memory backends, set the data storage path with `--db <dir>` (default: `./__benchmarks`). For RocksDB, configure cache size using `--cache-size <cache-size-in-MB>` (default: 1500).

### Authenticated Storage Selection

Choose an authenticated storage with `-a <name>` or `--algorithm <name>`. Options include:

- `raw`: No authenticated storage; writes changes directly to the backend.
- `lvmt`: The multi-Layer Versioned Multipoint Trie (LVMT)[3].
- `mpt`: OpenEthereum's MPT implementation.
- `rain`: A variant of RainBlocks's MPT[2].
- `amt<n>`: A single AMT with `n` heights (e.g., `amt20`). Maximum `n` value: 28.
- `lmpts`: The Layered Merkle Patricia Tries (LMPTs) [1] used in Conflux. It is tricky to evaluate LMPTs. See the last section for details.

For LVMT, configure the number of shards in proof sharding with `--shards <shards>`. Shard numbers must be a power of two (from 1 to 65536). Without this option, LVMT won't maintain associated information for proof.

### Task Types

Two types of tasks are available: random tasks and real Ethereum traces.

For random tasks, set the number of distinct keys using `--total-keys <number>` or `-k <number>`. You can also use the suffixes `k`, `m`, and `g` to represent kilo, million, and billion, respectively. For example, `2m` represents 2 million keys. By default, the program requests the Merkle root from authenticated storage every 10,000 operations (one epoch). Change this setting with `--epoch-size <operations>`.

For real Ethereum traces, enable with `--real-trace`. Set the trace data directory using `--trace <trace-dir>` (default: `./trace`). 

### Warmup Process

Before performance evaluation, the program warms up by inserting random values for keys in random tasks or importing initial ledger states for real traces.

For random tasks, the warmup process can be disabled with `--no-warmup`.

To share warmed-up databases between benchmark tasks, save warmup results using `--warmup-to` and load existing results with `--warmup-from`.

### Metric Data Collection

Customize metric data collection with the following options:

- `--report-epoch <epoch-number>` (default: 2): Sets the period for printing metric results to stdout.
- `--no-stat`: Disables backend statistics processing for more accurate running time measurements.
- `--stat-mem`: Periodically outputs memory usage data.
- `--pprof-report-to <report_dir>`: Enables pprof profiling and saves results to `report_dir`. If enabled, configure the report period in epochs using `--profile-epoch <epochs>`.

### Evaluation Duration

Control the evaluation duration using `--max-time <duration-in-seconds>` and `--max-epoch <max-epochs>`. The evaluation stops when either threshold is reached.

### Debugging Options

Use the following options for debugging purposes:

- `--seed <seed>`: Sets the random seed.
- `--print-root`: Prints the storage root every epoch

## Running Experiments with Memory Constraints

Our paper's experiments were conducted with a memory limit of 8GB. If you don't have a machine with exactly 8GB of memory, you'll need to limit the memory through cgroup, Docker, or some other method. Below is a solution using cgroup, **assuming you have sudo privileges on the system.**

### Install CGroup and Set Memory Limit

Firstly, you need to install cgroup on your system. On Ubuntu, you can do this using the following command:

```bash
sudo apt-get install cgroup-bin
```

Then, create a cgroup named `lvmt` with a memory limit of 8GB:

```bash
sudo cgcreate -g memory:/lvmt
sudo cgset -r memory.limit_in_bytes=$((8*1024*1024*1024)) lvmt
```

### Configure Sudo Permissions

Next, ensure that `cgclassify` and `sysctl` can be run with sudo command without requiring a password. You can achieve this by adding the following lines to your sudoers:

```bash
<your_username> ALL=NOPASSWD: /usr/bin/cgclassify
<your_username> ALL=NOPASSWD: /sbin/sysctl
```

Replace `<your_username>` with your actual username. You can edit the sudoers file using `sudo visudo`, or use `vim` as your editor by:

```bash
sudo update-alternatives --set editor /usr/bin/vim.basic
sudo visudo
```

### Create and Configure the Shell Script

Create a shell script with the following content:

```bash
#!/bin/bash

# Classify the current shell into the 'lvmt' memory cgroup
sudo cgclassify -g memory:/lvmt $$
COMMAND=$@

bash -c "$COMMAND"
# Alternatively, if you want to load your custom environment 
# (e.g., defined in .zprofile) before executing the command, 
# run the following line instead.
# bash -c "source ~/.zprofile && $COMMAND"
```

This script allows you to execute a task with a constrained memory limit under regular user mode rather than a superuser mode.

Make sure to give this script execute permissions:

```
chmod +x <your_script.sh>
```

### Modify the Preconfigured Run Script

Edit the `run.py` file to replace the `CGRUN_PREFIX` with the path to the script you created in the previous step, e.g., `CGRUN_PREFIX=/path/to/your_script.sh`. 

This setup will ensure that your experiments run with a memory constraint of 8GB, closely replicating the conditions of the experiments in our paper.

## Evaluating LMPTs: A Special Case

Evaluating Lightweight MPTs (LMPTs) can be challenging due to the strong coupling between its authenticated storage and the RocksDB backend. Additionally, since RocksDB is a C++ library, Rust allows only one crate to depend on the same C++ library, leading to a dependency conflict when compiling LMPTs with RocksDB in this tool.

To assess LMPTs, you'll need to make manual adjustments to the `asb-backend/Cargo.toml` file. Specifically, comment out the current dependency `cfx-kvdb-rocksdb` and uncomment the dependencies for the `lmpts-backend`. Then, compile using the `asb-authdb/lmpts` feature to evaluate LMPTs. This workaround enables you to properly benchmark LMPTs without encountering dependency conflicts in Rust.

To build with the `asb-authdb/lmpts` feature, use the following command:

```
cargo build --release --features asb-authdb/lmpts
```

## References

[1] Choi, Jemin Andrew, Sidi Mohamed Beillahi, Peilun Li, Andreas Veneris, and Fan Long. "LMPTs: Eliminating Storage Bottlenecks for Processing Blockchain Transactions." In *2022 IEEE International Conference on Blockchain and Cryptocurrency (ICBC)*, pp. 1-9. IEEE, 2022.

[2] Ponnapalli, Soujanya, Aashaka Shah, Souvik Banerjee, Dahlia Malkhi, Amy Tai, Vijay Chidambaram, and Michael Wei. "RainBlock: Faster Transaction Processing in Public Blockchains." In *USENIX Annual Technical Conference*, pp. 333-347. 2021.

[3] Chenxing Li, Sidi Mohamed Beillahi, Guang Yang, Ming Wu, Wei Xu, and Fan Long. "LVMT: An Efﬁcient Authenticated Storage for Blockchain". In *USENIX Symposium on Operating Systems Design and Implementation (OSDI)*. 2023.

[4] Alin Tomescu, Robert Chen, Yiming Zheng, Ittai Abraham, Benny Pinkas, Guy Golan Gueta, and Srinivas Devadas. Towards scalable threshold cryptosystems. In Proceedings of the *2020 IEEE Symposium on Security and Privacy, pages 877–893*. IEEE, 2020.