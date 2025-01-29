# QMDB: Quick Merkle Database

![Build Status](https://github.com/LayerZero-Labs/qmdb/actions/workflows/build.yml/badge.svg)
![Tests](https://github.com/LayerZero-Labs/qmdb/actions/workflows/tests.yml/badge.svg)

## Overview

The Quick Merkle Database (QMDB) is a high-performance verifiable key-value store, designed to optimize blockchain state storage.
It is designed to take advantage of modern SSDs and minimize flash write amplification with an append-only design.
QMDB can perform in-memory Merklelization with minimal DRAM usage, and offers efficient cryptographic proofs for inclusion, exclusion, and historical states.

Read the QMDB paper here: <https://arxiv.org/pdf/2501.05262>

*QMDB is ongoing research. Designed for high performance and practical use, some features are still evolving. We invite feedback and contributions from the community.*

## Use Cases

- **Blockchain State Storage**: Ideal for maintaining verifiable state in decentralized systems.
- **Database Optimization**: Useful for any application requiring high-performance verifiable key-value storage.

## Features

- **SSD-Optimized Design**  
  Reduces flash write amplification by storing updates as append-only twigs.

- **In-Memory Merkleization**  
  Minimizes disk I/O for proofs and updates, requiring only a small DRAM footprint.

- **Low I/O Overhead**  
  Achieves O(1) I/O per update and just one SSD read per state access.

- **High Throughput**  
  Demonstrated 6× gains over RocksDB and 8× over state-of-the-art verifiable databases.

- **Scalable Architecture**  
  Validated on datasets up to 15 billion entries, with projections up to 280 billion entries on a single machine.

- **Broad Hardware Compatibility**  
  Runs effectively on both consumer-grade PCs and enterprise servers, lowering barriers to blockchain participation.

## Key data structures

- **Entry** ([`qmdb/src/entryfile/entry.rs`](qmdb/src/entryfile/entry.rs)): The primitive data structure in QMDB, with each Entry corresponding to a single key-value pair.
- **Twigs** ([`qmdb/src/merkletree/twig.rs`](qmdb/src/merkletree/twig.rs)): A compact and efficient representation of the Merkle tree, minimizing DRAM usage by keeping most data on SSD.

## Installation

To get started, clone the repository:

```bash
git clone https://github.com/LayerZero-Labs/qmdb
cd qmdb
```

The following pre-requisites are required to build QMDB:

- g++
- linux-libc-dev
- libclang-dev
- unzip
- libjemalloc-dev
- make

We provide a script to install the pre-requisites on Ubuntu:

```bash
./install-prereqs-ubuntu.sh
```

Build the project using Cargo:

```bash
cargo build --release
```

Run a quick benchmark:

```bash
head -c 10M </dev/urandom > randsrc.dat
cargo run --bin speed -- --entry-count 4000000
```

Run unit tests:

```bash
cargo test
```

## Getting started

We include a simple example in [`examples/v2_demo.rs`](qmdb/examples/v2_demo.rs) to create a QMDB instance and interact with the database. You can run it as follows:

```bash
cargo run --example v2_demo
```

## Directory Structure

- **`qmdb/src/`**: Main QMDB source code
  - **`examples/`**: Example projects demonstrating QMDB usage.
  - **`tests/`**: Unit tests.
  - **`entryfile/`**: Implements the `Entry` data structure
  - **`merkletree/`**: Contains `Twigs` (Merkle subtrees ordered by insertion time and not key)
  - **`indexer/`**: In-memory indexer to map keys to QMDB entries
  - **`indexer/hybrid/`**: Hybrid indexer that is optimized for SSD
  - **`stateless/`**: Build a in-memory subset of world state for stateless validation
  - **`seqads/`**: Sequential ADS, used to generate input data for stateless validation
  - **`tasks/`**: The Create/Update/Delete requests to QMDB must be encapsulated into ordered tasks
  - **`utils/`**: Miscellaneous utility and helper functions.

- **`bench/`**: Benchmarking utility.
- **`hpfile/`**: Head-prunable file: HPfile are a series of fixed-size files in QMDB that simulate a single large file, enabling efficient pruning from the front.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for more information on how to contribute to QMDB.

## Any questions?

[Please raise a GitHub issue](https://github.com/LayerZero-Labs/qmdb/issues/new).

## License

This project is dual licensed under the MIT License and the Apache License 2.0.

## Acknowledgements

If you use QMDB in a publication, please cite it as:

**QMDB: Quick Merkle Database**<br>
Isaac Zhang, Ryan Zarick, Daniel Wong, Thomas Kim, Bryan Pellegrino, Mignon Li, Kelvin Wong<br>
<https://arxiv.org/abs/2501.05262>

```bibtex
@article{zhang2025qmdb,
  title={Quick Merkle Database},
  author={Zhang, Isaac and Zarick, Ryan and Wong, Daniel and Kim, Thomas and Pellegrino, Bryan and Li, Mignon and Wong, Kelvin},
  journal={arXiv preprint arXiv:2501.05262},
  year={2025}
}
```

QMDB is a product of [LayerZero Labs](https://layerzero.network) Research.

<!-- markdownlint-disable MD033 -->
<p align="center">
  <a href="https://layerzero.network#gh-dark-mode-only">
    <img alt="LayerZero" style="width: 50%" src="https://github.com/LayerZero-Labs/devtools/raw/main/assets/logo-dark.svg#gh-dark-mode-only"/>
  </a>  
  <a href="https://layerzero.network#gh-light-mode-only">
    <img alt="LayerZero" style="width: 50%" src="https://github.com/LayerZero-Labs/devtools/raw/main/assets/logo-light.svg#gh-light-mode-only"/>
  </a>
</p>

<p align="center">
  <a href="https://layerzero.network" style="color: #a77dff">Homepage</a> | <a href="https://docs.layerzero.network/" style="color: #a77dff">Docs</a> | <a href="https://layerzero.network/developers" style="color: #a77dff">Developers</a>
</p>
