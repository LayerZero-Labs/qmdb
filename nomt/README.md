## NOMT: Nearly Optimal Merkle Trie

An implementation of a novel binary Merkle Trie and DB, written in Rust.

NOMT is an embedded key-value store that maintains a Merklized representation of key-value pairs with a simple key-value API, powering high throughput authenticated commits with billions of key-value pairs on relatively inexpensive hardware. It is largely designed for use in a blockchain setting as a drop-in replacement for RocksDB, MDBX, LevelDB, or ParityDB.

NOMT is optimized for fast random lookups of values, fast merkle tree updates, and fast writeout. It supports the generation of Merkle multiproofs for large batches of changes.

NOMT is designed to take advantage of hardware improvements in Solid State Drives (SSDs) using NVMe and Linux's io-uring API for asynchronous I/O. NOMT adequately supports generic Unix as well as macOS for daily development and testing, but primarily targets Linux for performance. The impressive trend in performance and capacity in modern SSDs enables us to build a DB that scales along with the hardware.

NOMT exposes a many-readers-one-writer API organized around batch transactions referred to as `Session`s. Predictable performance in a metered execution environment is a key goal of NOMT, and therefore only one `Session` may be live at a time.

## Project Structure

<pre>
NOMT: Project Root.
├──<a href="./benchtop">benchtop</a>: A benchmarking tool for NOMT.
|--<a href="./core">core</a>: Core logic, primarily for verifying and updating the NOMT.
|--<a href="./docs">docs</a>: Documentation
|--<a href="./fuzz">fuzz</a>: Fuzzing suite.
├──<a href="./examples">examples</a>: Various examples of using NOMT.
│   ├── <a href="./examples/commit_batch">commit_batch</a>: Demonstration of a simple commit.
│   ├── <a href="./examples/read_value">read_value</a>: Reading a value from the NOMT.
│   ├── <a href="./examples/witness_verification">witness_verification</a>: Demonstration of how to verify a witness in a light-client setting.
|--<a href="./nomt">nomt</a>: Implementation of the NOMT database.
|──<a href="./torture">torture</a>: Extensive testing suite for NOMT.
|--<a href="./trickfs">trickfs</a>: A FUSE filesystem aiding deeper testing. Experimental.
│   ├──<a href="./trickfs/trickmnt">trickmnt</a>: A tool that allows mounting trickfs.
</pre>

## Architecture

Internally, NOMT consists of two parallel stores, Beatree and Bitbox. Beatree stores raw key-value pairs and is based around a B-Tree variant optimized for stable, fast random access patterns and high-entropy keys. Bitbox stores a custom sparse binary merkle tree in an on-disk hashtable in a format amenable to SSDs.

For more information on NOMT, the thesis behind it, and performance targets, see [this November 2024 presentation](https://x.com/TheKusamarian/status/1855477208762261910) by @rphmeier or [view the slides here](https://hackmd.io/@Xo-wxO7bQkKidH1LrqACsw/rkG0lmjWyg#/).

We have built a benchmarking tool, `benchtop`, which is located in the `benchtop` directory as a separate subcrate.

## Contributing

See [CONTRIBUTING.md](docs/CONTRIBUTING.md).

If you would like to discuss the development of NOMT or follow along with contributor discussions, join the official [Telegram Channel](https://t.me/thrum_nomt).

## Acknowledgements

The development of this project is supported financially by [Sovereign Labs](https://www.sovereign.xyz/), creators of the [Sovereign SDK](https://github.com/Sovereign-Labs/sovereign-sdk/). The idea for this project originated in [this post by Preston Evans](https://sovereign.mirror.xyz/jfx_cJ_15saejG9ZuQWjnGnG-NfahbazQH98i1J3NN8).
