#!/usr/bin/env bash
# Rebuilds ../guest.bin from start.S and guest.c with the musl cross
# toolchain (brew install FiloSottile/musl-cross/musl-cross). The MMU stays
# off in the guest, so all memory is Device memory: no unaligned accesses
# (-mstrict-align), no FP/SIMD, and no compiler-generated memset/memcpy.
set -euo pipefail
cd "$(dirname "$0")"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cc=aarch64-linux-musl-gcc
flags=(-O2 -ffreestanding -nostdlib -fno-builtin -fno-stack-protector -fno-pic
       -mgeneral-regs-only -mstrict-align -fno-tree-loop-distribute-patterns)
$cc "${flags[@]}" -c start.S -o "$tmp/start.o"
$cc "${flags[@]}" -c guest.c -o "$tmp/guest.o"
$cc "${flags[@]}" -static -Wl,-T,link.ld -Wl,--build-id=none "$tmp/start.o" "$tmp/guest.o" -o "$tmp/guest.elf"
aarch64-linux-musl-objcopy -O binary "$tmp/guest.elf" ../guest.bin
ls -l ../guest.bin
