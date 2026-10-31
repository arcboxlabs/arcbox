#!/usr/bin/env bash
# HV checkpoint spikes 4 and 5 matrix: the spike 3 Linux guest with
# virtio-net and virtio-vsock carrying live connections across the
# checkpoint: a TCP echo client, an idle TCP client blocked in recv, a TCP
# download and a vsock echo server. Builds the
# initramfs and the probe, signs it, then runs the baseline, repeated
# save/restore cycles, a vCPU-count sweep and the spike 3 block-only mode as
# a regression check. The code-level negative controls are manual; see
# docs/experiments/2026-10-31-hv-checkpoint-net-vsock-spike.md and
# docs/experiments/2026-10-31-hv-checkpoint-tcp-reset-spike.md.
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
export SPIKE_NET=1

section() { printf '\n== %s\n' "$*"; }

section "baseline: 4 vCPUs, block + TCP + vsock traffic, save, restore"
"$bin" save "$work/base" 5 | grep '^\['
"$bin" restore "$work/base" 5 | grep '^\['

section "20 save/restore cycles at random points"
fail=0
: >"$work/lat"
: >"$work/save"
for i in $(seq 20); do
  rm -rf "$work/cycle"
  "$bin" save "$work/cycle" 2 >"$work/save.out" 2>&1
  grep -o '[0-9]* TCP flows; pause.*total [0-9]* ms' "$work/save.out" >>"$work/save"
  if "$bin" restore "$work/cycle" 3 >"$work/restore.out" 2>&1; then
    {
      sed -n 's/.*hv_restore returned in \([0-9.]*\) ms.*/\1/p' "$work/restore.out"
      sed -n 's/.*first reconnect Some(\([0-9]*\).*first echo Some(\([0-9]*\).*/\1 \2/p' "$work/restore.out"
      sed -n 's/.*idle: 1 ended, reconnect Some(\([0-9]*\).*download.*reconnect Some(\([0-9]*\).*/\1 \2/p' "$work/restore.out"
    } | tr '\n' ' ' >>"$work/lat"
    echo >>"$work/lat"
  else
    fail=$((fail + 1))
    grep -E '^error' "$work/restore.out"
  fi
done
echo "failed restores: $fail of 20"
stat() { sort -n | awk -v name="$1" '{a[NR]=$1} END {printf "%s: min %s p50 %s max %s\n", name, a[1], a[int((NR+1)/2)], a[NR]}'; }
awk '{print $1}' "$work/lat" | stat "hv_restore, ms"
awk '{print $2}' "$work/lat" | stat "TCP reconnect after hv_restore call, ms"
awk '{print $3}' "$work/lat" | stat "first vsock echo after hv_restore call, ms"
awk '{print $4}' "$work/lat" | stat "idle TCP reconnect after hv_restore call, ms"
awk '{print $5}' "$work/lat" | stat "download reconnect after hv_restore call, ms"
grep -o '[0-9]* TCP flows' "$work/save" | sort | uniq -c | sed 's/^/flows aborted per checkpoint: /'
awk '{print $5}' "$work/save" | stat "vCPU pause, ms"
sed -n 's/.*I\/O drain \([0-9.]*\) ms.*/\1/p' "$work/save" | stat "I/O worker stop, ms"

section "vCPU count sweep"
for n in 1 2 8; do
  printf -- '-- %s vCPUs: ' "$n"
  SPIKE_VCPUS=$n "$bin" save "$work/n$n" 3 | grep -o 'pause.*total [0-9]* ms' | tr '\n' ' '
  "$bin" restore "$work/n$n" 4 | tail -1
done

section "spike 3 mode (no net, no vsock) still restores"
SPIKE_NET= "$bin" save "$work/blk" 3 >/dev/null
SPIKE_NET= "$bin" restore "$work/blk" 4 | tail -1
