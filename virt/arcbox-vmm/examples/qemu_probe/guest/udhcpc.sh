#!/bin/sh
# busybox udhcpc hook: applies the lease and prints it.
case "$1" in
bound|renew)
    echo "PROBE: lease ip=$ip mask=$mask subnet=$subnet router=$router dns=$dns"
    ip addr add "$ip/${mask:-24}" dev "$interface"
    [ -n "$router" ] && ip route add default via "${router%% *}" dev "$interface"
    : > /etc/resolv.conf
    for d in $dns; do echo "nameserver $d" >> /etc/resolv.conf; done
    ;;
esac
exit 0
