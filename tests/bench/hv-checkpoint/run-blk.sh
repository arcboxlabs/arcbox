#!/usr/bin/env bash
# HV checkpoint spike 2 matrix: multi-vCPU VM with a virtio-blk device
# under load. Builds and signs the probe, then runs the baseline, the
# negative controls, two clones of one checkpoint, repeated save/restore
# cycles and a vCPU-count sweep. See README.md and
# docs/experiments/2026-10-31-hv-checkpoint-blk-spike.md.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
work=${1:-$(mktemp -d)}
cd "$root"

cargo build --release -p arcbox-vmm --example checkpoint_blk_spike >&2
bin=target/release/examples/checkpoint_blk_spike
codesign --force --entitlements bundle/arcbox.dev.entitlements -s - "$bin" 2>/dev/null

section() { printf '\n== %s\n' "$*"; }
expect_fail() {
  local status=0
  "$@" >/dev/null 2>"$work/err" || status=$?
  if [ "$status" -eq 0 ]; then
    echo "UNEXPECTED SUCCESS"
  elif [ "$status" -gt 128 ]; then
    echo "process killed by signal $((status - 128))"
  else
    cut -c1-160 "$work/err"
  fi
}

section "baseline: 4 vCPUs, save, restore lazily, restore eagerly"
"$bin" save "$work/base" 300
"$bin" restore "$work/base" 500
"$bin" restore "$work/base" 500 --eager

section "negative controls (each is expected to fail)"
for part in mmio disk gic icc; do
  printf -- '-- skip %s: ' "$part"
  SPIKE_SKIP=$part expect_fail "$bin" restore "$work/base" 300
done
printf -- '-- GIC read after the vCPUs are destroyed: '
SPIKE_LATE_GIC=1 "$bin" save "$work/late" 200 >/dev/null
expect_fail "$bin" restore "$work/late" 300

section "two concurrent clones of one checkpoint"
before=$(shasum -a 256 "$work/base/mem.bin" "$work/base/disk.img" | cut -d' ' -f1)
"$bin" restore "$work/base" 500 >"$work/clone1" 2>&1 &
"$bin" restore "$work/base" 500 >"$work/clone2" 2>&1 &
wait
cat "$work/clone1" "$work/clone2"
after=$(shasum -a 256 "$work/base/mem.bin" "$work/base/disk.img" | cut -d' ' -f1)
[ "$before" = "$after" ] && echo "mem.bin and disk.img unchanged by both clones" || echo "checkpoint CHANGED"

section "20 save/restore cycles"
fail=0
for i in $(seq 20); do
  rm -rf "$work/cycle"
  "$bin" save "$work/cycle" 100 >"$work/save.out"
  sed -n 's/.*in flight at vCPU stop \(\[[0-9, ]*\]\).*/\1/p' "$work/save.out" >>"$work/inflight"
  if "$bin" restore "$work/cycle" 100 >"$work/restore.out" 2>&1; then
    sed -n 's/.*an I\/O on every vCPU \([0-9.]*\) ms.*/\1/p' "$work/restore.out" >>"$work/lat"
  else
    fail=$((fail + 1))
    cat "$work/restore.out"
  fi
done
echo "failed restores: $fail of 20"
awk -F'[][, ]+' '{for (i = 2; i < NF; i++) { n++; if ($i > 0) k++ }} END {printf "queues with a request in flight at the pause: %d of %d\n", k, n}' "$work/inflight"
sort -n "$work/lat" | awk '{a[NR]=$1} END {printf "an I/O on every vCPU after restore, ms: min %s p50 %s p90 %s max %s\n", a[1], a[int(NR/2)], a[int(NR*0.9)], a[NR]}'

section "vCPU count sweep"
for n in 1 2 8 12; do
  echo "-- $n vCPUs"
  SPIKE_VCPUS=$n "$bin" save "$work/n$n" 200 | grep -E "saved|pause"
  "$bin" restore "$work/n$n" 300 | grep -E "built|per vCPU"
done
