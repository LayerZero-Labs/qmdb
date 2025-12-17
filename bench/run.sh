#!/bin/bash
QMDB_DIR=${1:-/tmp/QMDB}
ENTRY_COUNT=${2:-7000000000}
# Create randsrc if it doesn't exist
if [ ! -f randsrc.dat ]; then
    head -c 10M </dev/urandom > randsrc.dat
fi

if [ ! -d $QMDB_DIR ]; then
    mkdir -p $QMDB_DIR
fi

ulimit -n 65535

if command -v gtime &> /dev/null; then
    TIME_CMD="/usr/bin/time --verbose"
else
    TIME_CMD="time"
fi

$TIME_CMD cargo run --bin speed --release -- \
    --db-dir $QMDB_DIR \
    --entry-count $ENTRY_COUNT \
    --ops-per-block 1000000 \
    --hover-recreate-block 100 \
    --hover-write-block 100 \
    --hover-interval 1000 \
    --output-filename qmdb_benchmark.json \
    --tps-blocks 500 2>&1
