#!/bin/busybox sh
# Staged at /arcbox/bin/arcbox-agent, the path rcS (`init`) and inittab
# (`serve`) run; the real agent sits next to it as arcbox-agent.real.
#
# The agent runs from a devtmpfs copy, not from the share. QEMU TCG keeps
# executing its translation of a guest page after a vhost-user back-end
# rewrites that page, so a binary paged in by FUSE_READ into a page that
# once held code runs stale instructions (exec_coherence.rs measures it).
# The copy is written by the guest CPU, which QEMU does track.
bb=/bin/busybox
X=/dev/.probe
$bb mkdir -p $X
if [ ! -x $X/agent ]; then
  $bb cat /arcbox/bin/arcbox-agent.real > $X/agent.tmp
  $bb chmod 755 $X/agent.tmp
  $bb mv $X/agent.tmp $X/agent
fi
[ "$1" = init ] && exec $X/agent init
if [ ! -e /run/probe-driver ]; then
  : > /run/probe-driver
  $bb setsid /arcbox/probe/driver.sh </dev/null >/dev/console 2>&1 &
fi
export RUST_LOG=info,arcbox_agent::mount=debug
exec $X/agent "$@"
