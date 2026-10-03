#!/bin/zsh
# Usage: DOCKER_HOST=... busy0.sh start|stop  — pin a busy loop to guest CPU0 (the SPI target).
set -eu
case $1 in
  start) docker run -d --rm --name busy0 --cpuset-cpus=0 alpine sh -c 'while :; do :; done' >/dev/null; sleep 1; docker run --rm --pid=host --privileged alpine sh -c 'top -bn1 | head -4 | tail -2' ;;
  stop) docker rm -f busy0 >/dev/null 2>&1 || true ;;
esac
