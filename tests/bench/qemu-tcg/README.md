# QEMU TCG backend probes

`run.sh` drives the `qemu_probe` example (`virt/arcbox-vmm/examples/qemu_probe/`)
on a Mac. The example boots a busybox initramfs on the production `Vmm`
with the QEMU backend: same command line, NIC socketpair and host network
stack, console pipes, QMP session and pidfile as the System VM, with the
ArcBox kernel and `rootfs.erofs` from the boot-asset cache. The guest's
`/init` (`guest/init`) runs the mode named by `probe=` and prints `PROBE:`
lines on `hvc0`. Results: `docs/logs/2026-11-02-qemu-tcg-host-backend.md`.

| Section | What it shows |
|---|---|
| devices, network and consoles | `vda`/`vdb`/`vdc` order, read-only rootfs and PCI slots; a guest `blkdiscard` returning blocks of the sparse `docker.img`; a DHCP lease, DNS and HTTP through the daemon's network stack; output on `hvc1`; the QMP `guest-shutdown` |
| guest reset | QMP `guest-reset`, `reset_requested`, `Vmm::reboot` and the new boot's consoles |
| host stop | `Vmm::stop` of a running guest through QMP `quit`, pidfile removed |
| orphan | QEMU outliving a `kill -9`ed owner, then `reap_orphaned_qemu` stopping it |
| speed | `guest/bench.c` natively and under TCG at 4 and 12 vCPUs |
| dockerd by memory | dockerd from exec to `API listen on` at 2–14 GiB |
| host memory | host free memory while the guest touches 3 GiB and frees it |

`sigterm.sh <data-dir> poweroff|kill` runs the daemon itself on a
development data directory whose System VM assets are prepared. It boots the
System VM on QEMU, lets QEMU go away (the guest powering off at rcS, or a
`kill -9` of QEMU), and times the daemon's exit on SIGTERM.

Needs: `qemu-system-aarch64` in `ARCBOX_QEMU_PATH`, brew's
`FiloSottile/musl-cross/musl-cross` for the guest benchmark, and network
access to fetch Debian's `busybox-static` package (`guest/build.sh` pins its
sha256; when Debian drops that version from its pool, take it from
`snapshot.debian.org`).
