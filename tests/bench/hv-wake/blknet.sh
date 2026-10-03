#!/bin/zsh
# Usage: DOCKER_HOST=... blknet.sh <grpc-socket> <label>
# blk/net-heavy checks: docker load 300 MB x3, docker build, docker pull (net RX), stdin EOF.
set -eu
SOCK=$1; LABEL=$2; B=$(dirname $0); TAR=$B/bigimg.tar
if ! docker image inspect arcbox-bench-big >/dev/null 2>&1; then
  printf 'FROM alpine\nRUN dd if=/dev/urandom of=/big bs=1M count=300 2>/dev/null\n' | docker build -q -t arcbox-bench-big - >/dev/null
fi
[[ -s $TAR ]] || docker save arcbox-bench-big -o $TAR
for i in 1 2 3; do
  docker rmi -f arcbox-bench-big >/dev/null 2>&1 || true
  b=$($B/snap.sh $SOCK); t0=$(python3 -c 'import time;print(time.time())')
  docker load -i $TAR >/dev/null
  t1=$(python3 -c 'import time;print(time.time())'); a=$($B/snap.sh $SOCK)
  jq -nc --arg label "$LABEL-load-$i" --argjson b "$b" --argjson a "$a" --argjson t0 $t0 --argjson t1 $t1 \
    '{label:$label, secs:(($t1-$t0)*100|round/100), d_kick:($a.kick-$b.kick), d_unpark:($a.unpark-$b.unpark), d_kicks_received:($a.kicks_received-$b.kicks_received)}'
done
t0=$(python3 -c 'import time;print(time.time())')
printf 'FROM alpine\nRUN dd if=/dev/urandom of=/f bs=1M count=64 2>/dev/null && sha256sum /f > /s\nCMD cat /s\n' | docker build -q --no-cache -t arcbox-bench-build - >/dev/null && docker run --rm arcbox-bench-build | cut -c1-16
t1=$(python3 -c 'import time;print(time.time())'); echo "{\"label\":\"$LABEL-build\",\"secs\":$(python3 -c "print(round($t1-$t0,2))")}"
docker rmi -f ubuntu:24.04 >/dev/null 2>&1 || true
t0=$(python3 -c 'import time;print(time.time())'); docker pull -q ubuntu:24.04 >/dev/null; t1=$(python3 -c 'import time;print(time.time())')
echo "{\"label\":\"$LABEL-pull-ubuntu\",\"secs\":$(python3 -c "print(round($t1-$t0,2))")}"
printf 'hello\n' | docker run -i --rm alpine cat
