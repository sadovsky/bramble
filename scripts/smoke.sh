#!/usr/bin/env bash
# Boot, capture the serial log and a framebuffer screenshot, then exit.
# This is how each phase's milestone is demonstrated without a human watching.
#
# Usage: scripts/smoke.sh [--cmdline "..."] [--wait-for "text"] [--timeout N]
set -euo pipefail
cd "$(dirname "$0")/.."

CMDLINE=""
WAIT_FOR="phase 0 complete"
TIMEOUT=45

while [ $# -gt 0 ]; do
    case "$1" in
        --cmdline) CMDLINE="$2"; shift 2 ;;
        --wait-for) WAIT_FOR="$2"; shift 2 ;;
        --timeout) TIMEOUT="$2"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

OVMF_CODE=/usr/share/OVMF/OVMF_CODE_4M.fd
mkdir -p build
[ -f build/OVMF_VARS.fd ] || cp /usr/share/OVMF/OVMF_VARS_4M.fd build/OVMF_VARS.fd

# Always restage the config, so a cmdline from an earlier run cannot linger.
CONF=build/iso_root/boot/limine/limine.conf
[ -f "$CONF" ] || { echo "run scripts/build-iso.sh first" >&2; exit 2; }
cp limine.conf "$CONF"
[ -n "$CMDLINE" ] && printf '    cmdline: %s\n' "$CMDLINE" >> "$CONF"
xorriso -as mkisofs -quiet -R -r -J \
    --efi-boot boot/limine/limine-uefi-cd.bin \
    -efi-boot-part --efi-boot-image --protective-msdos-label \
    build/iso_root -o build/bramble.iso

rm -f build/serial.log build/qmp.sock build/screen.ppm build/screen.png
ACCEL=()
[ -w /dev/kvm ] && ACCEL=(-enable-kvm -cpu host)

qemu-system-x86_64 \
    -machine q35 -m 512M "${ACCEL[@]}" \
    -drive if=pflash,unit=0,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,unit=1,format=raw,file=build/OVMF_VARS.fd \
    -cdrom build/bramble.iso -boot d \
    -serial file:build/serial.log -display none \
    -qmp unix:build/qmp.sock,server,nowait \
    -no-reboot -no-shutdown &
QEMU_PID=$!
trap 'kill $QEMU_PID 2>/dev/null || true' EXIT

for _ in $(seq $((TIMEOUT * 4))); do
    if [ -f build/serial.log ] && grep -qF "$WAIT_FOR" build/serial.log 2>/dev/null; then
        break
    fi
    kill -0 $QEMU_PID 2>/dev/null || break
    sleep 0.25
done

python3 scripts/screendump.py build/qmp.sock build/screen.ppm || true
kill $QEMU_PID 2>/dev/null || true
wait $QEMU_PID 2>/dev/null || true

if [ -f build/screen.ppm ]; then
    python3 -c "
from PIL import Image
Image.open('build/screen.ppm').save('build/screen.png')
print('build/screen.png written')
" || true
fi

# Decode whatever state left the machine and verify it offline. The kernel and
# the host tool check the same invariants from opposite sides, so a
# disagreement fails the build.
if grep -q -- "--- snapshot begin" build/serial.log 2>/dev/null; then
    python3 tools/graphdump.py build/serial.log --check --diff \
        --dot build/graph.dot --png build/graph.png || exit 1
elif grep -q -- "--- graph begin" build/serial.log 2>/dev/null; then
    python3 tools/graphdump.py build/serial.log --check \
        --dot build/graph.dot --png build/graph.png || exit 1
fi

echo "--- serial log ---"
cat build/serial.log 2>/dev/null || echo "(no serial output)"

if ! grep -qF "$WAIT_FOR" build/serial.log 2>/dev/null; then
    echo "SMOKE FAILED: never saw '$WAIT_FOR'" >&2
    exit 1
fi
echo "SMOKE OK"
