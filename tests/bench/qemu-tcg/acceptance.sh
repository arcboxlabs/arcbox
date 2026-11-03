#!/usr/bin/env bash
# Acceptance run of the System VM on QEMU, on a Mac without hardware
# virtualization: the daemon falls back to QEMU, dockerd comes up, Docker
# works (amd64 through FEX too), a guest reboot recovers, and SIGTERM stops
# the daemon with no QEMU left. Prints one timing line per step and exits
# non-zero when any check fails. See README.md.
#
#   [ARCBOX_QEMU_PATH=<qemu-system-aarch64>] [ARCBOX_AGENT=<agent>] \
#     acceptance.sh <data-dir> [restarts]
#
# <data-dir> is a development data directory with the System VM's assets
# (`abctl boot prefetch --data-dir`), not the ~/.arcbox of a running
# production daemon. Without ARCBOX_QEMU_PATH (or `[qemu] path`) the daemon
# installs the QEMU build assets.lock pins into <data-dir>/runtime/qemu;
# a QEMU named there must be an arcboxlabs/qemu build. ARCBOX_AGENT, a musl
# build of this tree's agent, replaces the boot assets' one.
set -euo pipefail
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
out=$data/acceptance.out
failures=0
daemon=

now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }
since() { perl -e 'printf "%.2f s", $ARGV[1] - $ARGV[0]' "$1" "$2"; }
qemu_bin=${ARCBOX_QEMU_PATH:-$data/runtime/qemu/}
qemu() { pgrep -f "$qemu_bin" | head -1 || true; }
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}
# Stops a daemon this script started and left running, on any exit.
cleanup() {
  if [ -n "$daemon" ]; then
    kill -TERM "$daemon" 2>/dev/null || true
    wait "$daemon" 2>/dev/null || true
  fi
}
trap cleanup EXIT
# `wait_for SECONDS WHAT cmd...` retries cmd until it succeeds, and gives up
# with the daemon's log when the daemon exits or SECONDS pass.
wait_for() {
  local limit=$1 what=$2 s
  shift 2
  s=$(date +%s)
  until "$@" >/dev/null 2>&1; do
    if ! kill -0 "$daemon" 2>/dev/null || [ $(($(date +%s) - s)) -ge "$limit" ]; then
      echo "FAIL: no $what within $limit s" >&2
      tail -20 "$log" >&2
      exit 1
    fi
    sleep 0.25
  done
}
# `timed [-e TEXT] cmd...` runs one step and prints its time; the step fails
# when cmd does, or when its output lacks TEXT.
timed() {
  local s rc=0 expect=
  if [ "$1" = -e ]; then
    expect=$2
    shift 2
  fi
  s=$(now)
  "$@" >"$out" 2>&1 || rc=$?
  printf '%-62s rc=%d %8s | %s\n' "$*" "$rc" "$(since "$s" "$(now)")" \
    "$(tail -1 "$out" | cut -c1-50)"
  if [ "$rc" -ne 0 ]; then
    fail "$* exited $rc"
  elif [ -n "$expect" ] && ! grep -qF "$expect" "$out"; then
    fail "$* printed no $expect"
  fi
}
start_daemon() {
  local s
  s=$(now)
  target/debug/arcbox-daemon --data-dir "$data" --grpc-socket "$data/run/arcbox.sock" \
    --socket "$data/run/docker.sock" --dns-port 0 --ssh-port 0 --kubernetes-port 0 \
    --no-mount-nfs --foreground >>"$log" 2>&1 &
  daemon=$!
  wait_for 120 "dockerd" docker version
  echo "daemon start -> dockerd ready: $(since "$s" "$(now)")"
}
stop_daemon() {
  local s left
  s=$(now)
  kill -TERM "$daemon"
  wait "$daemon" || true
  daemon=
  left=$(qemu)
  echo "SIGTERM -> daemon exit: $(since "$s" "$(now)"), QEMU left: ${left:-none}"
  [ -z "$left" ] || fail "QEMU $left outlived the daemon"
}
new_qemu() {
  local pid
  pid=$(qemu)
  [ -n "$pid" ] && [ "$pid" != "$before" ]
}
container_runs() { docker run --rm alpine echo back | grep -q back; }
guest_agent() {
  docker run --rm --privileged --pid=host alpine sh -c 'for p in /proc/[0-9]*; do
    case $(readlink "$p/exe" 2>/dev/null) in
      *arcbox-agent) echo "guest agent pid ${p#/proc/}: $(readlink "$p/exe")" ;;
    esac
  done'
}

if [ -n "$(qemu)" ]; then
  echo "a QEMU from $qemu_bin is already running" >&2
  exit 1
fi
echo "kern.hv_support: $(sysctl -n kern.hv_support 2>/dev/null || echo absent)"
start_daemon
target/debug/abctl daemon status --grpc-socket "$data/run/arcbox.sock" --data-dir "$data" |
  grep -E 'virtualization|backend' || true
echo "QEMU: $(ps -o command= -p "$(qemu)" | cut -d' ' -f1)"

docker rmi -f alpine python:3.13-slim >/dev/null 2>&1 || true
timed -e hello docker run --rm alpine echo hello
for _ in 1 2 3 4 5; do timed -e hello docker run --rm alpine echo hello; done
timed docker pull python:3.13-slim
timed -e 499999500000 docker run --rm python:3.13-slim python3 -c 'print(sum(range(10**6)))'
docker rm -f acceptance >/dev/null 2>&1 || true
timed docker run -d --name acceptance --restart=always alpine \
  sh -c 'while true; do echo tick; sleep 1; done'
timed docker exec acceptance cat /etc/alpine-release
sleep 2
timed -e tick docker logs acceptance
timed -e x86_64 docker run --rm --platform linux/amd64 alpine uname -m
timed -e x86_64 docker run --rm --platform linux/amd64 alpine uname -m
# The agent runs from /arcbox/bin, the `arcbox` share, not from a copy.
timed -e ': /arcbox/bin/arcbox-agent' guest_agent

# A reboot from inside: QEMU exits on the reset and the daemon starts a
# new one.
before=$(qemu)
s=$(now)
docker run --rm --privileged --pid=host alpine \
  nsenter -t 1 -m -u -i -n -p -- reboot >/dev/null 2>&1 || true
wait_for 120 "new QEMU after the reboot" new_qemu
wait_for 120 "container after the reboot" container_runs
echo "guest reboot -> a container runs again: $(since "$s" "$(now)")"
status=$(docker ps --filter name='^acceptance$' --format '{{.Status}}')
echo "acceptance after the reboot: ${status:-gone}"
case $status in Up*) ;; *) fail "the --restart=always container did not come back" ;; esac
docker rm -f acceptance >/dev/null

for _ in $(seq 1 "$restarts"); do
  stop_daemon
  start_daemon
done
stop_daemon
if [ "$failures" -ne 0 ]; then
  echo "$failures check(s) failed" >&2
  exit 1
fi
echo "every check passed"
