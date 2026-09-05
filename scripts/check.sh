#!/usr/bin/env bash
# Everything that must pass before a commit: host tests for the graph, clippy
# on both crates, a kernel build, and a boot in QEMU that reaches its milestone.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> graph tests (host)"
cargo ktest

echo "==> clippy"
cargo kclippy -- -D warnings
cargo clippy -p bramble-kernel -- -D warnings

echo "==> kernel build and boot image"
./scripts/build-iso.sh >/dev/null

echo "==> boot smoke test"
./scripts/smoke.sh --wait-for "${1:-phase 2 complete}" >/dev/null

echo "all checks passed"
