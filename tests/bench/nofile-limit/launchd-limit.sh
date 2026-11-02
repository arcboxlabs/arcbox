#!/usr/bin/env bash
# Starts an arcbox-daemon the way launchd starts the Desktop's daemon and the
# `abctl _install` agent: a LaunchAgent whose plist sets no resource limits,
# so the job gets launchd's `maxfiles` soft limit. Then it holds <connections>
# idle connections open on the daemon's gRPC socket and counts the daemon's
# descriptors with lsof (docs/logs/2026-11-02-daemon-nofile-limit.md).
#
# A stock Mac's `launchctl limit maxfiles` soft value is 256. On a Mac whose
# launchd was configured higher (an Amp Mac satellite reports 524288), pass
# the soft limit to emulate as <soft>: the plist then sets SoftResourceLimits
# to it and nothing else.
#
# A daemon that keeps launchd's 256 cannot hold more than 256 descriptors; one
# that raises its own limit holds one per connection. The client's connects
# succeed either way: connections the daemon cannot accept wait in the
# listen backlog.
#
# Usage: launchd-limit.sh <arcbox-daemon> [connections] [soft]
set -euo pipefail

daemon=$(realpath "$1")
connections=${2:-1000}
soft=${3:-}
label=com.arcboxlabs.bench.nofile-limit
domain="gui/$(id -u)"
plist="$HOME/Library/LaunchAgents/$label.plist"
dir=$(mktemp -d /tmp/abx-nofile.XXXXXX)
mkdir -p "$(dirname "$plist")"

limits=
if [ -n "$soft" ]; then
  limits="<key>SoftResourceLimits</key><dict><key>NumberOfFiles</key><integer>$soft</integer></dict>"
fi

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
    <string>--dns-domain</string><string>nofile.dev.arcbox.local</string>
    <string>--dns-port</string><string>0</string>
    <string>--kubernetes-port</string><string>0</string>
    <string>--ssh-port</string><string>0</string>
    <string>--no-linux-vm</string>
    <string>--no-mount-nfs</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict><key>ARCBOX_HOST_MOUNT_DIR</key><string>$dir/mnt</string></dict>
  <key>RunAtLoad</key><true/>
  $limits
</dict>
</plist>
EOF

trap 'launchctl bootout "$domain/$label" 2>/dev/null || true; rm -f "$plist"' EXIT
launchctl bootout "$domain/$label" 2>/dev/null || true
while launchctl print "$domain/$label" >/dev/null 2>&1; do sleep 1; done
echo "launchctl limit maxfiles: $(launchctl limit maxfiles | tr -s ' ')"
launchctl bootstrap "$domain" "$plist"

for _ in $(seq 1 60); do [ -S "$dir/arcbox.sock" ] && break; sleep 0.5; done
pid=$(launchctl print "$domain/$label" | awk '$1 == "pid" { print $3 }')
echo "daemon pid $pid"
grep -h 'open-file soft limit' "$dir"/log/daemon.log* || echo "(no open-file limit line)"
echo "daemon descriptors before: $(lsof -n -p "$pid" -F f | grep -c '^f[0-9]')"

# Hold the connections in a child so the count is taken while they are open.
python3 -I - "$dir/arcbox.sock" "$connections" "$pid" <<'EOF'
import resource, socket, subprocess, sys, time
path, n, pid = sys.argv[1], int(sys.argv[2]), sys.argv[3]
# The client needs one descriptor per connection too.
_, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
resource.setrlimit(resource.RLIMIT_NOFILE, (n + 64, hard))
held, refused = [], None
for i in range(n):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(5)
    try:
        s.connect(path)
    except OSError as e:
        refused = f"connection {i + 1} failed: {e}"
        break
    held.append(s)
time.sleep(3)
# -F ft: one "f<fd>" line per descriptor, then its "t<type>" line.
lsof = subprocess.run(["lsof", "-n", "-p", pid, "-F", "ft"], capture_output=True, text=True)
fields = lsof.stdout.splitlines()
fds = sum(1 for f in fields if f[:1] == "f" and f[1:].isdigit())
unix = sum(1 for f in fields if f == "tunix")
print(f"client connected {len(held)}/{n}" + (f" ({refused})" if refused else ""))
print(f"daemon descriptors while held: {fds} ({unix} unix sockets)")
EOF
sleep 1
echo "daemon still running: $(launchctl print "$domain/$label" | awk '$1 == "state" { print $3; exit }')"
echo "warnings and errors logged: $(grep -c -E '"level":"(WARN|ERROR)"' "$dir"/log/daemon.log* | awk -F: '{ s += $NF } END { print s + 0 }')"
echo "data directory: $dir"
