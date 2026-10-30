# ADR 0009: Host work must drain before machine teardown

- Status: accepted (2026-10-09); supersedes ADR 0003 decision 5
- Deciders: ArcBox implementation review
- Evidence: [Local domain implementation log](../logs/2026-10-08-custom-local-domains.md); `engine/arcbox-engine/src/machine/tests.rs`

## Context

CA delivery retains a native connection and RPC worker until cancellation finishes. Machine mounts also depend on a live VM while establishing their export. The previous ten-second wait logged a timeout and continued teardown. A stopped record could also bypass that wait during a later removal attempt.

## Decision

1. Admit VM-dependent host work through `MachineManager::host_hold_expected` before its first asynchronous operation. Require the canonical machine to be running the captured physical VM.
2. Close host-work admission before publishing `MachineStopping`. Cancel and join CA delivery before releasing its hold. Mounts retain their hold through the first unmount attempt, as before.
3. Wait at most ten seconds for host holds before stopping or removing the VM. Return a timeout without initiating teardown when holds remain.
4. Apply the same wait to removal of created or stopped records. Recheck the captured physical VM and its state after reacquiring the machine registry lock.
5. Preserve an active graceful stop when a forced takeover fails. Restore admission only after confirming the same physical VM is live.

## Consequences

A stuck host operation can prevent an explicit forced stop or removal. The caller receives an error and can retry after that operation releases its hold. Forced shutdown cannot discard active native work safely.

## Alternatives considered

- Continue teardown after the deadline: invalidates resources still owned by host workers.
- Wait without a deadline: leaves stop and removal callers indefinitely blocked.
