#!/usr/bin/env bash
# HV checkpoint spike 3 matrix: a Linux guest checkpointed by the production
# HV VMM in the middle of block I/O. Builds the initramfs and the probe,
# signs it, then runs the baseline, the negative controls, two clones of one
# checkpoint, repeated save/restore cycles, a vCPU-count sweep and a
# checkpoint with CPUs powered off. See README.md and
# docs/experiments/2026-10-31-hv-checkpoint-linux-spike.md.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
work=${1:-$(mktemp -d)}
mkdir -p "$work"
work=$(cd "$work" && pwd)
cd "$root"

virt/arcbox-vmm/examples/checkpoint_linux_spike/guest/build.sh "$work/initramfs.cpio.gz" >&2
cargo build --release -p arcbox-vmm --example checkpoint_linux_spike >&2
bin=target/release/examples/checkpoint_linux_spike
codesign --force --entitlements bundle/arcbox.dev.entitlements -s - "$bin" 2>/dev/null

section() { printf '\n== %s\n' "$*"; }
expect_fail() {
  local status=0
  "$@" >"$work/out" 2>&1 || status=$?
  if [ "$status" -eq 0 ]; then
    echo "UNEXPECTED SUCCESS"
  elif [ "$status" -gt 128 ]; then
    echo "process killed by signal $((status - 128))"
  else
    grep -m2 -E '^error|FAIL|Oops' "$work/out" | cut -c1-160 | tr '\n' ' '
    echo
  fi
}
# Edits one part of a copy of a checkpoint's state.json.
tamper() {
  python3 -I -c '
import json, sys
path, what = sys.argv[1], sys.argv[2]
s = json.load(open(path))
for v in filter(None, s["vcpus"]):
    if what == "timer": v["vtimer_offset"] = 0
    if what == "icc": v["icc"] = [0] * len(v["icc"])
if what == "devices": s["devices"] = []
json.dump(s, open(path, "w"))' "$@"
}

section "baseline: 4 vCPUs, 5 s of I/O, save, restore"
"$bin" save "$work/base" 5 | grep '^\['
"$bin" restore "$work/base" 5 | grep '^\['

section "negative controls (each is expected to fail)"
"$bin" save "$work/other" 3 >/dev/null
SPIKE_APPEND="maxcpus=2" "$bin" save "$work/off2" 3 >/dev/null
for part in devices timer icc gic-empty gic-swap gic-other mem disk; do
  printf -- '-- %s: ' "$part"
  rm -rf "$work/neg"
  cp -c -R "$work/base" "$work/neg"
  case $part in
    gic-empty) : >"$work/neg/gic.bin" ;;
    gic-swap) cp "$work/other/gic.bin" "$work/neg/gic.bin" ;; # same config, other moment
    gic-other) cp "$work/off2/gic.bin" "$work/neg/gic.bin" ;; # CPUs 2-3 never enabled
    mem) cp -c "$work/other/mem.bin" "$work/neg/mem.bin" ;;
    disk) ;;
    *) tamper "$work/neg/state.json" "$part" ;;
  esac
  if [ "$part" = disk ]; then
    SPIKE_TAMPER=disk expect_fail "$bin" restore "$work/neg" 4
  else
    expect_fail "$bin" restore "$work/neg" 4
  fi
done
echo "(gic-swap: base and other differ in $(cmp -l "$work/base/gic.bin" "$work/other/gic.bin" | wc -l | tr -d ' ') byte(s) of GIC state)"

section "two concurrent clones of one checkpoint"
before=$(shasum -a 256 "$work/base/mem.bin" "$work/base/disk0.img" | cut -d' ' -f1)
"$bin" restore "$work/base" 5 >"$work/clone1" 2>&1 &
"$bin" restore "$work/base" 5 >"$work/clone2" 2>&1 &
wait
grep -h -E '^\[restore\] (OK|.*ticks)|^error' "$work/clone1" "$work/clone2"
after=$(shasum -a 256 "$work/base/mem.bin" "$work/base/disk0.img" | cut -d' ' -f1)
[ "$before" = "$after" ] && echo "mem.bin and disk0.img unchanged by both clones" || echo "checkpoint CHANGED"

section "20 save/restore cycles at random points"
fail=0
: >"$work/lat"
for i in $(seq 20); do
  rm -rf "$work/cycle"
  "$bin" save "$work/cycle" 2 >"$work/save.out" 2>&1
  sed -n 's/.*\(pause [0-9.]* ms\).*total \([0-9]*\) ms/\1 total \2 ms/p' "$work/save.out" >>"$work/pauses"
  if "$bin" restore "$work/cycle" 3 >"$work/restore.out" 2>&1; then
    sed -n 's/.*hv_restore returned in \([0-9.]*\) ms.*/\1/p' "$work/restore.out" >>"$work/lat"
  else
    fail=$((fail + 1))
    grep -E '^error' "$work/restore.out"
  fi
done
echo "failed restores: $fail of 20"
sort -n "$work/lat" | awk '{a[NR]=$1} END {printf "hv_restore, ms: min %s p50 %s max %s\n", a[1], a[int((NR+1)/2)], a[NR]}'
awk '{print $2}' "$work/pauses" | sort -n | awk '{a[NR]=$1} END {printf "vCPU pause, ms: min %s p50 %s max %s\n", a[1], a[int((NR+1)/2)], a[NR]}'

section "vCPU count sweep"
for n in 1 2 8; do
  printf -- '-- %s vCPUs: ' "$n"
  SPIKE_VCPUS=$n "$bin" save "$work/n$n" 3 | grep -o 'pause.*total [0-9]* ms' | tr '\n' ' '
  "$bin" restore "$work/n$n" 4 | tail -1
done

section "checkpoint with CPUs 2-3 off, onlined after the restore"
SPIKE_APPEND="maxcpus=2 spike_online_at=7" "$bin" save "$work/cpuoff" 3 | grep -o '[0-9]/4 vCPUs on'
"$bin" restore "$work/cpuoff" 6 | grep -E 'guest\| online|^\[restore\] (OK|.*ticks)'
