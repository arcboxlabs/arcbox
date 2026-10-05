# ArcBox Storage

This crate records the persistent identity of the System VM's Btrfs data image and ext4 metadata image. It loads, durably saves, and validates paired manifests, and verifies host file identities and filesystem UUIDs without changing either image.

```rust,no_run
use std::path::Path;

let directory = Path::new("data");
let manifest = arcbox_storage::StorageManifest::load(
    &directory.join("docker.storage.json"),
)?;
manifest.verify_images(directory)?;
# Ok::<(), arcbox_storage::Error>(())
```

The manifest records both image basenames, host file identities, filesystem UUIDs, and initialization state. A `New` image must have its matching provisioning header. `Formatting` and `Ready` images must contain the recorded filesystem UUID. Missing, replaced, unreadable, or mismatched images require recovery.

A filesystem signature alone never authorizes formatting. Do not clear a manifest or change its state to bypass recovery.
