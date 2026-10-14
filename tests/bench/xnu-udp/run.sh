#!/bin/sh
# Run inside devenv so the recorded toolchain matches the benchmark build.
set -eu

if [ "$#" -ne 1 ]; then
    echo "Usage: $0 send|recv|smoke" >&2
    exit 2
fi
case "$1" in
    send) set -- '^ipv4/' --sample-size 10 --warm-up-time 1 --measurement-time 2 ;;
    recv) set -- '^ipv4_recv/' --sample-size 30 --warm-up-time 1 --measurement-time 3 ;;
    smoke) set -- '^ipv4(_recv)?/' --test ;;
    *) echo "Usage: $0 send|recv|smoke" >&2; exit 2 ;;
esac

root=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
cd "$root"
output=$(mktemp -d "${TMPDIR:-/tmp}/arcbox-xnu-udp.XXXXXX")
printf 'Results: %s\n' "$output"
{
    date -u '+UTC: %Y-%m-%dT%H:%M:%SZ'
    printf 'Git HEAD: '
    git rev-parse HEAD
    printf 'Git status:\n'
    git status --short
    uname -a
    if [ "$(uname -s)" = Darwin ]; then
        sw_vers
        sysctl hw.model hw.physicalcpu hw.logicalcpu hw.memsize
    fi
    rustc -vV
    cargo --version
} > "$output/environment.txt"

set -- cargo bench --locked -p arcbox-xnu-net --bench throughput -- "$@"
printf '%s\n' "$@" > "$output/argv.txt"
status=0
CRITERION_HOME="$output/criterion" "$@" > "$output/output.log" 2>&1 || status=$?
printf '%s\n' "$status" > "$output/exit-code.txt"
cat "$output/output.log"
exit "$status"
