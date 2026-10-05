# ArcBox Storage

This crate owns persistent identity and formatting authority for the System VM's Btrfs data image and ext4 metadata image. The host provisions and verifies images. The guest verifies filesystem identity and consumes a new image's formatting authority.

```rust,no_run
use std::path::Path;

let manifest = arcbox_storage::prepare_pair(
    Path::new("data/docker.img"),
    Path::new("data/docker-meta.img"),
    8 * 1024_u64.pow(4),
    2 * 1024_u64.pow(3),
)?;
manifest.verify_images(Path::new("data"))?;
# Ok::<(), arcbox_storage::Error>(())
```

The manifest records both image basenames, host file identities, filesystem UUIDs, and initialization state. `New` authorizes formatting only when the attached block device has the matching provisioning header. The guest persists `Formatting` before invoking the formatter with the recorded UUID. The guest then verifies the UUID and persists `Ready`. A failed format cannot regain authority. After an interruption, a matching filesystem can advance from `Formatting` to `Ready` without another format attempt; an unrecognized filesystem requires recovery.

`prepare_pair` uses exclusive creation for new images. Existing images with unknown signatures are preserved. A recorded pair with a missing or replaced member is rejected. Valid older pairs can be adopted. A Btrfs-only installation enters `LegacyMigration`; the guest must verify populated original metadata before advancing to `Migrating`. A missing metadata image after prior migration cannot silently produce empty Docker state. `Paired` records that both filesystems and their metadata mappings are ready.

Preserve both stopped images and the manifest before recovery. `rebind_images` verifies a recovery copy against the recorded filesystem UUIDs before replacing its host file identities. The caller must save that manifest in the copy's directory. A filesystem signature alone never authorizes formatting. Do not clear a manifest or change its state to bypass recovery.
