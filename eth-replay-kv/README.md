# eth-replay-kv

Tools for replaying and analyzing per-block KV change-sets.

## Binaries

### `eth-replay-kv`

Replays per-block KV changes over a base KV snapshot range.

```bash
cargo run --release -- 15012300 15012400 ./kv/
```

Positional args:

- `start_height`: first block height (inclusive)
- `end_height`: last block height (inclusive)
- `input_dir`: directory containing input `.bin` files

### `kv-op-counts`

Counts how many `Create` / `Read` / `Update` / `Delete` operations appear in `{height}_operation_kv.bin` for a single height or a height range.

```bash
cargo run --release --bin kv-op-counts -- 15012300 15012400 ./kv/ --per-block
```

S3 input works too (uses your normal AWS credential/region resolution via the AWS SDK):

```bash
cargo run --release --bin kv-op-counts -- 15012300 15012400 s3://my-bucket/some/prefix --per-block
```

To count a single block, pass the same height twice:

```bash
cargo run --release --bin kv-op-counts -- 15012300 15012300 ./kv/
```

## Input directory format

For each height `H` in `[start_height, end_height]`, the tools look for:

- `input_dir/{H}_base_kv.bin` (base KV snapshot per block; missing files are skipped)
- `input_dir/{H}_operation_kv.bin` (Create/Read/Update/Delete ops per block; missing files are skipped)
- `input_dir/{H}_change_kv.bin` (optional; CUD-only ops per block, used by e.g. `replay_kv_simulation` in cyclone)

All files are expected to be **bincode**-encoded.

