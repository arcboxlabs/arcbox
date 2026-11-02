#!/usr/bin/env bash
# Runs one QEMU download scenario on a Mac without hardware virtualization
# (an Amp Mac satellite), against cdn.py standing in for the CDN, with an
# isolated development daemon. Prints the System status updates the daemon
# streamed, the CDN's request log and what was installed.
#
# Usage: run.sh <arcbox-daemon> <abctl> <archive> <scenario> [out-dir]
#
#   full       download at 1.5 MB/s
#   resume     the first transfer is cut at 2 MB; the retry resumes
#   restart    the daemon is stopped mid-transfer and started again
#   checksum   the pin names another SHA-256
#   import     the CDN answers 503; `abctl qemu import` installs a
#              quarantined copy while the daemon waits to retry
#   signature  for binaries built with the archive pinned in assets.lock
#              (cdn = http://127.0.0.1:18080/qemu): the pinned build needs
#              ArcBox's Developer ID, which the ad-hoc CI build lacks
#
# Every scenario but `signature` names the archive through the development
# override (ARCBOX_QEMU_VERSION/SHA256/CDN).
set -euo pipefail

daemon=$(realpath "$1")
abctl=$(realpath "$2")
archive=$(realpath "$3")
scenario=$4
out=${5:-/tmp/qemu-download}/$scenario
here=$(cd "$(dirname "$0")" && pwd)
name=$(basename "$archive")
version=${name#qemu-darwin-arm64-v}
version=${version%.tar.gz}
sha=$(shasum -a 256 "$archive" | awk '{print $1}')
port=18080
dir=$(mktemp -d /tmp/abx-qemu.XXXXXX)
rm -rf "$out" && mkdir -p "$out"
mkdir -p "$dir/bin" "$out/cdn"
cp "$archive" "$out/cdn/"
# The System VM never boots here; a stub agent is enough for startup.
printf '#!/bin/sh\n# arcbox-storage-recovery-v1\n' >"$dir/bin/arcbox-agent"
chmod +x "$dir/bin/arcbox-agent"

export ARCBOX_HOST_MOUNT_DIR=$dir/mnt
if [ "$scenario" != signature ]; then
  export ARCBOX_QEMU_VERSION=$version ARCBOX_QEMU_CDN=http://127.0.0.1:$port/qemu
  export ARCBOX_QEMU_SHA256=$sha
  [ "$scenario" = checksum ] && export ARCBOX_QEMU_SHA256=$(printf 'x' | shasum -a 256 | awk '{print $1}')
fi

pids=()
cleanup() { for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done; wait 2>/dev/null || true; }
trap cleanup EXIT

# Runs the command until it succeeds, for up to 30 s, while process $2 lives.
wait_ready() {
  local what=$1 pid=$2
  shift 2
  for _ in $(seq 1 300); do
    "$@" && return 0
    kill -0 "$pid" 2>/dev/null || { echo "$what exited before it was ready; logs in $out" >&2; return 1; }
    sleep 0.1
  done
  echo "timed out waiting for $what; logs in $out" >&2
  return 1
}

# A cold python3 can take seconds to bind; the daemon must not race it.
cdn() {
  python3 "$here/cdn.py" "$out/cdn" "$port" "$@" >>"$out/cdn.log" 2>&1 &
  pids+=($!)
  wait_ready cdn.py $! grep -q '^serving' "$out/cdn.log"
}
start_daemon() {
  "$daemon" --profile development --data-dir "$dir" --grpc-socket "$dir/arcbox.sock" \
    --socket "$dir/docker.sock" --dns-domain qemu.dev.arcbox.local --dns-port 0 \
    --kubernetes-port 0 --ssh-port 0 --no-mount-nfs >>"$out/daemon.out" 2>&1 &
  daemon_pid=$!
  pids+=("$daemon_pid")
  wait_ready arcbox-daemon "$daemon_pid" test -S "$dir/arcbox.sock"
  printf '\x00\x00\x00\x00\x02{}' | curl -sN --unix-socket "$dir/arcbox.sock" \
    -H 'Content-Type: application/connect+json' --data-binary @- \
    http://localhost/arcbox.v1.SystemService/WatchSetupStatus |
    python3 "$here/watch.py" >>"$out/watch.log" &
  pids+=($!)
}
state() { grep -c "$1" "$out/watch.log" 2>/dev/null || true; }
wait_for() { for _ in $(seq 1 "${2:-600}"); do [ "$(state "$1")" -gt 0 ] && return 0; sleep 0.1; done; echo "timed out waiting for $1; logs in $out" >&2; return 1; }

case $scenario in
  full | checksum | signature) cdn --rate 1500000; start_daemon ;;
  resume) cdn --rate 2000000 --cut-first 2000000; start_daemon ;;
  restart)
    cdn --rate 1000000
    start_daemon
    kept() { [ -n "$(find "$dir/runtime/qemu/.downloads" -name '*.part' -size +2000k 2>/dev/null)" ]; }
    wait_ready "a 2 MB partial download" "$daemon_pid" kept
    kill -TERM "$daemon_pid"; wait "$daemon_pid" || true
    echo "--- daemon restarted with $(stat -f %z "$dir"/runtime/qemu/.downloads/*.part) bytes kept ---" >>"$out/watch.log"
    start_daemon
    ;;
  import)
    cdn --status 503
    start_daemon
    wait_for WAITING_TO_RETRY
    copy=$out/$name
    cp "$archive" "$copy"
    xattr -w com.apple.quarantine "0083;$(printf %x "$(date +%s)");Safari;" "$copy"
    echo "--- abctl qemu import (archive quarantine: $(xattr -p com.apple.quarantine "$copy")) ---" >>"$out/watch.log"
    "$abctl" --profile development qemu import "$copy" --data-dir "$dir" >>"$out/watch.log" 2>&1
    ;;
esac
wait_for "FAILED"
sleep 0.5

echo "== System status updates"; cat "$out/watch.log"
echo "== CDN requests"; grep '^GET' "$out/cdn.log" || true
echo "== $dir/runtime/qemu"; ls -A "$dir/runtime/qemu" 2>/dev/null || true
installed=$dir/runtime/qemu/$version
if [ -x "$installed/bin/qemu-system-aarch64" ]; then
  echo "licenses: $(ls "$installed/licenses" | tr '\n' ' ')"
  echo "quarantined files: $(find "$installed" -exec xattr -p com.apple.quarantine {} \; 2>/dev/null | wc -l | tr -d ' ')"
  "$installed/bin/qemu-system-aarch64" --version | head -1
fi
if [ "$scenario" = signature ]; then
  echo "== abctl qemu import of the same archive"
  "$abctl" --profile development qemu import "$archive" --data-dir "$dir" 2>&1 || true
fi
echo "data directory: $dir"
