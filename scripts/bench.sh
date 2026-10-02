#!/usr/bin/env bash
# Compares tinyhttp with a tiny_http-based server on the same machine with the same load generator.
# Usage: scripts/bench.sh   (about 3 minutes)
set -euo pipefail
export MSYS_NO_PATHCONV=1
cd "$(dirname "$0")/.."
cargo build --release --examples >/dev/null 2>&1
cargo build --release >/dev/null 2>&1
T=target/release
THREADS=32
SECONDS_PER_RUN=5
RUNS=3

run() { # name, port, path, clients
  for i in $(seq $RUNS); do
    printf '%-10s %-14s ' "$1" "$3 x$4"
    $T/examples/loadgen.exe 127.0.0.1:$2 "$3" --clients "$4" --seconds $SECONDS_PER_RUN
  done
}

$T/tinyhttp.exe . --demo -p 18100 -q --threads $THREADS --max-requests 0 >/dev/null & A=$!
$T/examples/baseline.exe 18101 --threads $THREADS >/dev/null & B=$!
$T/examples/baseline.exe 18102 --big --threads $THREADS >/dev/null & C=$!
sleep 1
trap 'kill $A $B $C 2>/dev/null || true' EXIT

for clients in 1 8 32; do
  run tinyhttp 18100 /_/hello/world $clients
  run tiny_http 18101 /_/hello/world $clients
done
for clients in 1 8; do
  run tinyhttp 18100 "/_/big?mib=1" $clients
  run tiny_http 18102 /big $clients
done
