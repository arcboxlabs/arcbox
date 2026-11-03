# 2026-11-02 — Does the HV vsock data plane carry the daemon's real traffic to a QEMU TCG guest over vhost-user?

- Type: experiment
- Area: `virt/arcbox-virtio-vsock` (`vhost_user`), `tests/e2e` (`qemu`
  harness); QEMU TCG backend plan, item 1d
- Outcome: works; every check below passes. Found on the way: under TCG,
  code executed straight from a vhost-user-fs share runs stale translations
  (fixed in arcboxlabs/qemu `aa1c259`, Finding 5)
- Probes: `tests/e2e/src/bin/qemu_vsock_e2e/` (real QEMU, real agent, real
  dockerd), `arcbox_e2e::qemu` (the VM harness), unit tests in
  `virt/arcbox-virtio-vsock/src/vhost_user/device.rs`
- Host / guest: Amp Mac satellite, Apple M4 Pro (Virtual), 6 vCPU, 14 GB,
  macOS 26.3.1 (25D2128), otherwise idle. QEMU 10.2.4 from the
  arcboxlabs/qemu CI (`qemu-darwin-arm64-v0.0.0-dev.0169f2b.tar.gz`, sha256
  `53657969…a092aa`), `-accel tcg,thread=multi -cpu
  max,sve=off,sme=off,pauth-impdef=on -smp 4`, 4 GiB of guest RAM on
  `memory-backend-shm,share=on`. Guest: the unmodified System VM from boot
  assets 0.8.12 arm64 (kernel `a122ce7a…913e02c4`, rootfs
  `ceb81067…4b02fa36`), agent built from this branch with Rust 1.96.0
  (`94300fa4…9c4901b2`). NIC: gvproxy v0.8.7 over `-netdev stream`.

## Question

The QEMU backend must give the guest the same host-dialled vsock ports as
HV — 1024 agent RPC, 2375 Docker API, 16443 Kubernetes, 2049 NFS relay, 2201
SSH-auth relay — without changing the guest. The plan serves them from the
daemon over `vhost-user-vsock-pci`, reusing the HV `VirtioVsock` data plane
and `VsockConnectionManager` under the vhost-user framework's event loop
(ADR 0010). The framework's own experiment
([vhost-user on macOS](2026-11-02-vhost-user-on-macos.md)) only saw the
guest driver bind and post RX buffers. Unknown: whether that data plane
carries the daemon's real clients — `AgentClient`, the `HalfCloseStream`
Docker channel and the Docker CLI's streaming and half-close cases —
under load, when the guest runs out of RX buffers, and across a guest
reboot.

## Hypotheses

1. The HV data plane runs unchanged on vhost-user rings when a pass binds
   the current memory table, drains TX, injects RX and hands the avail
   cursor back with `set_last_avail_idx`.
2. Host connects and readable sockets need no I/O thread: a framework Waker
   as the manager's doorbell plus edge-triggered readability of each host
   socket is enough, and RX starvation recovers on the guest's RX kick.
3. A guest reboot (`RESET_DEVICE`) ends every host connection with EOF, and
   the re-initialized driver is served on the same back-end.

## Method

`abctl boot prefetch --data-dir ~/qv-data`, then:

```text
cargo run --release -p arcbox-e2e --bin qemu_vsock_e2e -- \
    --qemu ~/qemu-dl/qemu/bin/qemu-system-aarch64 --data-dir ~/qv-data \
    --agent target/aarch64-unknown-linux-musl/release/arcbox-agent \
    --gvproxy ~/gvp/gvproxy
RUST_LOG=info,arcbox_virtio_vsock::vhost_user=debug,arcbox_vhost_user::backend=debug  # parks, reset trace
```

The probe raises its descriptor limit (see Findings 4), boots the VM and
runs, in order: Ping readiness, `GetSystemInfo`, 200 connect+Ping; 512
connections opened (≤ 64 handshakes in flight) and pinged simultaneously;
512 connects released at once; 100 host closes before the handshake and 100
after an RPC; `EnsureRuntime`, then the Docker CLI through a Unix socket
relayed onto vsock 2375 with `HalfCloseStream`, as the daemon relays it;
bulk transfers with the probe's and QEMU's CPU time; killing the agent from
the debug console; `busybox reboot`; Ping and Docker again. Each check
asserts its result (byte counts, output, `connection_count() == 0`). The
numbers below are from the last of three passing runs; the other two were
within 5 % on throughput and 20 % on p50 latency. The control run adds
`--exec-from-share`.

The table was measured with `--cpu max,sve=off,sme=off,pauth-impdef=on`.
The probe now defaults to `max,pauth-impdef=on`, SVE and SME on, the CPU
model chosen for the QEMU backend. Two runs with it passed every check:
connect+Ping p50 1.66 and 2.09 ms, `_ping` p50 4.62 and 4.88 ms,
host→guest 16.9 and 21.1 MiB/s, guest→host 10.5 and 9.4 MiB/s, 16 streams
32.7 and 32.4 MiB/s. That shows no consistent difference from the table.

Rerun on 2026-11-03, rebased on master `e97ddc89`, with QEMU
`qemu-darwin-arm64-v0.0.0-dev.aa1c259.tar.gz` (sha256 `c2640cab…7799e4`)
and `--cpu max,pauth-impdef=on,dic=off`: every check passed, connect+Ping
p50 1.61 ms, `_ping` p50 5.73 ms, host→guest 21.9 MiB/s, guest→host
9.1 MiB/s, 16 streams 31.9 MiB/s. The same run with `--exec-from-share`
also passed every check.

## Results

| Check | Result |
|---|---|
| First Ping after QEMU starts | 2.0 s |
| 200 connect+Ping, fresh connection each | p50 1.68 ms, p99 2.34 ms |
| 512 open connections, 512 simultaneous Pings | 0.48 s; p50 89 ms, p99 197 ms |
| 512 connects released at once | 179 answered, 333 reset by the guest; 0 left open |
| Host close: 100 before the handshake, 100 after an RPC | agent answers; 0 left open |
| `EnsureRuntime` → dockerd 29.9.2 answers on 2375 | 5.2 s |
| `docker pull alpine:3.20` (removed first) | downloaded, 1.7 s |
| `run --rm echo`, `run -i` from a pipe, `exec -i` 1 MiB, `logs -f` to exit, `logs -f` killed | all correct; stdin EOF reaches the container |
| 100 `GET /_ping`, fresh connection each | p50 5.35 ms, p99 9.55 ms |
| Host→guest 256 MiB, `docker exec -i … wc -c` | 21.8 MiB/s; back-end+proxy 9 % CPU, QEMU 270 % |
| Guest→host 256 MiB, `docker exec … head -c` | 10.2 MiB/s; back-end+proxy 10 %, QEMU 317 % |
| 16 concurrent host→guest streams × 16 MiB | 32.8 MiB/s; back-end+proxy 17 %, QEMU 359 % |
| RX passes parked on an empty guest ring | 6 010, every one resumed |
| Agent killed under an idle connection | host EOF 0.52 s; respawned agent 1.7 s later |
| `busybox reboot` under an idle connection | host EOF 0.51 s; `GET_VRING_BASE` ×2, `RESET_DEVICE`, `SET_MEM_TABLE` (same layout), rings restarted; Ping 5.1 s later, dockerd again |
| Control: agent executed from the share | no answer in 300 s; the agent crashes before serving |

## Findings

1. Hypotheses 1–3 hold. The data plane is the HV one; nothing in
   `VirtioVsock` or the manager changed beyond `unbind_ctx`, `remove_all`
   and making `len`/`is_empty` public.
2. The bulk numbers are bounded by the emulated guest, not the vsock path:
   QEMU's vCPU threads take 2.7–3.6 cores while the back-end and the
   Docker relay together take 0.1–0.2. Why guest→host is half the
   host→guest rate was not measured.
3. The agent's listen backlog is 128 (`arcbox-transport` `vsock/linux.rs`);
   a burst past it is reset by the guest kernel, and the device cleans up
   every refused connection. This is a guest property and applies to HV.
4. Every host connection holds two descriptors in the back-end's process,
   the daemon's end and the device's end, so 512 concurrent connections
   need a soft `RLIMIT_NOFILE` far above macOS's default of 256; the probe
   raises it, as the daemon does at startup
   (`app/arcbox-daemon/src/fd_limit.rs`). The first runs here also hit a
   ~64-connection cap in the fd placement shared with HV (host port clamped
   to the limit − 64), which master fixed in `93c272fa`;
   `relocate_daemon_end` carries that fix for both backends, and the
   512-connection step passes on it.
5. Under TCG, code executed straight from a vhost-user-fs share runs stale
   translations. TCG drops translated blocks only when its softmmu sees a
   store to the page; in system mode `IC IVAU` is `ARM_CP_NOP`
   (`target/arm/helper.c`) and `-cpu max` reports `CTR_EL0.DIC/IDC`
   (`target/arm/tcg/cpu64.c`). The back-end fills page-cache pages through
   shared memory, which QEMU never sees. The file bytes are correct
   (sha256 matches); the agent crashes with corrupt-heap symptoms (failed
   16-byte allocation, libunwind `truncated uleb128`, tokio assertions).
   Seen with no vsock traffic, with 2 and 4 GiB of RAM, with single-threaded
   TCG, and with `-cpu max` and `neoverse-n1` (which reports `DIC = 0`, so
   Linux issues `IC IVAU`, which QEMU ignores). The harness
   copies the agent into a guest tmpfs before executing it (`AGENT_LAUNCHER`
   in `arcbox_e2e::qemu`); dockerd and the runtime already execute from the
   Btrfs data disk (virtio-blk, QEMU's own DMA), so only the agent needed it.
   arcboxlabs/qemu `aa1c259` fixes it: with that build and `dic=off`,
   `--exec-from-share` passes every check, the agent starting from the
   share at boot, after the kill and after the reboot (2026-11-03).
6. The guest resolver forwards to the ArcBox gateway `10.0.2.1:53`; gvproxy
   serves 192.168.127.0/24 and cannot be moved. The probe DNATs that
   address in the guest after boot. The QEMU network task has to provide the
   10.0.2.0/24 layout.

## Decisions taken / open

- `VhostUserVsock` (`arcbox_virtio_vsock::vhost_user`) is the entry point
  for the QEMU backend: `spawn(cid, UnixStream)`, `connect_vsock(port)`.
- Closed 2026-11-03: the harness dropped `AGENT_LAUNCHER` and
  `--exec-from-share`. The agent always runs from the share, and `--cpu`
  defaults to the daemon's `max,pauth-impdef=on,dic=off`, so the probe
  needs arcboxlabs/qemu.
- Not exercised over real traffic here: 16443, 2049 and 2201 (same
  connect path as 1024 and 2375).
