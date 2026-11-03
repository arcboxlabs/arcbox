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

`sigterm.sh <data-dir> kill|ready` runs the daemon itself on a
development data directory whose System VM assets are prepared. It boots the
System VM on QEMU, then either `kill -9`s QEMU 0.5 s into the boot or waits
for dockerd, and times the daemon's exit on SIGTERM. (Its former `poweroff`
mode relied on rcS failing without the `arcbox` share, which the daemon now
serves.)

`acceptance.sh <data-dir> [restarts]` is the plan's acceptance run on a Mac
without hardware virtualization: the fallback to QEMU, cold start to dockerd
ready, warm `docker run`, a `python:3.13-slim` pull, `exec` and `logs`, an
amd64 container through FEX, the agent's executable path in the guest, the
host reaching an nginx container at its own IP through the bridge NIC, a
guest reboot, and SIGTERM with no QEMU left after each restart; the
daemon's log must show no route retry. A development daemon gets the bridge
NIC only as root, and its routes go through `arcbox-helper`: start
`sudo -b env ARCBOX_HELPER_SOCKET=/tmp/helper.sock target/debug/arcbox-helper`,
build, then run the script under
`sudo -E env PATH="$PATH" ARCBOX_HELPER_SOCKET=/tmp/helper.sock`. Without
root it reports the missing NIC and skips the container-IP check; as root a
missing NIC fails the run. Set
`ARCBOX_AGENT` to a musl build of this tree's agent to stage it over the
boot assets' one. Without `ARCBOX_QEMU_PATH` the daemon installs the build
`assets.lock` pins, so the run also covers the download. Results:
`docs/logs/2026-11-03-qemu-tcg-system-vm-to-dockerd.md` and
`docs/logs/2026-11-03-qemu-bridge-nic.md`.

`acceptance.sh` also fails a stop when the daemon logged "Server drain
timed out" after that SIGTERM: a System listener that ignores shutdown
holds every stop for the drain's 5 s
(`docs/logs/2026-11-03-kubernetes-proxy-drain.md`).

Both need an arcboxlabs/qemu build (the CPU carries `dic=off`).

Needs: `qemu-system-aarch64` in `ARCBOX_QEMU_PATH` (optional for
`acceptance.sh`), brew's
`FiloSottile/musl-cross/musl-cross` for the guest benchmark, and network
access to fetch Debian's `busybox-static` package (`guest/build.sh` pins its
sha256; when Debian drops that version from its pool, take it from
`snapshot.debian.org`).
