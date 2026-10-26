# 2026-10-23 — E2E finalization requires successful daemon exit

- Type: change
- Area: `tests/e2e`
- Source base: `9eb0f060a9418105ddc786be153d80df5148f545`
- Related: [E2E harness](../../tests/e2e/README.md)

## Problem and change

The boot-assets and shared VZ scenario runners could report passing assertions before observing daemon termination. Dropping a daemon handle discarded its exit status, so a successful test exit did not prove successful daemon shutdown.

These runners now require a successful daemon exit before recording passing metrics. Cleanup and metrics-write errors fail the runner. When both the scenario and finalization fail, the returned error retains the original typed error and adds the finalization failure. Failure paths preserve the test directory.

## Verification

All discovery and execution ran on the dedicated `xuan-work-mbp` test Mac. Development-machine work was limited to source checks, compilation, signing, packaging, and evidence review. The six-file E2E patch has SHA-256 `b53e36e109d25906f1abdab23d410b7c1c57dedaaeeee7dc1f75435823735b19`.

The configured Rust 1.96 formatter and strict Clippy passed. The library harness passed all 26 discovered tests, including the four combinations of scenario and finalization success or failure. The signed, notarized binaries then passed these exact VZ scenarios through devenv:

| Target | Exact test | Result |
| --- | --- | --- |
| `boot_assets` | `boot_assets` | 1 passed, none ignored or filtered |
| `network_fault` | `net_upstream_rst_is_propagated_promptly` | 1 passed, none ignored or filtered |
| `network_workload` | `net_workload_egress_suite` | 1 passed, none ignored or filtered; all 9 workloads passed |

The workload suite covered steady and paused uploads, single-container and multi-container bursts, sequential connection churn, parallel downloads, Docker build networking, and upload/download content integrity. Each daemon reported its actual exit status as 0. Independent post-run checks found no surviving owned daemon or VM process, mounted test filesystem, owned socket, held daemon lock, or open test-directory file. `KEEP_TEST_DIR=1` preserved disks and evidence; retained container metadata was not asserted empty.

Seven native-state checks captured 14 runtime samples. The original ArcTap App, installed NE, VPN session, profile, and core state matched the restored original. Scoped network comparisons passed. The driver performed no native lifecycle action, and released its operation lock.

Evidence is retained at `/private/tmp/arctap-vm-lib-results-oct23-v2` and `/private/tmp/arctap-vm-acceptance-results-oct23-v2`. The VM state SHA-256 is `a349733520e2d901f06f360c14190eef985348b41844fc4e3f810a4295f19224`; the collected log archive SHA-256 is `7f230265200f79a1e4ae0fb39b57ece4fae84e7a64064853ad93208e0d86a5a0`.

## Limits and preserved failures

The first VM wrapper attempt stopped before launching a scenario because its asset list included `.verified.json`. The second attempt copied only the manifest's ten runtime binaries and recorded the known cache separately. The first failure remains in `/private/tmp/arctap-vm-acceptance-results-oct23-v1`; no test assertion changed.

The boot-assets run logged a vmnet start timeout and used `VZNATNetworkDeviceAttachment`. All three runs logged unavailable privileged route installation and a five-second server-drain timeout before aborting remaining tasks. The workload run also logged ICMP `sendto` failures. The successful exits and resource checks do not prove natural draining of every task, bridged host routing, or ICMP correctness. The staged guest had no `vm-agent`, so this suite does not verify sandbox creation. These functional checks are not throughput benchmarks.
