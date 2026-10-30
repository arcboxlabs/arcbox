# ADR 0007: Local HTTPS authorization is independent of startup

- Status: accepted (2026-10-08)
- Supersedes: the startup gate in [ADR 0006](0006-authorize-local-ca-in-user-session.md)
- Evidence: [Native authorization and restart checks](../logs/2026-10-08-helper-ca-trust.md)

## Context

Requiring CA trust before VM boot prevents ordinary Docker use when a user cancels HTTPS authorization. OrbStack requests certificate installation on first HTTPS use. The private root remains a daemon-owned resource; native trust changes still require the login session.

## Decision

1. Prepare the private identity without changing trust. Disable Keychain UI in the daemon process.
2. Report HTTPS trust through `SetupStatus.local_https`. Preserve VM and Docker readiness when HTTPS trust is absent or inspection fails.
3. Keep removal of an exposed private root key as a required barrier before sharing the data directory.
4. Preserve a trusted previous public root until an explicit login-session action revokes that exact certificate. Publication must not request authorization.
5. Let the login-session CLI publish the current public root after authorization. The CLI must not create or use the private root key.
6. Prepare and recover the identity in the background every five seconds, then inspect trust without authorization. Publish status changes and cancel the monitor at daemon shutdown.
7. Select the current published identity when a CSR arrives. Preserve the same signer handle for an unchanged root and reject signatures completed after that handle changes.

## Consequences

Existing trusted profiles continue without authorization. An untrusted profile can run containers and HTTP services. Native cancellation affects the requested HTTPS action only. Desktop service entry points and first-use browser authorization consume the independent state; their end-to-end verification remains required before feature completion.
