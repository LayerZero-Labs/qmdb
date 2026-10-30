# Backend switching (LVMT ↔ Reth)

## Why this exists

`cfx-kvdb-rocksdb` (LVMT's backend) and `reth-primitives` both declare
`links = "zstd"` in their Cargo metadata, but pin to incompatible major
versions of `zstd-sys` (^1 vs ^2). Cargo resolves `links = ` at workspace
level **before** evaluating features, so even if both are marked `optional`,
having both as path deps in `bench/Cargo.toml` makes resolution fail.

The clean workaround is to keep two `Cargo.toml` snapshots and physically
swap them before `cargo build`.

## Files

| File | Mode | LVMT deps | Reth deps |
|---|---|---|---|
| `Cargo.toml.lvmt` | default | active | commented out |
| `Cargo.toml.reth` | reth | commented out | active |
| `Cargo.toml` | live (overwritten by switch) | — | — |
| `Cargo.lock.lvmt`, `Cargo.lock.reth` | lock file of each mode, copied to the workspace `Cargo.lock` by the switch | — | — |
| `switch_backend.sh` | toggle | — | — |

## Usage

```sh
./switch_backend.sh lvmt    # for use_qmdb / use_nomt / use_lvmt
./switch_backend.sh reth    # for use_reth
./switch_backend.sh status  # which mode is active right now
```

Run it before `cargo build`. QMDB and NOMT builds work in lvmt-mode (the
default): LVMT deps are gated by `optional = true`, so they are not pulled in
for qmdb/nomt builds.

## Drift check

When updating `Cargo.toml.lvmt` (e.g. bumping a workspace dep), apply the
same change to `Cargo.toml.reth` and verify with:

```sh
diff Cargo.toml.lvmt Cargo.toml.reth     # should only differ in the LVMT/Reth blocks
```

## If reth build still fails on zstd

The `reth-primitives` default features include `reth-codec`, which pulls
`zstd` (single-source, so no conflict in reth-mode). If a future Reth update
adds another zstd path or an indirect conflict appears, edit
`Cargo.toml.reth` to disable default features and pick only what
`reth_adapter.rs` actually needs:

```toml
reth-primitives = { path = "...", optional = true, default-features = false, features = ["std", "alloy-compat", "c-kzg", "secp256k1"] }
```
