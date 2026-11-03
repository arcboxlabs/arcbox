#!/usr/bin/env bash
# A failed QEMU boot must not hold the daemon. Boots the System VM on QEMU
# with a dev daemon, lets QEMU go away (the guest powers off once rcS fails
# without its virtio-fs share, or QEMU is SIGKILLed), then measures how long
# the daemon takes to exit on SIGTERM. See README.md.
#
#   ARCBOX_QEMU_PATH=<qemu-system-aarch64> sigterm.sh <data-dir> poweroff|kill
#
# <data-dir> is a development data directory whose System VM assets are
# prepared, not the ~/.arcbox of a running production daemon.
set -euo pipefail
: "${ARCBOX_QEMU_PATH:?set ARCBOX_QEMU_PATH to qemu-system-aarch64}"
data=$(cd "${1:?data directory}" && pwd)
mode=${2:?poweroff or kill}
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
fi
while kill -0 "$pid" 2>/dev/null; do sleep 0.05; done
gone=$(now)
sleep 1
target/debug/abctl daemon status --grpc-socket "$data/run/arcbox.sock" --data-dir "$data" |
  grep -E '^System (runtime|detail)' || true
term=$(now)
kill -TERM "$daemon"
wait "$daemon"
exited=$(now)
echo "QEMU spawned -> gone: $(since "$spawned" "$gone")"
echo "SIGTERM -> daemon exit: $(since "$term" "$exited")"
if [ -n "$(qemu)" ]; then
  echo "QEMU left running" >&2
  exit 1
fi
