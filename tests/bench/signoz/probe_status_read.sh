#!/bin/bash
set -euo pipefail
mkdir /tmp/proc-read-check
(while :; do for ((busy=0;busy<100;busy++)); do :; done; sleep 0.001; done) &
target=$!
trap 'kill "$target"' EXIT
for mode in direct buffered; do
    for ((sample=0;sample<10000;sample++)); do
        printf 'begin\t%s\t%s\n' "$mode" "$sample"
        if [[ $mode == direct ]]; then
            while IFS= read -r line; do printf '%s\n' "$line"; done < "/proc/$target/status"
        else
            status=$(<"/proc/$target/status")
            while IFS= read -r line; do printf '%s\n' "$line"; done <<< "$status"
        fi
        printf 'end\t%s\t%s\n' "$mode" "$sample"
    done > "/tmp/proc-read-check/$mode.txt"
done
sha256sum /tmp/proc-read-check/*.txt > /tmp/proc-read-check/SHA256SUMS
