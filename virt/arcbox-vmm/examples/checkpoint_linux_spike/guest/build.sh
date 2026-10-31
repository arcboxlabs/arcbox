#!/usr/bin/env bash
# Builds the spike 3 initramfs (a gzipped newc cpio holding only the static
# /init from init.c) with the musl cross toolchain
# (brew install FiloSottile/musl-cross/musl-cross).
#
#   build.sh <out.cpio.gz>
set -euo pipefail
out=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
src=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/root"
aarch64-linux-musl-gcc -O2 -static -pthread -o "$tmp/root/init" "$src/init.c"
(cd "$tmp/root" && echo init | cpio -o -H newc --quiet) | gzip -9 >"$out"
ls -l "$out"
