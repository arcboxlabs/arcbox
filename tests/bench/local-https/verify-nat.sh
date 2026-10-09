#!/bin/sh
set -eu

test "${ARCBOX_TEST_ISOLATED_NAT:-}" = 1
for interface in /sys/class/net/*; do
    test "${interface##*/}" = lo
done

for tool in iptables iptables-save iptables-restore ip; do
    command -v "$tool"
done
/sbin/iptables --version
iptables --version
iptables-save --version
iptables-restore --version
ip -Version
uname -r
readlink /proc/self/ns/net
ip -brief link show

/sbin/iptables -w 2 -t nat -S > /tmp/nat-before.rules
while IFS= read -r rule; do
    case "$rule" in
        '-P '*) ;;
        *) printf '%s\n' 'NAT must be empty before validation.' >&2; exit 1 ;;
    esac
done < /tmp/nat-before.rules
iptables-save -t nat > /tmp/nat-before.save
trap 'iptables-restore -w 2 < /tmp/nat-before.save' EXIT

/sbin/iptables -w 2 -t nat -I PREROUTING 1 -d 192.0.2.123/32 -p tcp --dport 443 -m comment --comment arcbox-validation:https -j REDIRECT --to-ports 18443
/sbin/iptables -w 2 -t nat -I PREROUTING 1 -d 192.0.2.123/32 -p tcp --dport 80 -m comment --comment arcbox-validation:http -j DNAT --to-destination 192.0.2.123:18080
/sbin/iptables -w 2 -t nat -C PREROUTING -d 192.0.2.123/32 -p tcp --dport 443 -m comment --comment arcbox-validation:https -j REDIRECT --to-ports 18443
/sbin/iptables -w 2 -t nat -C PREROUTING -d 192.0.2.123/32 -p tcp --dport 80 -m comment --comment arcbox-validation:http -j DNAT --to-destination 192.0.2.123:18080
/sbin/iptables -w 2 -t nat -S PREROUTING
iptables-save -t nat > /tmp/nat-active.save

/sbin/iptables -w 2 -t nat -D PREROUTING -d 192.0.2.123/32 -p tcp --dport 443 -m comment --comment arcbox-validation:https -j REDIRECT --to-ports 18443
/sbin/iptables -w 2 -t nat -D PREROUTING -d 192.0.2.123/32 -p tcp --dport 80 -m comment --comment arcbox-validation:http -j DNAT --to-destination 192.0.2.123:18080
/sbin/iptables -w 2 -t nat -S > /tmp/nat-deleted.rules
cmp /tmp/nat-before.rules /tmp/nat-deleted.rules

iptables-restore -w 2 < /tmp/nat-active.save
/sbin/iptables -w 2 -t nat -C PREROUTING -d 192.0.2.123/32 -p tcp --dport 443 -m comment --comment arcbox-validation:https -j REDIRECT --to-ports 18443
/sbin/iptables -w 2 -t nat -C PREROUTING -d 192.0.2.123/32 -p tcp --dport 80 -m comment --comment arcbox-validation:http -j DNAT --to-destination 192.0.2.123:18080
iptables-restore -w 2 < /tmp/nat-before.save
/sbin/iptables -w 2 -t nat -S > /tmp/nat-restored.rules
cmp /tmp/nat-before.rules /tmp/nat-restored.rules
trap - EXIT
printf '%s\n' 'PASS: legacy NAT insert, check, list, delete, save, and restore; original rules restored.'
