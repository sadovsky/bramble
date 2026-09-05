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

WAIT_FOR="${1:-phase 4 complete}"

echo "==> debug kernel, boot smoke test"
./scripts/build-iso.sh >/dev/null
./scripts/smoke.sh --wait-for "$WAIT_FOR" >/dev/null

# The performance gates are only enforced with the optimiser on: unoptimised,
# a ratio between a layered abstraction and a three-line control measures the
# optimiser rather than the design.
echo "==> release kernel, boot smoke test (performance gates enforced)"
PROFILE=release ./scripts/build-iso.sh >/dev/null
./scripts/smoke.sh --wait-for "$WAIT_FOR" >/dev/null

echo "all checks passed"
