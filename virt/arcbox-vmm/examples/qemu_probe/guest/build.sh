#!/usr/bin/env bash
# Builds the probe initramfs: Debian's static busybox, /init, the DHCP hook,
# the benchmark (built with brew's FiloSottile/musl-cross/musl-cross) and,
# when a directory holding them is given, dockerd and its runtime
# (`~/.arcbox/runtime/<version>/bin`).
#
#   build.sh <out.cpio.gz> [docker-bin-dir]
set -euo pipefail
out=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
src=$(cd "$(dirname "$0")" && pwd)
deb=busybox-static_1.37.0-6+b9_arm64.deb
sha=c833be48abfa16bc19c4966ec93e289ff1ce5d2f1476cad3a57bd105378cd15c
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/$deb" "https://deb.debian.org/debian/pool/main/b/busybox/$deb"
echo "$sha  $tmp/$deb" | shasum -a 256 -c - >/dev/null
(cd "$tmp" && ar x "$deb" data.tar.xz && tar -xf data.tar.xz ./usr/bin/busybox)
aarch64-linux-musl-gcc -O2 -static -o "$tmp/bench" "$src/bench.c"
python3 -I "$src/mkcpio.py" "$out" "$tmp/usr/bin/busybox" "$src/init" \
    "$src/udhcpc.sh" "$tmp/bench" ${2:+"$2"}
