#!/usr/bin/env bash
# Puts a free homebrew test ROM at tests/roms/test.nds (gitignored).
#
# By default the ROM is generated here, offline: a minimal homebrew image (our own code, no
# third-party content) whose ARM9 and ARM7 binaries each spin in a loop. It boots with dust's
# direct boot (no BIOS or firmware dump needed); it has no Nintendo logo, so it can't boot
# through the firmware.
#
# Generated ROM layout (tests rely on it):
#   ARM9, entry 0x02000000: stores 0x5A as a word at 0x02001000 once (the `str` is at
#   0x02000008), then spins with `b .` at 0x0200000C.
#   ARM7, entry 0x02380000: spins with `b .`.
# The header's ROM version byte (0x01E) is the generator version; an existing generated ROM with
# an older version is regenerated.
#
# To use a different homebrew ROM instead, download it:
#   DUST_TEST_ROM_URL=https://example.org/homebrew.nds \
#   DUST_TEST_ROM_SHA256=<sha256> tools/fetch_test_rom.sh
#
# Usage: tools/fetch_test_rom.sh [--force]
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out_dir="$root/tests/roms"
out="$out_dir/test.nds"

generator_version=1

if [[ -f "$out" && "${1:-}" != "--force" ]]; then
    game_code="$(dd if="$out" bs=1 skip=$((0x00C)) count=4 status=none)"
    rom_version="$(od -An -tu1 -j $((0x01E)) -N 1 "$out" | tr -d ' ')"
    if [[ -n "${DUST_TEST_ROM_URL:-}" || "$game_code" != "DUST" \
        || "$rom_version" == "$generator_version" ]]; then
        echo "$out already exists (pass --force to replace it)"
        exit 0
    fi
    echo "$out was made by an older version of this script; regenerating it"
fi
mkdir -p "$out_dir"
tmp="$(mktemp "$out_dir/.test.nds.XXXXXX")"
trap 'rm -f "$tmp"' EXIT

if [[ -n "${DUST_TEST_ROM_URL:-}" ]]; then
    curl --fail --location --silent --show-error --output "$tmp" "$DUST_TEST_ROM_URL"
    if [[ -n "${DUST_TEST_ROM_SHA256:-}" ]]; then
        echo "$DUST_TEST_ROM_SHA256  $tmp" | sha256sum --check --quiet
    fi
    mv "$tmp" "$out"
    echo "downloaded $DUST_TEST_ROM_URL to $out"
    exit 0
fi

# Writes raw bytes (given as printf escapes) at a byte offset.
put() {
    printf "$2" | dd of="$tmp" bs=1 seek="$(($1))" conv=notrunc status=none
}

le32() {
    printf '\\x%02x\\x%02x\\x%02x\\x%02x' \
        $(($1 & 0xFF)) $((($1 >> 8) & 0xFF)) $((($1 >> 16) & 0xFF)) $((($1 >> 24) & 0xFF))
}

le16() {
    printf '\\x%02x\\x%02x' $(($1 & 0xFF)) $((($1 >> 8) & 0xFF))
}

rom_size=$((0x20000)) # 128 KiB, device capacity 0
arm9_offset=$((0x200))
arm9_addr=$((0x02000000))
arm7_offset=$((0x220))
arm7_addr=$((0x02380000))
# ARM `b .` (branch to self).
spin_loop=$((0xEAFFFFFE))
# ARM9 program: ldr r0, [pc, #8]; mov r1, #0x5A; str r1, [r0]; b .; .word 0x02001000
arm9_code=(0xE59F0008 0xE3A0105A 0xE5801000 "$spin_loop" 0x02001000)

head -c "$rom_size" /dev/zero > "$tmp"

put 0x000 'DUSTTEST'                    # Game title (12 bytes, NUL-padded)
put 0x00C 'DUST'                        # Game code
put 0x010 '00'                          # Maker code
put 0x01E "$(printf '\\x%02x' "$generator_version")" # ROM version
put 0x020 "$(le32 $arm9_offset)"        # ARM9 ROM offset
put 0x024 "$(le32 $arm9_addr)"          # ARM9 entry address
put 0x028 "$(le32 $arm9_addr)"          # ARM9 load address
put 0x02C "$(le32 $((${#arm9_code[@]} * 4)))" # ARM9 size
put 0x030 "$(le32 $arm7_offset)"        # ARM7 ROM offset
put 0x034 "$(le32 $arm7_addr)"          # ARM7 entry address
put 0x038 "$(le32 $arm7_addr)"          # ARM7 load address
put 0x03C "$(le32 4)"                   # ARM7 size
put 0x080 "$(le32 $((arm7_offset + 4)))" # Total used ROM size
put 0x084 "$(le32 0x4000)"              # Header size
for i in "${!arm9_code[@]}"; do
    put $((arm9_offset + i * 4)) "$(le32 $((arm9_code[i])))"
done
put "$arm7_offset" "$(le32 $spin_loop)"

# Header CRC-16 (MODBUS: reflected 0x8005, init 0xFFFF) over 0x000..0x15D, stored at 0x15E.
crc=$((0xFFFF))
for byte in $(od -An -v -tu1 -N $((0x15E)) "$tmp"); do
    crc=$((crc ^ byte))
    for _ in 1 2 3 4 5 6 7 8; do
        if ((crc & 1)); then
            crc=$(((crc >> 1) ^ 0xA001))
        else
            crc=$((crc >> 1))
        fi
    done
done
put 0x15E "$(le16 $crc)"

mv "$tmp" "$out"
echo "generated $out"
