#!/usr/bin/env bash
# QEMU TCG backend probes on a Mac: devices and network, the QMP lifecycle,
# orphan recovery, TCG speed and guest memory. See README.md and
# docs/logs/2026-11-02-qemu-tcg-host-backend.md.
#
#   ARCBOX_QEMU_PATH=<qemu-system-aarch64> run.sh [work-dir]
#
# Boot assets and the docker runtime come from the newest version under
# QEMU_PROBE_DATA (default ~/.arcbox), the data directory of a daemon that
# has prepared the System VM once.
set -euo pipefail
: "${ARCBOX_QEMU_PATH:?set ARCBOX_QEMU_PATH to qemu-system-aarch64}"
root=$(cd "$(dirname "$0")/../../.." && pwd)
work=${1:-$(mktemp -d)}
mkdir -p "$work"
work=$(cd "$work" && pwd)
cd "$root"

data=${QEMU_PROBE_DATA:-$HOME/.arcbox}
boot=$(ls -d "$data"/boot/*/ | sort -V | tail -1)
export QEMU_PROBE_KERNEL=${QEMU_PROBE_KERNEL:-${boot}kernel}
export QEMU_PROBE_ROOTFS=${QEMU_PROBE_ROOTFS:-${boot}rootfs.erofs}
runtime=$(ls -d "$data"/runtime/*/bin | sort -V | tail -1)
guest=virt/arcbox-vmm/examples/qemu_probe/guest
"$guest/build.sh" "$work/initramfs.cpio.gz" "$runtime"
clang -O2 -o "$work/bench-native" "$guest/bench.c"
cargo build -p arcbox-vmm --example qemu_probe >&2
bin=target/debug/examples/qemu_probe

section() { printf '\n== %s\n' "$*"; }
fresh() {
  rm -f "$work/docker.img" "$work/docker-meta.img"
  truncate -s 64G "$work/docker.img"
  truncate -s 2G "$work/docker-meta.img"
}
# Waits for a line in the running probe's output; fails once it exits.
wait_for() {
  until grep -q "$1" "$work/out"; do
    kill -0 "$2" 2>/dev/null || return 1
    sleep 0.2
  done
}
probe() { "$bin" "$@" 2>&1 | grep -E 'PROBE|QEMU guest shut down|error'; }
free_mib() {
  vm_stat | awk '/page size of/ {ps=$8} /Pages free/ {f=$3} /Pages speculative/ {s=$3}
    END {printf "%d", (f + s) * ps / 1048576}'
}

section "devices, network and consoles; docker.img blocks around the discard"
fresh
"$bin" probe "$work" >"$work/out" 2>&1 &
wait_for "wrote 64 MiB" $!; echo "blocks after the write: $(stat -f %b "$work/docker.img")"
wait_for "discarded them" $!; echo "blocks after the discard: $(stat -f %b "$work/docker.img")"
wait
grep -E 'PROBE|QEMU guest shut down' "$work/out"

section "guest reset, in-place reboot"
probe reset "$work"
section "host stop of a running guest (QMP quit)"
probe stop "$work"

section "orphan: kill -9 the owner, then reap"
"$bin" orphan "$work" >"$work/out" 2>&1 &
owner=$!
wait_for "idles; kill -9" "$owner"
kill -9 "$owner"; wait "$owner" 2>/dev/null || true
echo "left running: $(pgrep -f qemu-system-aarch64 | tr '\n' ' ') pidfiles: $(ls "$work/run")"
probe reap "$work"
echo "after reap: $(pgrep -f qemu-system-aarch64 || echo none) pidfiles: $(ls "$work/run")"

section "speed: native, then TCG at 4 and 12 vCPUs"
"$work/bench-native"
probe bench "$work" 4096 4
probe bench "$work" 4096 12

section "dockerd readiness by guest memory (4 vCPUs)"
for mib in 2048 4096 8192 14336; do
  echo "-- ${mib} MiB"; probe docker "$work" "$mib" 4
done

section "host memory while the guest touches and frees 3 GiB"
echo "free before: $(free_mib) MiB"
"$bin" touch "$work" 8192 4 >"$work/out" 2>&1 &
while kill -0 $! 2>/dev/null; do
  echo "free $(free_mib) MiB $(grep -oE 'PROBE: (touching|touched|freed)' "$work/out" | tail -1)"
  sleep 1
done
echo "free after QEMU exits: $(free_mib) MiB"
