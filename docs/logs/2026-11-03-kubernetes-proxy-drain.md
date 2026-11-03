# 2026-11-03 — Every daemon stop waited 5 s for the Kubernetes API proxy

- Type: change
- Area: `arcbox-daemon` (`kubernetes_proxy`, `shutdown`), `tests/bench/qemu-tcg`
- Commits / PRs: this change
- Related: [QEMU System VM to dockerd](2026-11-03-qemu-tcg-system-vm-to-dockerd.md) (its follow-up on the 5 s drain), probe [`acceptance.sh`](../../tests/bench/qemu-tcg/acceptance.sh)

## Problem or trigger

In the QEMU acceptance run, SIGTERM to daemon exit took 7.29–7.35 s while
the guest's own shutdown took about 2.3 s, and every stop logged "Server
drain timed out after 5s, aborting remaining tasks".

## What was done

`shutdown::drain_system` awaits the System listeners under `DRAIN_TIMEOUT`
(5 s): the Docker API server, the Kubernetes API proxy and the container
route guard. Which one held it:

- The Docker API server logs "Docker API server drained" within a
  millisecond of the signal.
- The route guard (`services::container_route_guard`) selects on the
  shutdown token in its loop.
- The Kubernetes API proxy did not: `KubernetesProxy::start` spawned
  `loop { listener.accept().await … }` with no shutdown token, so the task
  ended only when the drain timed out and aborted it. The proxy joined the
  drain with k3s support (`f268d883`, 2026-03) and moved into
  `drain_system` in `6cdeeb6d`.

The accept loop now selects on the shutdown token and closes its listener;
connections already forwarding end on their own, as before. The regression
test `kubernetes_proxy::tests::cancelled_proxy_stops_and_releases_its_port`
fails without the select (the task never ends). `acceptance.sh` now fails a
stop whose log has a drain timeout.

## Evidence

Host: Amp Mac satellite, macos-26, `kern.hv_support` absent, System VM on
QEMU v0.1.0, debug daemon. `acceptance.sh` on fresh data directories:

| Build | Runs | SIGTERM to daemon exit | "Server drain timed out" |
|---|---|---|---|
| Without the fix | 2 stops | 7.31, 7.36 s | 2 of 2 stops; "Docker API server drained" at 14:20:20.536, timeout at 14:20:25.539 |
| With the fix, as the user | 4 stops | 2.41, 2.30, 2.30, 2.28 s | 0 |
| With the fix, as root (vmnet bridge NIC up) | 4 stops | 2.45, 2.29, 2.31, 2.29 s | 0 |

With the fix the log shows "Kubernetes API proxy stopped" within 0.2 ms of
"Shutdown signal received", next to "Docker API server drained". What
remains of a stop is the guest's own shutdown.

VZ and HV, inferred from the code and not run (this Mac has no hardware
virtualization): the proxy binds whenever the Linux VM is enabled (16443 by
default, best-effort; `--kubernetes-port` must bind), and the drain does
not depend on the backend. So every stop on every backend whose proxy had
bound has paid the same 5 s since `f268d883`.
