#!/usr/bin/env bash
# HV checkpoint spike matrix: builds and signs the probe, then runs the
# baseline, the negative controls, two clones of one checkpoint, and the
# size/latency sweep. See README.md and
# docs/experiments/2026-10-31-hv-checkpoint-spike.md.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
work=${1:-$(mktemp -d)}
cd "$root"

cargo build --release -p arcbox-vmm --example checkpoint_spike >&2
bin=target/release/examples/checkpoint_spike
codesign --force --entitlements bundle/arcbox.dev.entitlements -s - "$bin" 2>/dev/null

section() { printf '\n== %s\n' "$*"; }

section "baseline: save, restore lazily, restore eagerly"
"$bin" save "$work/base" 300
"$bin" restore "$work/base" 500
"$bin" restore "$work/base" 500 --eager

section "negative controls (each is expected to fail)"
for part in gic icc simd sysfirst; do
  printf -- '-- skip %s: ' "$part"
  if SPIKE_SKIP=$part "$bin" restore "$work/base" 300 >/dev/null 2>"$work/err"; then
    echo "UNEXPECTED SUCCESS"
  else
    cat "$work/err"
  fi
done

section "guest time across a 2 s gap between save and restore"
"$bin" save "$work/gap" 200 >/dev/null
sleep 2
echo "-- vtimer offset restored:"
"$bin" restore "$work/gap" 200 | grep -E "guest time|tick rate"
echo "-- vtimer offset left at the new VM's default:"
SPIKE_SKIP=vtimer "$bin" restore "$work/gap" 200 | grep -E "ticks|guest time|tick rate"

section "two concurrent clones of one checkpoint"
before=$(shasum -a 256 "$work/base/mem.bin" | cut -d' ' -f1)
"$bin" restore "$work/base" 500 >"$work/clone1" 2>&1 &
"$bin" restore "$work/base" 500 >"$work/clone2" 2>&1 &
wait
cat "$work/clone1" "$work/clone2"
after=$(shasum -a 256 "$work/base/mem.bin" | cut -d' ' -f1)
[ "$before" = "$after" ] && echo "mem.bin unchanged by both clones" || echo "mem.bin CHANGED"

section "size sweep: 1 GiB guest RAM, dirty MiB varied"
for fill in 0 64 256 1000; do
  echo "-- dirty ${fill} MiB"
  SPIKE_RAM_MB=1024 SPIKE_FILL_MB=$fill "$bin" save "$work/fill$fill" 200
  "$bin" restore "$work/fill$fill" 200
  "$bin" restore "$work/fill$fill" 200 --eager | grep -E "restored|memory mapped|RSS"
  rm -rf "$work/fill$fill"
done

section "restore latency, 20 runs (1 GiB RAM, 256 MiB dirty)"
SPIKE_RAM_MB=1024 SPIKE_FILL_MB=256 "$bin" save "$work/lat" 200 >/dev/null
for _ in $(seq 20); do
  "$bin" restore "$work/lat" 50 | sed -n 's/.*first tick \([0-9.]*\) ms.*/\1/p'
done | sort -n | awk '{a[NR]=$1} END {printf "first tick ms: min %s p50 %s p90 %s max %s\n", a[1], a[int(NR/2)], a[int(NR*0.9)], a[NR]}'
