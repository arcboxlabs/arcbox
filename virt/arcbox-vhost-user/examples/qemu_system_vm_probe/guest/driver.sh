#!/bin/busybox sh
# Guest side of qemu_system_vm_probe, started by wrapper.sh beside the
# long-running agent. Reports `PROBE ...` lines on the console, then powers
# the VM off. /arcbox/probe/env sets G (runtime generation), U and P (the
# small-file trees under /Users and /private) and EXEC_ROUNDS.
. /arcbox/probe/env
bb=/bin/busybox
rpc=/arcbox/bin/agent-rpc
R=/run/arcbox/data/runtime
RT=/arcbox/runtime/$G
say() { $bb printf 'PROBE %s\n' "$*" > /dev/console; }
up() { $bb cut -d' ' -f1 /proc/uptime; }
drop() { echo 3 > /proc/sys/vm/drop_caches; }

say "$(/arcbox/bin/exec-coherence /arcbox/probe/ret2.bin /arcbox/probe/exec $EXEC_ROUNDS 2>&1)"

i=0
until $rpc 1 > /run/p-ping 2>&1; do
  i=$((i + 1)); [ $i -gt 600 ] && break; $bb sleep 0.5
done
say "ping up=$(up) $(cat /run/p-ping)"
for m in /arcbox /Users /private; do say "mount $($bb grep " $m " /proc/mounts)"; done

# EnsureRuntime answers "starting" at once and materializes the runtime in
# the background, so poll it, and time the staging directory meanwhile.
(
  s=
  while [ ! -e $R/$G ]; do
    [ -z "$s" ] && $bb ls -d $R/.$G.*.tmp > /dev/null 2>&1 && s=$(up) && say "staging up=$s"
    $bb sleep 0.1
  done
  say "published up=$(up)"
) &
watcher=$!
t0=$(up)
i=0
until $rpc 3 0801 > /run/p-ensure 2>&1 && $bb grep -q ' 1=1' /run/p-ensure; do
  $bb grep -q '"failed"' /run/p-ensure && break
  i=$((i + 1)); [ $i -gt 3000 ] && break; $bb sleep 0.2
done
t1=$(up)
$bb sleep 0.3; kill $watcher 2> /dev/null; wait
say "ensure start=$t0 end=$t1 $(cat /run/p-ensure)"

drop
n=$($bb stat -c %s $RT/bin/* $RT/kernel/* | $bb awk '{s+=$1} END {print s}')
t0=$(up); $bb cat $RT/bin/* $RT/kernel/* > /dev/null; t1=$(up)
say "runtime-read bytes=$n start=$t0 end=$t1"

$bb sync; say "sync rc=$?"
for m in /arcbox /Users /private /Users; do $bb sync -f $m; say "syncfs $m rc=$?"; done

# Tars directory $1 and writes to /run/p-$2 how many 4096-byte files the
# archive holds and tar's exit status. A file the walk missed lowers the
# count; a failed read makes tar exit non-zero.
tarred() {
  n=$({ $bb tar cf - -C "$1" .; echo $? > /run/p-$2.rc; } |
    $bb tar tvf - | $bb awk '$3 == 4096 {n++} END {print n+0}')
  echo "files=$n rc=$(cat /run/p-$2.rc)" > /run/p-$2
}

drop
t0=$(up); tarred "$U" u; t1=$(up)
say "users-tar1 $(cat /run/p-u) start=$t0 end=$t1"
drop
t0=$(up)
for d in 0 1 2 3; do tarred "$P/d$d" p$d & done
wait
t1=$(up)
say "private-tar4 $($bb cat /run/p-p0 /run/p-p1 /run/p-p2 /run/p-p3 | $bb awk -F'[= ]' \
  '{n+=$2; if ($4 != 0) rc=$4} END {print "files=" n " rc=" rc+0}') start=$t0 end=$t1"
$bb sync; say "done"
$bb poweroff -f
