# 2026-10-19 — Child processes kept closed test listeners alive on macOS

- Type: investigation
- Area: `arcbox-core` unit tests (`runtime::tests`, `runtime::kubernetes_lb::tests`, `migration::tests`)
- Commits / PRs: this change
- Related: none

## Problem or trigger

The macOS `Build, Lint, and Test` job failed intermittently on `arcbox-core` tests that assert a listener has closed or a port has come free. The failing tests were `conflicting_sandbox_cleanup_does_not_remove_the_existing_listener` (`Address already in use` on a port the test had just freed), `retiring_system_vm_container_networking_drops_every_trace_of_its_containers` (`the listener must be gone with the VM`), and the `kubernetes_lb` listener tests. These failures blocked the root release PR on 2026-10-07, 2026-10-14 and 2026-10-19, and master CI failed the same way on 2026-10-19. Each time a rerun passed. An earlier note blamed a macOS delay in releasing a closed port.

## What was done

- Measured whether the failure depends on concurrency. The `arcbox-core` lib test binary failed 5 of 40 runs with `--test-threads=16` and 0 of 40 with `--test-threads=1`.
- Sampled the child processes of the test binary during a run: `scutil --proxy` and `ifconfig` from `ProxyEnvironment::detect` (every `Runtime::new` under the default `proxy = "system"` policy), and the `docker` shell shims of the migration tests.
- Reproduced the mechanism outside Rust (Python, the same syscalls). macOS has no `SOCK_CLOEXEC`, so std and mio mark a new socket close-on-exec with a second `fcntl` call. A child that another thread spawns between the two calls inherits the socket and holds it until the child exits.
- Separated the cause from the platform. With no concurrent spawns, 0 of 5000 closed listeners accepted again and 0 of 5000 freed ports refused a rebind. With four threads spawning `ifconfig`, 46 of 3000 closed listeners still accepted a connection and 39 of 5000 freed ports refused a rebind. When every socket existed before the spawns started, 0 of 3000 leaked, so the leak needs a socket created during a spawn.
- `Config::for_test` builds a test configuration with `proxy = "none"`, so a test `Runtime` spawns no probes. Every `Runtime` that `arcbox-core` tests build goes through it.
- `test_support::CHILD_PROCESSES` is a read/write lock. The migration tests that spawn the `docker` shim take the write half; the six tests that watch a socket close take the read half. The write half also replaces the migration tests' `ENV_LOCK`, because those tests already needed exclusive access to `PATH`.

## Evidence

| Configuration | Parallel failures |
|---|---|
| Before, `--test-threads=16` | 5 of 40 |
| Before, `--test-threads=1` | 0 of 40 |
| Proxy probe off only | 1 of 60 (migration shim) |
| Proxy probe off, migration tests skipped | 0 of 60 |
| This change, `--test-threads=16` | 0 of 100 |

All runs used the same `arcbox-core` lib test binary on an M-series Mac, macOS 26, on 2026-10-19.

## Follow-ups

- A production daemon has the same exposure. A child process spawned while a listener is being created can hold that listener past `remove_rule`. The window is one `fcntl` call wide, and the daemon spawns rarely after startup, so no fix is planned.
