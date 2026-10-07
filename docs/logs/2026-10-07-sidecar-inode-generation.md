# 2026-10-07 — Side entries distinguish reused inode generations

- Type: investigation
- Area: arcbox-agent machine export
- Commits / PRs: runtime PR #734
- Related: [Storage recovery I/O fault experiment](../experiments/2026-10-05-storage-recovery-io-fault.md)

## Problem or trigger

The Linux unit test `a_side_entry_of_a_recreated_file_is_dropped` failed because a new file inherited the previous file's resource fork. ext4 can reuse an inode while the creation timestamp remains unchanged. The previous identity combined those values, so the side entry appeared to belong to the new file. Modification timestamps cannot identify a file because ordinary edits change those timestamps.

## What was done

Side-entry version 2 stores the opaque type and bytes returned by `name_to_handle_at` with `AT_HANDLE_FID`. The handle includes the inode generation, survives ordinary edits and renames, and identifies a symlink without following the link. `AT_HANDLE_FID` supports overlayfs without enabling NFS export or changing mount options. Mount IDs are not persisted because mount IDs can change across boots.

The native call requires Linux 6.5 or newer and filesystem file-handle support. Kernel tag `v0.0.25` builds Linux 6.18.38. Both ArcBox kernel configurations enable `CONFIG_EXPORTFS` and overlayfs; Linux enables `CONFIG_FHANDLE` by default. Native errors, including an oversized handle, propagate without a timestamp fallback.

Unsupported versions return `Unsupported` before either replacement or merge changes inline attributes. The original side entry remains available for recovery. The agent does not migrate version 1 because its identity cannot distinguish reused inodes. Malformed current-format records retain the existing removal behavior. Earlier runtimes remove unknown versions, so preserve `.arcbox-xattrs` before downgrading.

## Evidence

The original recreation assertion remains unchanged. The complete sidecar suite passes on both ext4 and default overlayfs in a Linux 6.8 aarch64 guest. The suite covers content and permission changes, regular-file and symlink recreation, renames, and unsupported-version preservation for reads, replacement, and merge. Host-only tests retain the prior macOS identity implementation; those tests do not validate Linux file handles.

Build the Linux agent test binary, then run the checked-in driver as root on a guest whose `/tmp` is ext4:

```sh
cargo test --locked -p arcbox-agent --bin arcbox-agent --no-run
sudo sh tests/bench/sidecar-identity/run.sh /absolute/path/to/arcbox-agent-test-binary
```

The driver runs the same tests on ext4 and a temporary overlayfs mount. The driver removes the mount and temporary files after the tests.

Native contract: [name_to_handle_at(2)](https://man7.org/linux/man-pages/man2/open_by_handle_at.2.html). Kernel configuration: [Linux v6.18 FHANDLE default](https://github.com/torvalds/linux/blob/v6.18/init/Kconfig).

## Filesystem scope follow-up

Two newly formatted ext4 roots returned the same handle: type 1, inode 2, generation 0. A side entry beside a mount point belongs to the parent filesystem, so replacing the mounted volume could reuse that side entry. The mounted-root regression failed with handle-only identity and passed after adding the filesystem ID.

The final format is version 3. The identity combines `fstatfs().f_fsid` with the file handle. Both calls use one `O_PATH | O_NOFOLLOW` descriptor, so the filesystem ID describes the same object as the handle, including a symlink itself. Versions 1 and 2 remain untouched and return `Unsupported`.

The native filesystem ID remains stable for the supported mounts:

- [ext4](https://github.com/torvalds/linux/blob/v6.18/fs/ext4/super.c) derives the ID from the filesystem UUID.
- [btrfs](https://github.com/torvalds/linux/blob/v6.18/fs/btrfs/super.c) combines the filesystem UUID with the subvolume ID.
- [overlayfs](https://github.com/torvalds/linux/blob/v6.18/fs/overlayfs/super.c) uses its persistent UUID when available, otherwise the real root filesystem ID. [UUID initialization](https://github.com/torvalds/linux/blob/v6.18/fs/overlayfs/util.c) reads the saved upper-directory UUID before generating a new one. Remounting the same upper directory preserves that UUID.

The driver also runs two privileged tests with disposable images. The mounted-root test verifies ext4 and btrfs volume replacement, preservation after remounting the original volume, and symlink identity when the linked filesystem changes. The overlay test verifies lower-file copy-up and preservation after unmounting and remounting the same upper directory. Both tests pass in the Linux 6.8 guest. Neither the mount ID nor `st_dev` is persisted.

The static musl run also exposed an API difference: `OpenOptions` removes `O_ACCMODE` from custom flags, and musl includes `O_PATH` in that mask. The resulting `O_NOFOLLOW` open rejected symlinks with `ELOOP`. Native `nix::fcntl::open` preserves `O_PATH`; `OwnedFd` owns the descriptor. The regression driver must execute the musl binary as well as compile that binary.
