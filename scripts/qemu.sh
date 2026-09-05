#!/usr/bin/env bash
# Boot the ISO under QEMU with UEFI firmware. Serial goes to stdout.
#
# There is no KVM in the usual development container, so this runs under TCG.
# Timings measured that way are only meaningful as ratios (DESIGN Q8).
set -euo pipefail
cd "$(dirname "$0")/.."

OVMF_CODE=/usr/share/OVMF/OVMF_CODE_4M.fd
OVMF_VARS_SRC=/usr/share/OVMF/OVMF_VARS_4M.fd
mkdir -p build
[ -f build/OVMF_VARS.fd ] || cp "$OVMF_VARS_SRC" build/OVMF_VARS.fd

ACCEL=()
[ -w /dev/kvm ] && ACCEL=(-enable-kvm -cpu host)

exec qemu-system-x86_64 \
    -machine q35 -m 512M "${ACCEL[@]}" \
    -drive if=pflash,unit=0,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,unit=1,format=raw,file=build/OVMF_VARS.fd \
    -cdrom build/bramble.iso -boot d \
    -serial stdio -display none \
    -no-reboot -no-shutdown "$@"
