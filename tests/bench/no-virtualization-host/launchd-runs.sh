#!/usr/bin/env bash
# Runs an arcbox-daemon under a LaunchAgent with the Desktop daemon's restart
# policy (KeepAlive SuccessfulExit=false, ThrottleInterval 5) on an isolated
# development data directory, then prints launchd's run count and the
# daemon's setup status.
#
# On a Mac without hardware virtualization (a macOS VM such as an Amp Mac
# satellite), a daemon that exits when the System VM cannot start shows a
# climbing `runs`; one that stays up shows `runs = 1`.
#
# Usage: launchd-runs.sh <arcbox-daemon> <abctl> [seconds]
set -euo pipefail

daemon=$(realpath "$1")
abctl=$(realpath "$2")
seconds=${3:-60}
label=com.arcboxlabs.bench.no-virtualization
domain="gui/$(id -u)"
plist="$HOME/Library/LaunchAgents/$label.plist"
dir=$(mktemp -d /tmp/abx-novirt.XXXXXX)

# Daemons before the host check look for this marker in the staged agent
# before they start the VM. The VM never boots here, so a stub carrying it
# is enough to reach the VM start.
mkdir -p "$dir/bin" "$(dirname "$plist")"
printf '#!/bin/sh\n# arcbox-storage-recovery-v1\n' >"$dir/bin/arcbox-agent"
chmod +x "$dir/bin/arcbox-agent"

cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key>
  <array>
    <string>$daemon</string>
    <string>--profile</string><string>development</string>
    <string>--data-dir</string><string>$dir</string>
    <string>--grpc-socket</string><string>$dir/arcbox.sock</string>
    <string>--socket</string><string>$dir/docker.sock</string>
    <string>--dns-domain</string><string>novirt.dev.arcbox.local</string>
    <string>--dns-port</string><string>0</string>
    <string>--kubernetes-port</string><string>0</string>
    <string>--ssh-port</string><string>0</string>
    <string>--no-mount-nfs</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict><key>ARCBOX_HOST_MOUNT_DIR</key><string>$dir/mnt</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>5</integer>
</dict>
</plist>
EOF

trap 'launchctl bootout "$domain/$label" 2>/dev/null || true; rm -f "$plist"' EXIT
# bootout returns before the job is gone; bootstrapping the label again
# meanwhile fails with EIO.
launchctl bootout "$domain/$label" 2>/dev/null || true
while launchctl print "$domain/$label" >/dev/null 2>&1; do sleep 1; done
launchctl bootstrap "$domain" "$plist"
sleep "$seconds"

echo "after ${seconds}s:"
launchctl print "$domain/$label" | grep -E '^[[:space:]]+(state|runs|last exit code) = ' | head -3
"$abctl" daemon status --profile development --data-dir "$dir" \
  --grpc-socket "$dir/arcbox.sock" || true
echo "error-level log lines: $(grep -c '"level":"ERROR"' "$dir/log/daemon.log" || true)"
echo "data directory: $dir"
