# HV wake-path probes

Small probes for the HV backend's host→guest wake path: how many vCPU
kicks, unparks and interrupts a transfer costs, and what a vsock RPC or a
guest-internal wakeup takes. They drove `docs/adr/0001-hv-spi-is-the-whole-wake.md`
and `docs/experiments/2026-09-30-hv-wake-path.md`; run them against an
isolated dev daemon, never the user's `~/.arcbox` one.

All scripts take the daemon from the environment:
`DOCKER_HOST=unix://<data_dir>/run/docker.sock` and the gRPC socket path
`<data_dir>/run/arcbox.sock` as their first argument where they need it.
`grpcurl`, `jq` and `python3` must be on `PATH`.

| Script | What it measures |
|---|---|
| `snap.sh <sock>` | One JSON line of `GetVirtioDebug` counters: `kick_broadcasts`, `unpark_broadcasts`, per-vCPU `kicks_received` (total and vCPU 0), vsock device interrupts and RX `used_idx`. |
| `pipe1g.sh <sock> <label> [bytes]` | `head -c N /dev/zero \| docker run -i --rm alpine wc -c`, timed, with the counter deltas around it. The reference host→guest vsock bulk test. |
| `rpcping.py <sock> <label> [n]` | `n` `MachineService/Ping` calls over one Connect/JSON connection to the daemon's unix socket; p50/p90/p99/max in ms. Sub-millisecond resolution, unlike `abctl machine ping` (process start dominates). |
| `busy0.sh start\|stop` | Pins a busy loop to guest CPU 0, the CPU Linux routes every virtio SPI to, so the interrupt target is running guest code rather than idle. |
| `ipipong.sh <label> [rounds]` | Guest-internal wake latency: two containers ping-pong over FIFOs, pinned with `--cpuset-cpus` to CPUs 3 and 4 (the peer must be woken by an IPI) and both to CPU 3 (no IPI). µs per round trip. |
| `blknet.sh <sock> <label>` | blk/net-heavy sanity: `docker load` of a 300 MB image ×3 with counter deltas, a `docker build`, a `docker pull`, and the stdin-EOF check. Leaves `bigimg.tar` next to itself; delete it. |

Read the numbers against the host load (`uptime`) printed with them: the
pipe varies ±15% run to run on a loaded host, the RPC p50 by ±0.1 ms, so
compare configurations back to back, not across days.
