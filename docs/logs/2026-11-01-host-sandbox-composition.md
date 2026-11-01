# 2026-11-01 — The daemon runs host sandboxes, HV by default

- Type: change
- Area: `app/arcbox-host-sandbox`, `app/arcbox-daemon` (`host_sandboxes`),
  `app/arcbox-core`, `app/arcbox-api` (`connect/sandbox_route.rs`),
  `computer/arcbox-sandbox-service`, `computer/arcbox-computer-runtime`,
  `engine/arcbox-snapshot`, `tests/e2e` (`host_sandbox`)
- Commits / PRs: host sandboxes workstream W8, branch `hs-w8-daemon`
- Related: [ADR 0010](../adr/0010-sandboxes-run-on-the-host.md),
  [host sandboxes design](../architecture/host-sandboxes.md),
  [plan](../plans/host-sandboxes-v1.md) W8,
  [HV driver log](2026-11-01-hv-driver-process-per-vm.md)

## Problem or trigger

Wave 1 delivered the drivers, the host network, clonefile CoW and the
runtime's host placement, but nothing composed them: the Sandbox API
refused `RUN_LOCATION_HOST` and reported the location as a placeholder.

## What was done

- The Sandbox API service (frame dispatch, executions, files, templates)
  moved out of the guest agent into `computer/arcbox-sandbox-service`,
  behind a `SandboxHooks` seam. The guest agent composes it over guest DNS
  and DNAT, and the daemon composes it over the host.
- `app/arcbox-host-sandbox` composes one host location: HV
  (`HvDriver`, `DatapathPlacement::Driver`) by default, or VZ
  (`VzDriver`, `DatapathPlacement::Network`, network port forwards, and a
  checkpoint gate on `checkpoint_blocker`). The address pool is
  `10.200.0.0/16`, the CoW backend is `Clonefile` under
  `<data_dir>/sandbox`, and the kernel is the boot assets' kernel.
- The daemon composes it at host admission (`[sandbox] backend`,
  `ARCBOX_SANDBOX_BACKEND`; `[sandbox] enabled = false` skips it). The
  Sandbox API routes an existing id to the location that owns it, and a
  new create, template or catalog call to the host, its default (D2).
  `GetCapabilities.locations` reports the host location's answer.
- Packaging builds, signs and ships `arcbox-hv-vmm` next to the daemon.
- D5: `docker:<ref>` pulls from the registry first and converts the image
  on the host. When the pull fails and the System VM's dockerd is
  ready, the image is exported from that dockerd instead.
- The guest clock push is awaited after every host boot, restore and
  resume. The warm key includes the macOS build.
- Fixes found by the first real boots:
  - The catalog committed only Firecracker's `vmstate` + `mem`, so every
    VZ and HV checkpoint failed with ENOENT after the driver had written
    it.
  - The spec always asked for dirty tracking, which VZ and HV refuse.
  - HV's blocking-mode vsock descriptors were refused by the tokio-only
    agent client. They are now relayed.
  - The ready gate expected the guest's dial-out, which neither host
    driver listens for. It is now a retried agent round trip.
  - Template warm restores required the jailer.

## Evidence

All runs used isolated `/tmp` data dirs, an unlocked session, and the
release build of this branch on an Apple Silicon Mac.

`host_sandbox` e2e (`--no-linux-vm`; create from
`docker:public.ecr.aws/docker/library/alpine:latest`, exec, Pause,
Resume, a file written before the pause survives, guest clock within
60 s, Remove):

| Backend | Result | sandbox_ready | pause | resume |
|---------|--------|---------------|-------|--------|
| VZ | pass | 7.66 s (includes the registry pull) | 95 ms | 167 ms |
| HV, before W2's vsock port fix | fails on the first exec after the warm capture (see follow-ups) | — | — | — |
| HV, rebased onto master with `4d4b2f06` | pass, warm cache on | — | — | — |
| HV, warm cache off and 10 s idle before Pause | pass | — | — | — |

SDK suites with `ARCBOX_E2E_SANDBOX_LOCATION=host`:

| Suite | VZ | HV |
|-------|----|----|
| Python (`tests/test_e2e.py`, 7 tests) | 7 pass | 6 pass; `test_sync_phase_2b_surface` hits the vsock EOF below |
| TypeScript (`test/e2e.test.ts`, 5 tests) | 5 pass | 2 pass; the 2b-surface and template tests hit the same vsock EOF, and the idle auto-pause test times out at 300 s |

## Follow-ups

- HV, VMM vsock (outside this change): after an HV restore,
  `VsockConnectionManager` starts host ports at 50000 again. The guest
  still holds its pre-checkpoint sockets on those ports: they were closed
  by a guest SHUTDOWN that the host never answered with RST. So the first
  dial after a restore is reset, and the agent client reads EOF. This
  breaks every warm create, and any resume within about 8 s of the last
  pre-pause connection.
  Fixed by `4d4b2f06` (the vsock device carries its next host port in
  the checkpoint); after rebasing onto it, `host_sandbox` passes on HV
  with the warm cache on. The SDK suites were not rerun on HV.
- HV, VMM vsock: a guest connect to a host port with no listener is not
  refused, so vm-agent's ready dial-out blocks for its 2 s connect timeout
  on every HV cold boot.
- HV control socket: `<data_dir>/sandbox/sandboxes/<id>/vmm.sock` must fit
  in `sun_path` (104 bytes). A long data dir or sandbox id fails the boot.
  The driver should report an `id_budget`.
- The host ships no static Linux busybox, so the built-in template (`""`)
  cannot be built on the host. The SDK harness stages it from the busybox
  image; the product needs a shipped busybox (boot assets) or another
  built-in template source.
- `<id>.sandbox.arcbox.local` for a host sandbox: decided (c), the
  10.200/16 pool becomes routable through a helper-installed route into a
  daemon utun forwarded to each VM's datapath. Until then the name is not
  published and answers NODATA (d), rather than pointing at an address
  nothing routes to.
