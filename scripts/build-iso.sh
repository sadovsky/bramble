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

ISO_ROOT=build/iso_root
rm -rf "$ISO_ROOT" && mkdir -p "$ISO_ROOT/boot/limine" "$ISO_ROOT/EFI/BOOT"
cp "$KERNEL" "$ISO_ROOT/boot/bramble"
cp limine.conf "$ISO_ROOT/boot/limine/"
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
