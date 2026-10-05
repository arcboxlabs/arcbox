# ArcBox Engine

ArcBox Engine owns machine and VM lifecycles without a daemon dependency.

Use `VmLifecycleManager::force_stop` to interrupt a boot or graceful shutdown and wait for machine removal:

```rust,ignore
lifecycle.force_stop().await?;
```

During removal, the lifecycle state is `Stopping`. Concurrent `force_stop` and `shutdown` calls share the removal result. A readiness request received during removal can start the VM only after removal succeeds.

Successful removal sets `NotExist` and publishes `MachineStopped`. An already absent machine also succeeds. Removal failure sets `Failed`, returns the error to stop and readiness waiters, and publishes no `MachineStopped` event.

Use `MachineManager::reserve_storage` to exclude normal System VM mutations during storage maintenance. The reservation lasts until its last clone is dropped. A durable `storage-recovery/hold` also blocks normal admission after a manager restart.

Reserve storage before stopping workloads. `shutdown` remains available to stop the VM; normal `ensure_ready` and `force_stop` calls are rejected while storage is held. `resume_storage` accepts only a reservation from the same `MachineManager` and starts the existing stopped machine.

```rust,ignore
let reservation = machines.reserve_storage()?;
lifecycle.shutdown().await?;
// Preserve and verify the stopped images before restarting.
lifecycle.resume_storage(&reservation).await?;
// Verify filesystem and runtime writes before releasing protection.
drop(reservation);
```

The recovery owner must retain any durable hold after failure. Remove the durable hold only after write verification succeeds. The engine reservation does not repair filesystems or verify runtime writes.

Use `AgentClient::storage_check` on a dedicated connection to a running guest. The client requires agent protocol v7 before sending the request.

Dropping the async future closes the connection. The guest contract cancels offline checks and retains online Docker cleanup after disconnect. Interrupting `storage_check_blocking` requires the caller to stop the recovery VM.

A successful RPC returns the guest's check results. Callers must require `StorageCheckResponse.passed` before treating verification as successful. The client does not remove storage protection or start runtime services.
