#!/usr/bin/env bash
# A daemon running QEMU must stop promptly on SIGTERM. Boots the System VM on
# QEMU with a dev daemon, then either SIGKILLs QEMU 0.5 s into the boot
# (`kill`: the boot must fail at once rather than wait out its budget) or
# waits for dockerd (`ready`: the clean stop), and measures how long the
# daemon takes to exit on SIGTERM. See README.md.
#
#   ARCBOX_QEMU_PATH=<qemu-system-aarch64> sigterm.sh <data-dir> kill|ready
#
# <data-dir> is a development data directory whose System VM assets are
# prepared, not the ~/.arcbox of a running production daemon.
set -euo pipefail
: "${ARCBOX_QEMU_PATH:?set ARCBOX_QEMU_PATH to qemu-system-aarch64}"
data=$(cd "${1:?data directory}" && pwd)
mode=${2:?kill or ready}
case $mode in kill | ready) ;; *) echo "unknown mode $mode" >&2 && exit 2 ;; esac
root=$(cd "$(dirname "$0")/../../.." && pwd)
cd "$root"
cargo build -p arcbox-daemon -p arcbox-cli >&2
export ARCBOX_HOST_MOUNT_DIR=$data/ArcBox

now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }
since() { perl -e 'printf "%.2f s\n", $ARGV[1] - $ARGV[0]' "$1" "$2"; }
qemu() { pgrep -f "$ARCBOX_QEMU_PATH" | head -1 || true; }

if [ -n "$(qemu)" ]; then
  echo "a QEMU from $ARCBOX_QEMU_PATH is already running" >&2
  exit 1
fi
target/debug/arcbox-daemon --data-dir "$data" --grpc-socket "$data/run/arcbox.sock" \
  --socket "$data/run/docker.sock" --dns-port 0 --ssh-port 0 --kubernetes-port 0 \
  --no-mount-nfs --foreground >"$data/sigterm.log" 2>&1 &
daemon=$!
pid=
until [ -n "$pid" ]; do
  kill -0 "$daemon"
  sleep 0.05
  pid=$(qemu)
done
spawned=$(now)
if [ "$mode" = kill ]; then
  sleep 0.5
  kill -9 "$pid"
  while kill -0 "$pid" 2>/dev/null; do sleep 0.05; done
  echo "QEMU spawned -> gone: $(since "$spawned" "$(now)")"
  sleep 1
else
  until DOCKER_HOST=unix://$data/run/docker.sock docker version >/dev/null 2>&1; do
    kill -0 "$daemon"
    sleep 0.25
  done
  echo "QEMU spawned -> dockerd ready: $(since "$spawned" "$(now)")"
fi
target/debug/abctl daemon status --grpc-socket "$data/run/arcbox.sock" --data-dir "$data" |
  grep -E '^System (runtime|detail)' || true
term=$(now)
kill -TERM "$daemon"
wait "$daemon"
exited=$(now)
echo "SIGTERM -> daemon exit: $(since "$term" "$exited")"
if [ -n "$(qemu)" ]; then
  echo "QEMU left running" >&2
  exit 1
fi
