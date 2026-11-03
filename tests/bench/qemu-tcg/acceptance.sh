#!/usr/bin/env bash
# Acceptance run of the System VM on QEMU, on a Mac without hardware
# virtualization: the daemon falls back to QEMU, dockerd comes up, Docker
# works (amd64 through FEX too), a guest reboot recovers, and SIGTERM stops
# the daemon with no QEMU left. Prints one timing line per step. See
# README.md.
#
#   ARCBOX_QEMU_PATH=<qemu-system-aarch64> [ARCBOX_AGENT=<agent>] \
#     acceptance.sh <data-dir> [restarts]
#
# <data-dir> is a development data directory with the System VM's assets
# (`abctl boot prefetch --data-dir`), not the ~/.arcbox of a running
# production daemon. ARCBOX_AGENT, a musl build of this tree's agent,
# replaces the boot assets' one. The QEMU must be an arcboxlabs/qemu build.
set -euo pipefail
: "${ARCBOX_QEMU_PATH:?set ARCBOX_QEMU_PATH to qemu-system-aarch64}"
data=$(cd "${1:?data directory}" && pwd)
restarts=${2:-3}
root=$(cd "$(dirname "$0")/../../.." && pwd)
cd "$root"
cargo build -p arcbox-daemon -p arcbox-cli >&2
export ARCBOX_HOST_MOUNT_DIR=$data/ArcBox DOCKER_HOST=unix://$data/run/docker.sock
if [ -n "${ARCBOX_AGENT:-}" ]; then
  for dir in "$data"/boot/*/; do cp "$ARCBOX_AGENT" "$dir/arcbox-agent"; done
fi
log=$data/acceptance.log

now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }
since() { perl -e 'printf "%.2f s", $ARGV[1] - $ARGV[0]' "$1" "$2"; }
qemu() { pgrep -f "$ARCBOX_QEMU_PATH" | head -1 || true; }
alive() { kill -0 "$daemon" 2>/dev/null || { tail -20 "$log" >&2; exit 1; }; }
timed() {
  local s rc=0
  s=$(now)
  "$@" >"$data/acceptance.out" 2>&1 || rc=$?
  printf '%-62s rc=%d %8s | %s\n' "$*" "$rc" "$(since "$s" "$(now)")" \
    "$(tail -1 "$data/acceptance.out" | cut -c1-50)"
}
start_daemon() {
  local s
  s=$(now)
  target/debug/arcbox-daemon --data-dir "$data" --grpc-socket "$data/run/arcbox.sock" \
    --socket "$data/run/docker.sock" --dns-port 0 --ssh-port 0 --kubernetes-port 0 \
    --no-mount-nfs --foreground >>"$log" 2>&1 &
  daemon=$!
  until docker version >/dev/null 2>&1; do
    alive
    sleep 0.25
  done
  echo "daemon start -> dockerd ready: $(since "$s" "$(now)")"
}
stop_daemon() {
  local s left
  s=$(now)
  kill -TERM "$daemon"
  wait "$daemon" || true
  left=$(qemu)
  echo "SIGTERM -> daemon exit: $(since "$s" "$(now)"), QEMU left: ${left:-none}"
}

if [ -n "$(qemu)" ]; then
  echo "a QEMU from $ARCBOX_QEMU_PATH is already running" >&2
  exit 1
fi
echo "kern.hv_support: $(sysctl -n kern.hv_support 2>/dev/null || echo absent)"
start_daemon
target/debug/abctl daemon status --grpc-socket "$data/run/arcbox.sock" --data-dir "$data" |
  grep -E 'virtualization|backend' || true

docker rmi -f alpine python:3.13-slim >/dev/null 2>&1 || true
timed docker run --rm alpine echo hello
for _ in 1 2 3 4 5; do timed docker run --rm alpine echo hello; done
timed docker pull python:3.13-slim
timed docker run --rm python:3.13-slim python3 -c 'print(sum(range(10**6)))'
docker rm -f acceptance >/dev/null 2>&1 || true
timed docker run -d --name acceptance --restart=always alpine \
  sh -c 'while true; do echo tick; sleep 1; done'
timed docker exec acceptance cat /etc/alpine-release
sleep 2
timed docker logs acceptance
timed docker run --rm --platform linux/amd64 alpine uname -m
timed docker run --rm --platform linux/amd64 alpine uname -m
# The agent runs from /arcbox/bin, the `arcbox` share, not from a copy.
docker run --rm --privileged --pid=host alpine sh -c 'for p in /proc/[0-9]*; do
  case $(readlink "$p/exe" 2>/dev/null) in
    *arcbox-agent) echo "guest agent pid ${p#/proc/}: $(readlink "$p/exe")" ;;
  esac
done'

# A reboot from inside: QEMU exits on the reset and the daemon starts a
# new one.
before=$(qemu)
s=$(now)
docker run --rm --privileged --pid=host alpine \
  nsenter -t 1 -m -u -i -n -p -- reboot >/dev/null 2>&1 || true
while [ "$(qemu)" = "$before" ]; do
  alive
  sleep 0.1
done
until docker run --rm alpine echo back 2>/dev/null | grep -q back; do
  alive
  sleep 0.5
done
echo "guest reboot -> a container runs again: $(since "$s" "$(now)")"
docker ps --filter name=acceptance --format '{{.Names}} after the reboot: {{.Status}}'
docker rm -f acceptance >/dev/null

for _ in $(seq 1 "$restarts"); do
  stop_daemon
  start_daemon
done
stop_daemon
