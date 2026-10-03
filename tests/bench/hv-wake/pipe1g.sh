#!/bin/zsh
# Usage: DOCKER_HOST=... pipe1g.sh <grpc-socket> <label> [bytes]
set -eu
SOCK=$1; LABEL=${2:-run}; BYTES=${3:-1073741824}
B=$(dirname $0)
before=$($B/snap.sh $SOCK)
load=$(uptime | sed 's/.*load averages*: *//')
t0=$(python3 -c 'import time;print(time.time())')
out=$(head -c $BYTES /dev/zero | docker run -i --rm alpine wc -c)
t1=$(python3 -c 'import time;print(time.time())')
after=$($B/snap.sh $SOCK)
jq -nc --arg label "$LABEL" --arg load "$load" --arg out "$out" \
  --argjson b "$before" --argjson a "$after" --argjson t0 $t0 --argjson t1 $t1 \
  '{label:$label, wc:$out, secs:(($t1-$t0)*100|round/100), load:$load,
    d_kick:($a.kick-$b.kick), d_unpark:($a.unpark-$b.unpark),
    d_kicks_received:($a.kicks_received-$b.kicks_received), d_kicks_v0:($a.kicks_v0-$b.kicks_v0),
    d_vsock_irqs:($a.vsock_irqs-$b.vsock_irqs)}'
