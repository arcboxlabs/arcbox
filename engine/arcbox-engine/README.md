# ArcBox Engine

ArcBox Engine owns machine and VM lifecycles without a daemon dependency.

Use `VmLifecycleManager::force_stop` to interrupt a boot or graceful shutdown and wait for machine removal:

```rust,ignore
lifecycle.force_stop().await?;
```

During removal, the lifecycle state is `Stopping`. Concurrent `force_stop` and `shutdown` calls share the removal result. A readiness request received during removal can start the VM only after removal succeeds.

Successful removal sets `NotExist` and publishes `MachineStopped`. An already absent machine also succeeds. Removal failure sets `Failed`, returns the error to stop and readiness waiters, and publishes no `MachineStopped` event.
