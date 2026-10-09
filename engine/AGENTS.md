# engine/ — Engine Layer Agent Guidance

The embeddable, daemon-free engine library. Crates here are the
platform-neutral core used by the macOS daemon (`app/`) and the System VM's
`guest/arcbox-agent`. Linux guest support remains part of the local product.
ADR 0004 archives the former bare-Linux node product expansion.
The restructure plan and its locked decisions live in
`docs/architecture/charter.md`.

## Layer rules (the `common/` "no VM dep" discipline, one level up)

- **No `app/` dependency, ever**: nothing here may reference the daemon,
  CLI, API, or docker-compat crates, nor daemon artifacts (lock file,
  socket paths, startup pipeline).
- **No direct macOS-only dependency** (`arcbox-vz`, `arcbox-vmnet`,
  `arcbox-route`, `ifbridge`): platform backends are reached only through
  the seam. Today that seam is `arcbox-vmm` + the `virt/arcbox-hypervisor`
  trait layer, and both edges are grandfathered — the target is the
  `virt/arcbox-vm-driver` port (vm-stack-redesign R4), after which
  neither crate appears in an engine manifest.
- **Platform-neutral**: every crate must compile and pass unit tests on
  Linux as well as macOS.
- **Mechanically checked**: `cargo xtask check-layers` (CI `linux-engine`
  job) fails on a direct edge from `engine/` into `app/`, a macOS-only
  crate (the list above plus `arcbox-hv`), a VMM adapter, or `arcbox-vmm` /
  `arcbox-hypervisor` — the last two are today's grandfathered edges
  (`arcbox-engine -> arcbox-vmm` / `arcbox-hypervisor`, until
  vm-stack-redesign R4); rules and exceptions live in
  `xtask/src/commands/check_layers/rules.rs`.
- **Own your errors**: each crate defines its own `thiserror` type built
  on `arcbox_error::CommonError`; `arcbox-core` converts via `From` (see
  `CoreError`'s `From<ImageError>`), so `is_not_found()`-style predicates
  answer identically through either type.
- During the migration, `arcbox-core` keeps compatibility re-exports of
  moved modules; consumers migrate to the engine crates in follow-up
  commits and the re-exports then die. Prefer importing from the engine
  crate in new code.

## Crates

- `arcbox-image` — the boot-asset chain, distro machine-image index, and
  shared remote-image fetch/staging primitives. `assets.lock` is
  `include_str!`-embedded at COMPILE TIME
  (`engine/arcbox-image/src/boot_assets/lockfile.rs`, path
  `../../../../assets.lock` — four levels up to the repo root, same depth
  as its old home under `app/arcbox-core/src/`); editing `assets.lock`
  without rebuilding the daemon changes nothing. The pin semantics and
  failure signatures stay documented in `app/AGENTS.md` ("Boot-asset
  pin").
- `arcbox-snapshot` — snapshot lineage: the checkpoint catalog
  (`snapshot`), the device-mapper copy-on-write rootfs manager checkpoints
  are cloned through (`snapshot_cow`), and the template catalog that
  promotes a snapshot into a versioned base image (`template_catalog`).
  Consumed by `computer/arcbox-computer-runtime` guest-side today.
  - The device-mapper paths only *do* anything on Linux (`dmsetup`, thin
    pools) but compile everywhere, which is what lets the layer above
    keep its own platform-neutrality promise.
  - Loop-device tooling is a seam, not an assumption: `CowManager` takes
    `CowOptions { block_tools: Arc<dyn BlockTools>, dmsetup_candidates, .. }`
    (`snapshot_cow/block_tools/`, one file per implementation).
    `BusyboxBlockTools` is the reference (the System VM's `/bin/busybox`
    applets); `UtilLinuxBlockTools` covers a stock distro's
    `losetup`/`blockdev`, and a consumer wanting neither (the loop ioctls
    direct) supplies its own without forking the module. `stat`/`mknod`
    are syscalls, not applets, so they have no seam. The crate's own
    `dmsetup` search list is the stock-distro one; the System VM's
    `/arcbox/bin/dmsetup` comes from the guest agent's `RuntimeConfig`
    (`firecracker.dmsetup_candidates`), not from here.
    Adding a new host operation means adding a trait method, never a
    hard-coded binary path. BusyBox's `losetup` has no `--show` (no long
    options at all), so `BusyboxBlockTools` attaches as `losetup -f` then
    `losetup LOOPDEV FILE` with a bounded retry when the queried device is
    claimed in between; util-linux attaches in one atomic `losetup -f
    --show` and retries `EBUSY` internally, which is why the two are
    separate types and not one configurable path. Both are tested against
    the real binaries in `test-vm-linux`'s `integration` job.
    `UtilLinuxBlockTools::discover` probes `--version` rather than trusting
    existence: on busybox-based images `/sbin/losetup` is a symlink to the
    applet, which existence alone would select — failing on the first
    attach instead of at composition.
  - `CowManager`'s test seam (`CowTestProbe`, `new_with_test_probe`) is
    behind the **`test-probe` feature**, not `#[cfg(test)]`: a
    `cfg(test)` item does not exist for another crate's tests, and its
    only consumer — `arcbox-computer-runtime`'s cleanup tests — now lives
    in one. `arcbox-computer-runtime` turns the feature on in its
    dev-dependencies.

## Web service directory

- Directory requests consume a dedicated client and accept a cancellation token on both transports. Cancel and await each request before retiring its owner.

## Kubernetes listing

- `AgentClient::list_kubernetes_load_balancers` consumes the connection and returns the client only after success. Cancel through the supplied token and await completion before retiring the VM. Reuse the returned client for the next poll.

## Guest trust

- `agent_client/guest_trust.rs` sends an explicit current public certificate or removal over a dedicated cancellable connection. Await cancellation before retiring the VM. Match the acknowledged fingerprint and expected VM incarnation before reporting client trust as ready.
- Machine trust requests must acquire `host_hold_expected` before connecting. Retain the hold until native connection and request workers finish cancellation.
- Stop and removal must fail before teardown when host holds remain after ten seconds. Drain holds outside the machine registry lock, including stopped-record removal retries. Revalidate the physical run after reacquiring the lock. See ADR 0009.

## Machine settings

- Use the physical run identity returned by `VmManager::start` for distro readiness. Guard Running state, serial drain, addresses, and start events against stopped or replaced runs. Release probe holds before retry sleep.
- Pin bridge-address queries to one physical VM run. Retain the expected host hold through native connection and RPC cleanup. Reject stopped or replaced runs before updating memory or persistence.
- Keep machine web backend selection in `machine::MachineWebPorts`. Preserve omitted updates and inherited clone/archive settings. Persist updates before changing the canonical record. Saved policy does not imply installed routes.

## Durable writes

- `MachineImageManager::pull` accepts a cancellation token. Cancel HTTP reads and lock waits, then await pending file writes before removing staging. Await the pull before releasing its owner; do not cancel by dropping the pull future.
- `MachineManager::close_agent_connections` permanently closes operational agent admission and interrupts registered sockets. Attach both normal and expected-run clients to the manager's socket scope. End readiness retries when the scope closes. Preserve the direct final-shutdown connection in `VmManager`. The caller must join native connectors and operation workers; scope closure alone does not establish worker completion.
- Use `agent_client/retained_session.rs` to own dedicated session transport workers. Drop protocol channels before cancellation and worker joins. Keep protocol admission and acknowledgements in each session module.
- Machine web sessions consume an expected machine connection and fix raw name, canonical hostname, and bridge IPv4 at Open. Replace the full policy with source revisions starting at one and strictly increasing. Validate the exact acknowledgement. Cancel and join before releasing the host hold; configuration acknowledgement does not imply installed routes.
- Instance authority sessions consume one connected agent client. Retain their coordinator, observe idle failure, then drop the session and join before reconnecting. The coordinator owns admission, sequential acknowledgements, continuous reads, and shutdown of both transport workers.
- VM listener transfer requires the expected `VmIncarnation`. Capture and validate the physical run under `VmManager.vms`; stopped or replaced runs must fail. The non-default `arcbox-engine/test-support` feature provides identity-only listener fixtures and never creates a live VM.
- Expected agent and arbitrary-port connections share the VM-entry guard. Hold that guard through the native connection call. Do not acquire a second VM registry lock inside the guarded operation.
- VM stop and removal require the captured `VmIncarnation`. Validate the expected run before shutdown connection, VMM transfer, and completion publication. An absent VMM during graceful shutdown does not mean the VM has stopped.
- Use `MachineManager::stop` for graceful shutdown and its forced fallback. Keep one expected run throughout the operation; never recapture by name after graceful shutdown.

New code that atomically replaces a **file** uses
`common/arcbox-atomic-file` rather than hand-rolling temp-then-rename. It
is **not** the same primitive as `arcbox-engine`'s private
`atomic_write`, which deliberately skips `fsync` for config files: the
crate fsyncs the file *and* the parent directory, and reports
`NotCommitted` separately from `DurabilityUncertain` so a caller can tell
"nothing happened" from "it happened, but might not survive power loss".
Pick the variant handling deliberately — a catalog that must not lie
about what it persisted treats the second as an error; a record store may
keep the record and warn.

Two places in `arcbox-snapshot` still roll their own, and both are
deliberate rather than missed:

- `snapshot::PendingSnapshot::commit` renames a whole **directory** into
  place. The crate covers files only; there is nothing to reuse.
- `snapshot_cow::persistence::write_owner_marker` writes the
  template-loop recovery markers. Migrating it is not a drop-in: it also
  `create_dir_all`s the marker directory, chmods it `0o700`, and fsyncs
  the **grandparent** so the directory creation itself is durable — and,
  less obviously, `cleanup_stale_template_markers` reaps leftover temps
  by matching the `TEMPLATE_MARKER_TEMP_PREFIX` (`.tmp-`) *prefix*, which
  the crate's `.arcbox-{uuid}.tmp` names do not have. Migrate the writer
  and that sweep together or crash recovery silently stops collecting
  its own orphans.
