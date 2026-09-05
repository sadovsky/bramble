#!/usr/bin/env bash
# Build the kernel and wrap it in a UEFI-bootable ISO with Limine.
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${PROFILE:-debug}"
CARGO_FLAGS=()
[ "$PROFILE" = "release" ] && CARGO_FLAGS+=(--release)

LIMINE_DIR=.limine
if [ ! -f "$LIMINE_DIR/BOOTX64.EFI" ]; then
    echo "fetching limine v9.x binaries (matching the limine crate's protocol revision)"
    rm -rf "$LIMINE_DIR"
    git clone -q --depth=1 --branch=v9.x-binary \
        https://github.com/limine-bootloader/limine.git "$LIMINE_DIR"
fi

cargo build -p bramble-kernel "${CARGO_FLAGS[@]}"
KERNEL="target/x86_64-unknown-none/$PROFILE/bramble"

# User programs are always built optimised: they are test subjects, not the
# thing under test, and a debug build of one is several megabytes of ELF.
PROFILE=release ./user/build.sh >/dev/null
USER_DIR="user/target/x86_64-unknown-none/release"

# The whole graph is meant to live in .bss: Graph::EMPTY is all zeroes, which
# is what lets it exist before the allocator does (DESIGN 3.7). A single
# non-zero field in any node body's ZERO silently moves 400+ KiB into .data and
# into the image, so check rather than trust.
DATA_BYTES=$(llvm-size --format=sysv "$KERNEL" | awk '$1 == ".data" { print $2 }')
BSS_BYTES=$(llvm-size --format=sysv "$KERNEL" | awk '$1 == ".bss" { print $2 }')
if [ "${DATA_BYTES:-0}" -gt 65536 ]; then
    echo "error: .data is ${DATA_BYTES} bytes; something that should be zero is not." >&2
    echo "       the graph arenas belong in .bss (currently ${BSS_BYTES} bytes)." >&2
    exit 1
fi
echo "sections: .data ${DATA_BYTES} B, .bss ${BSS_BYTES} B"

ISO_ROOT=build/iso_root
rm -rf "$ISO_ROOT" && mkdir -p "$ISO_ROOT/boot/limine" "$ISO_ROOT/EFI/BOOT"
cp "$KERNEL" "$ISO_ROOT/boot/bramble"
cp limine.conf "$ISO_ROOT/boot/limine/"
for program in hello faulter pinger ponger init worker v1; do
    cp "$USER_DIR/$program" "$ISO_ROOT/boot/$program"
done
cp "$LIMINE_DIR/limine-uefi-cd.bin" "$ISO_ROOT/boot/limine/"
cp "$LIMINE_DIR/BOOTX64.EFI" "$ISO_ROOT/EFI/BOOT/"

# Any extra arguments are boot modules, staged for later phases.
for module in "$@"; do
    cp "$module" "$ISO_ROOT/boot/$(basename "$module")"
done

mkdir -p build
xorriso -as mkisofs -quiet -R -r -J \
    --efi-boot boot/limine/limine-uefi-cd.bin \
    -efi-boot-part --efi-boot-image --protective-msdos-label \
    "$ISO_ROOT" -o build/bramble.iso

echo "build/bramble.iso  ($(du -h build/bramble.iso | cut -f1))"
