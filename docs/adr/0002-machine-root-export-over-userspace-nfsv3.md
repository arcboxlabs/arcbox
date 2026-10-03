# ADR 0002: A distro machine's root is served to the host by a userspace NFSv3 server in its agent, mounted over the bridge NIC

- Status: accepted (2026-10-03)
- Deciders: Xuan
- Commits: the `feat/machine-file-sharing` series (protocol `EnsureMachineExport`, `guest/arcbox-agent/src/machine_export/`, `app/arcbox-daemon/src/machine_mount/`)
- Evidence: `docs/experiments/2026-10-03-machine-root-export.md`

## Context

OrbStack shows every Linux machine's filesystem under `~/OrbStack/<machine>`,
read-write. ArcBox had the System VM's docker data at `~/ArcBox` (read-only
NFSv4, the kernel's nfsd behind a vsock relay) and nothing for machines.

A machine differs from the System VM in three ways that decide how its
root can be served:

- It boots a stock distro image. The boot shim pivots into the overlay and
  lazily unmounts its own EROFS, which is the only place `rpc.mountd` and
  the nfs-utils ship; the distro has neither unless the user installs
  `nfs-server`, and if the user does, that server owns port 2049 and
  `/etc/exports`.
- Its agent serves RPC and nothing else (`guest/AGENTS.md`): a service it
  starts must not take a port or a file a distro service may want.
- Since the bridge NIC landed (c237a83b…b00f585a) the Mac reaches a
  machine's address directly, so a TCP server in the machine needs no
  vsock relay on either side.

Two designs were on the table:

- **A — kernel nfsd.** Keep the shim's EROFS mounted so the agent can run
  the kernel server with the EROFS's `rpc.mountd`, as the System VM does.
  Fastest datapath (in-kernel, zero-copy) and NFSv4 for free. Costs: a
  boot-assets change and release before anything can be tested; the
  kernel server is one per network namespace, so a user's own `nfs-server`
  in the machine conflicts with it on 2049, `/etc/exports`, `exportfs` and
  `/proc/fs/nfsd`; and the in-kernel server enforces the guest's permission
  bits, so a read-write mount for the Mac user means `no_root_squash` or a
  uid map the kernel cannot express per machine.
- **B — userspace NFSv3 in the agent.** `nfs3_server` (BSD-3, the
  maintained fork of `nfsserve`, which hf-mount runs in production) on an
  ephemeral port bound to the machine's bridge address, serving `/`. No
  boot-assets change, no distro port or file touched, ownership mapping in
  our hands. Costs: NFSv3 (no `LINK`, no lock manager: the mount uses
  `locallocks`), a userspace copy per byte, and the server performs no
  authentication, so admission has to be ours.

## What was measured

The experiment entry holds the tables. The numbers the decision rests on,
alpine/ubuntu/fedora machines on VZ, M-series host:

- the mount appears within the machine's `start`, and `/etc/os-release`
  reads through it on every distro tried;
- a 1 GiB file writes and reads back through the mount at the rates in the
  entry — well inside what a developer workflow needs, and the same order
  as the `~/ArcBox` NFSv4 export over its vsock relay;
- `ls` of a 1 000-entry directory and `git status` of a checked-out
  repository complete in the times in the entry;
- a `git clone` from the Mac through the mount left `._pack-*.idx`
  AppleDouble sidecars in the machine that broke git there, until the
  agent kept the Mac's sidecars in memory; with that, the clone is clean
  on both sides.

## Decision

1. A distro machine's agent answers `EnsureMachineExport` by serving its
   root filesystem over NFSv3 from `nfs3_server`, read-write, on an
   ephemeral TCP port of the bridge NIC. The System VM refuses the RPC: its
   data stays behind the NFSv4 docker export.
2. The NFS server binds loopback only. The one socket on the bridge NIC is
   a relay that admits exactly the peers the host named in the request —
   the host's own addresses on the bridge network — and drops every other
   connection. Every VM and container on that network can reach the port;
   none may mount a machine.
3. Ownership crosses the wire through one involution: the host user's
   uid/gid and the guest owner's (root, until machines have a default user)
   trade places, every other id passes through. Files the Mac creates are
   root's in the machine; root's files are the Mac user's on the Mac.
4. The daemon mounts the export at `<root>/<name>` when the machine reaches
   readiness, where the root is `~/ArcBoxMachines` unless
   `ARCBOX_MACHINE_MOUNT_DIR` moves it, and unmounts it on the
   `MachineStopping` edge — while the server still answers — and again
   after stop, remove and at shutdown. Nothing else under the root is ever
   unmounted by the daemon.
5. `~/ArcBox/machines/<name>` is not the mount point, although the task
   asked for it: `~/ArcBox` is the read-only NFS mount of the docker
   export, and nothing can be created inside it. Moving that export to
   `~/ArcBox/docker` would free the name but moves the paths the desktop
   app maps guest paths onto; that is a separate decision.
6. Writes honour the client's stability request: an `UNSTABLE` write is
   not fsynced, the following `COMMIT` is. The server never claims
   `FILE_SYNC` for data it did not sync.
7. The `._<name>` AppleDouble files the macOS client writes for extended
   attributes never reach the machine's disk. The agent keeps the ones the
   Mac creates in memory (at most 4 096), answers them to lookups and
   lists none of them: the Mac sees extended attributes, not files, and
   git on the Mac never meets a `._pack-*.idx`. A `._` file made inside
   the machine is a plain file and takes precedence.

## Consequences

- `/arcbox` — ArcBox's VirtioFS share of the host's own data directory —
  is hidden from the export. Everything else under `/`, `/proc` and `/sys`
  included, is visible, as in the machine.
- Hard links cannot be created from the Mac (`NFS3ERR_NOTSUPP`), and file
  locks are client-local. Absolute symlinks inside a machine resolve against
  the Mac's root when followed from the Mac.
- An object deleted inside the machine keeps its handle until the Mac next
  touches it (`ESTALE`), and the id table grows with every distinct path the
  Mac has seen (~100 bytes each).
- The Mac's extended attributes on a machine's files last as long as the
  machine runs: a stopped machine, or the sidecar table evicting an old
  entry, drops them, and the Mac sees "no attribute" and writes them again
  on the next `setxattr`. Finder's `.DS_Store` files are ordinary files
  and do land in the machine, as on any network mount.
- Changing the mount root, the port strategy, or the admission rule is a
  new ADR; so is moving the docker export under `~/ArcBox/docker`.
- The relay costs one loopback hop per RPC inside the machine. Removing it
  requires an upstream API to serve an already-accepted connection, or a
  kernel-enforced per-socket source filter; either supersedes point 2's
  mechanism, not its rule.

## Alternatives considered

- **A, kernel nfsd from a retained EROFS**: rejected for the cross-repo
  release it needs before any test, and the collision with a user's own
  `nfs-server` (see Context).
- **The NFSv4 docker export's shape — vsock relay on both sides**: two
  userspace hops plus `HalfCloseStream` framing on every byte, when the
  bridge NIC already gives the Mac a direct TCP path.
- **`nfsserve` (xetdata/huggingface)**: same lineage, 1.1 M downloads, but
  its write path returns `FILE_SYNC` for every write without a `COMMIT`
  hook, so a correct server must fsync per 32 KiB–1 MiB chunk or lie.
  `zerofs_nfsserve` adds auth to the VFS but relicensed its changes AGPL.
- **A BPF socket filter instead of the relay**: zero steady-state cost and
  kernel-enforced, but a cBPF program keyed on the IPv4 source address is a
  rule nobody reading the code would expect to find on a listening socket.
  Kept as the mechanism to measure against if the relay's hop shows up.
- **Hiding `/proc`, `/sys` and `/dev` as well as `/arcbox`**: they are the
  machine's; OrbStack shows them; only `/arcbox` points back at the host.
