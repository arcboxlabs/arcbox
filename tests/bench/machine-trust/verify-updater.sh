#!/bin/sh
# Run only inside a disposable Linux container with two public CA PEM fixtures.
set -eu
old=${1:?first public CA PEM is required}
new=${2:?replacement public CA PEM is required}
anchor=/usr/local/share/ca-certificates/arcbox-local-ca.crt
bundle=/etc/ssl/certs/ca-certificates.crt
test ! -e "$anchor"
test ! -L "$anchor"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
/usr/sbin/update-ca-certificates
cp "$bundle" "$work/baseline.pem"
marked() {
    printf '\n# BEGIN ARCBOX LOCAL CA\n'
    cat "$1"
    printf '# END ARCBOX LOCAL CA\n'
}
assert_bundle() {
    awk '/^# BEGIN ARCBOX LOCAL CA$/ { in_ca = 1; next } /^# END ARCBOX LOCAL CA$/ { in_ca = 0; next } in_ca { print }' "$bundle" > "$work/installed.pem"
    cmp "$1" "$work/installed.pem"
}
marked "$old" > "$anchor"
/usr/sbin/update-ca-certificates
assert_bundle "$old"
/usr/sbin/update-ca-certificates
assert_bundle "$old"
marked "$new" > "$anchor"
/usr/sbin/update-ca-certificates
assert_bundle "$new"
rm "$anchor"
/usr/sbin/update-ca-certificates
cmp "$work/baseline.pem" "$bundle"
printf 'PASS: install, source rebuild, rotation, removal; baseline restored exactly\n'
cat /etc/os-release
