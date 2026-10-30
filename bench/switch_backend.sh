#!/bin/bash
# switch_backend.sh — toggle bench/Cargo.toml (and the workspace Cargo.lock)
# between LVMT-mode and Reth-mode.
#
# Why: cfx-kvdb-rocksdb (LVMT dep) declares zstd-sys ^1 and links="zstd";
# reth-primitives declares zstd-sys ^2 and links="zstd". Cargo refuses to
# resolve both in the same workspace even when one is feature-gated, so we
# physically swap Cargo.toml between the two configurations.
#
# Usage:
#   ./switch_backend.sh lvmt     # qmdb / nomt / lvmt builds (default)
#   ./switch_backend.sh reth     # reth builds
#   ./switch_backend.sh status   # show which mode is active
set -euo pipefail

DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$DIR"

for f in Cargo.toml.lvmt Cargo.toml.reth Cargo.lock.lvmt Cargo.lock.reth; do
    [[ -f $f ]] || { echo "❌ missing $f"; exit 1; }
done

current_mode() {
    if grep -q "^lvmt-db = " Cargo.toml 2>/dev/null; then
        echo lvmt
    elif grep -q "^reth-db = " Cargo.toml 2>/dev/null; then
        echo reth
    else
        echo unknown
    fi
}

case "${1:-status}" in
    lvmt|qmdb|nomt)
        cp Cargo.toml.lvmt Cargo.toml
        cp Cargo.lock.lvmt ../Cargo.lock
        echo "✅ Cargo.toml → LVMT mode (use_lvmt active; use_reth empty)"
        ;;
    reth)
        cp Cargo.toml.reth Cargo.toml
        cp Cargo.lock.reth ../Cargo.lock
        echo "✅ Cargo.toml → Reth mode (use_reth active; use_lvmt empty)"
        ;;
    status)
        echo "current backend mode: $(current_mode)"
        ;;
    *)
        echo "usage: $0 {lvmt|reth|status}"
        exit 1
        ;;
esac
