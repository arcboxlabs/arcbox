#!/bin/busybox sh
# Staged at /arcbox/bin/arcbox-agent, the path rcS (`init`) and inittab
# (`serve`) run; the real agent sits next to it as arcbox-agent.real and
# runs straight from the share, as the daemon's QEMU backend runs it.
[ "$1" = init ] && exec /arcbox/bin/arcbox-agent.real init
if [ ! -e /run/probe-driver ]; then
  : > /run/probe-driver
  /bin/busybox setsid /arcbox/probe/driver.sh </dev/null >/dev/console 2>&1 &
fi
export RUST_LOG=info,arcbox_agent::mount=debug
exec /arcbox/bin/arcbox-agent.real "$@"
