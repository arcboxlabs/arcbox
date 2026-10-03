#!/bin/sh
# Times one getaddrinfo lookup of a fresh name under DOMAIN through the
# system resolver (dscacheutil -> mDNSResponder). Usage: lookup.sh <label> [domain]
# A fresh random name per call keeps mDNSResponder's cache out of the number;
# jot ships with macOS, which is the only platform this probe makes sense on.
set -eu
label=$1
domain=${2:-arcbox.local}
name="$label-$$-$(jot -r 1 1000 9999).$domain"
start=$(python3 -c 'import time; print(time.monotonic())')
out=$(dscacheutil -q host -a name "$name" 2>&1 || true)
end=$(python3 -c 'import time; print(time.monotonic())')
addr=$(printf '%s\n' "$out" | awk '/ip_address|ipv6_address/ { printf "%s ", $2 }')
printf '%-14s %6.2fs %s\n' "$label" "$(echo "$end - $start" | bc)" "${addr:-no address}"
