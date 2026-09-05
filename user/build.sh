#!/usr/bin/env bash
# Build the user programs.
#
# They cannot be built with a bare `cargo build`: cargo *concatenates*
# `target.<triple>.rustflags` from every config file between the working
# directory and the filesystem root, so the kernel's linker script and code
# model from the repository root would be appended to these. An explicit
# RUSTFLAGS in the environment replaces the lot, which is the only clean way to
# opt out. Hence this script, and hence `user/` being a separate workspace.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

PROFILE="${PROFILE:-release}"
FLAGS=(--release)
[ "$PROFILE" = "debug" ] && FLAGS=()

RUSTFLAGS="-C relocation-model=static -C link-arg=-T$ROOT/user/linker.ld" \
    cargo build --manifest-path "$ROOT/user/Cargo.toml" \
    --target x86_64-unknown-none "${FLAGS[@]}" "$@"

echo "$ROOT/user/target/x86_64-unknown-none/$PROFILE"
