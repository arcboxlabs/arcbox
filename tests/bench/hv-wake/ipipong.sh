#!/bin/zsh
# Usage: DOCKER_HOST=... ipipong.sh <label> [rounds]
# Guest-internal wake latency: two containers ping-pong over FIFOs on a shared
# volume, pinned with --cpuset-cpus to different CPUs (the peer must be woken
# by an IPI) and to the same CPU (no IPI). Timed with /proc/uptime.
set -eu
LABEL=$1; N=${2:-2000}
run() { # $1 = cpu A (timer), $2 = cpu B (echo)
  docker volume rm -f pong >/dev/null 2>&1 || true
  docker run --rm -v pong:/x alpine sh -c 'mkfifo /x/f1 /x/f2'
  docker run -d --rm --name pongb --cpuset-cpus=$2 -v pong:/x alpine sh -c "for i in \$(seq $N); do read y < /x/f1; echo y > /x/f2; done" >/dev/null
  sleep 0.5
  docker run --rm --cpuset-cpus=$1 -v pong:/x alpine sh -c "
t0=\$(cut -d' ' -f1 /proc/uptime)
for i in \$(seq $N); do echo x > /x/f1; read y < /x/f2; done
t1=\$(cut -d' ' -f1 /proc/uptime)
awk -v a=\$t0 -v b=\$t1 -v n=$N 'BEGIN{printf \"%.1f\", (b-a)*1e6/n}'"
  docker rm -f pongb >/dev/null 2>&1 || true
}
cross=$(run 3 4); same=$(run 3 3)
echo "{\"label\":\"$LABEL\",\"ipi_pong_us\":{\"cross_cpu_3_4\":$cross,\"same_cpu_3\":$same},\"rounds\":$N}"
