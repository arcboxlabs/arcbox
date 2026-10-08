# app/ — Daemon & Core Agent Guidance

Covers `arcbox-daemon` (startup/shutdown), `arcbox-core` (`vm_lifecycle`),
`arcbox-api` (gRPC/diagnostics), `arcbox-cli` (daemon handoff), and
`arcbox-docker` (Engine API proxy). Reference material lives in
`docs/daemon-lifecycle.md` (lock/handoff, residual-state tables) and
`docs/data-directories.md` (filesystem paths) — point there, don't restate.

`arcbox-cli` ships one binary, `abctl`. User-facing strings must name it.

## Startup & readiness contract

- `local_https::Status` in `arcbox-core` is the read-only CA inspection shared by CLI and daemon diagnostics. `abctl tls status --json` returns a hand-written DTO with public metadata. Do not infer network reachability from CA trust or invoke native authorization while reading status. `SetupStatus.local_https` is independent of the startup phase.
- `abctl tls repair` restores only the current profile's public CA file. Repair must not create a key or change trust. Preserve recoverable different certificates and reject ambiguous identities.
- Hold `tls/operation.lock` for each login-session TLS mutation until native authorization and publication finish. Concurrent commands must not reorder trust changes.
- `abctl tls rotate` authorizes the pending candidate before revoking the exact previous root. Release the CLI profile lock before either daemon mutation RPC. The TLS worker serializes commands with refreshes, holds the lock through signer and status updates, and completes accepted work after a client disconnects.
- `SystemService.ListWebServices` and `WatchWebServices` return URLs from the guest's installed routes and current DNS ownership. Clients must use those URLs rather than derive URLs from exposed or published ports. Only managed HTTPS requires ArcBox CA trust. A failed snapshot ends the stream; clients must clear stale entries. The feature acceptance matrix lives in `docs/local-https.md`.
- Pass the validated `Domain` into `Runtime` before initialization. Set `NetConfig::dns` at construction. Host DNS and VM boot parameters must use the same immutable default suffix. Validate service URLs against current Registry ownership; custom claims can use other suffixes. Separate managed `.local` aliases from aliases without managed HTTPS in directory entries.
- HTTPS authorization must not block VM or Docker startup. The daemon owns the Keychain identity and reports `SetupStatus.local_https` independently. The daemon disables Keychain UI for its lifetime. Desktop runs `abctl tls trust` only for a deliberate HTTPS action in the login session. The helper stores public certificates and verifies TLS-only Admin trust. Remove legacy User trust during explicit authorization. Revoke the previous root before replacing `tls/ca.pem`; background publication preserves a trusted previous root. CA preparation errors affect HTTPS status, but failure to remove an exposed private key still blocks sharing the data directory. Development profiles and isolated data directories must not change host trust automatically.
- Register the CA worker independently of Linux VM startup, including in `--no-linux-vm` mode. CA preparation runs in the background and retries after failure. The signing service selects the current published identity when a CSR arrives. Preserve the same authority handle while the root DER is unchanged; reject signatures if that handle changes before delivery.
- Observe browser HTTPS only while the VM is ready and a Desktop client watches an untrusted root. Match exact host source addresses and current DNS ownership. Cancel and join each guest observation RPC before disabling its lease or starting another request.
- Startup is a phased, typed pipeline and the order is load-bearing — see
  root AGENTS.md "Architecture Principles" and `docs/daemon-lifecycle.md`
  before reordering. The current chain is 8 steps
  (`prepare_host → acquire_daemon_lease → start_control_plane →
  release_stale_resources → prepare_assets → boot_runtime →
  start_runtime_services → mark_ready`, `arcbox-daemon/src/main.rs`), each
  producing a richer context type so skipping a phase is a compile error
  (`context.rs`). gRPC (`start_control_plane`) must come before the slow
  boot phases so clients can watch progress.
- Two runtime handles, do not conflate them (`context.rs`): `shared_runtime`
  is filled only after full init and gates normal RPCs with `UNAVAILABLE`
  via `runtime.ready()`; `early_runtime` is filled the moment `Runtime` is
  constructed, before the VM boots, and is diagnostics-only. WHY: a stuck
  boot never fills `shared_runtime`, so any RPC needed to debug it must read
  `early_runtime`. `GetVirtioDebug` is the one such RPC
  (`arcbox-api/src/system.rs`, `self.early_runtime.ready()`). Never gate a
  diagnostic on `shared_runtime`; gate on `early_runtime.ready()` instead —
  `ready()` is the `SharedRuntimeExt` method (`grpc/mod.rs`) that returns
  `UNAVAILABLE` only while the handle's `OnceLock` is empty, and
  `early_runtime` fills the moment `Runtime` is constructed.
- `WatchSetupStatus` is the sole readiness/failure signal clients (desktop,
  e2e) may use — never log-grep or sleep. Fatal startup failures MUST call
  `SetupState::set_failed` before exit (200ms flush grace, `main.rs`) so
  clients see the cause instead of a bare disconnect. Route-install state is
  mirrored into `SetupState.route_installed` by `services::route_status_loop`
  (`ContainerRouteInstalled` sets, `MachineStopped` clears) — WHY: VM
  restarts install the route outside the cold-start path that sets the flag
  directly, so without the bridge the flag goes stale until the next daemon
  restart.
- **A `SetupStatus.Phase` value that nothing publishes is invisible as a
  gap** — it simply never arrives, so a client waits forever or reports a
  plausible zero. Declaring a phase in `api.proto` therefore obliges you to
  publish it; the happy-path progression and which phases are conditional
  live in the enum's own doc comment (`api.proto`) and
  `docs/daemon-lifecycle.md`, and must be updated with any change. This
  was CORE-67: `VM_STARTING`/`VM_READY`/`NETWORK_READY` sat declared and
  unpublished, leaving the slowest stretch of startup silent.
- **The VM phases are reported from inside `Runtime::init`, not derived
  from pipeline order** (`InitProgress` → `SetupPhase` in
  `startup/mod.rs::init_runtime`). WHY: `boot_runtime` is one stage that
  stages guest binaries, boots the VM, waits for the agent, then waits for
  dockerd. Publishing at the stage boundaries would bill the binary
  download to `VM_STARTING` and make `VM_READY` mean "dockerd answered" —
  the span ABX-309 budgets would measure the wrong thing. `init` reports
  `SystemVmStarting` after the binaries are staged and `SystemVmReady` when
  `vm_lifecycle.ensure_ready` returns (readiness level 2 below); the
  dockerd wait lands in the `VM_READY → NETWORK_READY` window. `init`
  reports nothing under `--no-linux-vm`, which is what keeps the VM pair
  off the wire when no guest boots.
- **`SetupState` streams every update, not the newest snapshot**
  (`arcbox-api/src/system.rs`: a `watch` for `GetSetupStatus`, a `broadcast`
  for `WatchSetupStatus`). WHY: `NETWORK_READY` and `READY` are published
  ~300 µs apart with no await point between them, so a snapshot channel
  hands a subscriber only `READY` whenever it does not get scheduled in
  that window — indistinguishable from a phase that was never published,
  and the exact bug CORE-67 set out to remove. The two halves are kept
  atomic by opposite sides of one lock: `publish` broadcasts from inside
  `send_modify`, holding the write lock, and `subscribe` takes the snapshot
  and the receiver together under the read lock that write lock excludes.
  Split either pair and you drop an update or replay one already folded
  into the snapshot, walking a client's phase backwards. Regressions:
  `back_to_back_phases_are_all_delivered`,
  `the_snapshot_is_not_replayed_as_an_update`.
- **A listener the phase promises is bound before `start_services` returns,
  never inside its spawned task** (`DnsService::bind_requested` in
  `start_control_plane`, then `DockerApiServer::bind` + `serve` — CORE-71).
  WHY: a task that binds and only logs its error cannot fail startup, so the
  pipeline publishes `NETWORK_READY` and `READY` for a daemon whose primary
  API no client can reach. `NETWORK_READY` therefore covers whichever
  services this daemon runs — `--no-linux-vm` reaches it with DNS alone —
  and the Kubernetes proxy is the deliberate exception: a taken 16443 is
  tolerated, so it is started here but not promised. Adding a listener means
  deciding which of those two it is. DNS is promised yet never fails on a
  taken default: it binds the profile's port (`ArcboxProfile::dns_host_port`,
  5553 production / 5554 development) and falls back to an OS-allocated one,
  which self-setup publishes through `/etc/resolver/<domain>` and `abctl dns
  status` reads back from that file; only an explicit `--dns-port` /
  `ARCBOX_DNS_PORT` must bind. The DNS socket is bound in
  `start_control_plane`, right after the lease is held, and served from
  `start_services` once the runtime's `NetworkManager` exists (`ControlPlane`
  in `context.rs` carries it across). WHY: both profiles' daemons run on one
  machine, and a development build that shared 5553 with production
  crash-looped under launchd for three days, a full VM boot per 5 s cycle,
  because the bind then sat after `boot_runtime`; a bind failure now costs
  no boot.
- **An unknown name under the local domain is answered NODATA, never
  NXDOMAIN** (`DnsForwarder::try_resolve_locally_or_nodata`). WHY: the
  domain ends in `.local`, which RFC 6762 gives to mDNS, so mDNSResponder
  multicasts every `*.arcbox.local` question as well and takes the unicast
  answer only when it is positive or NODATA; NXDOMAIN, SERVFAIL and REFUSED
  are ignored and the lookup waits out the 5 s mDNS timeout per record type,
  10 s for a getaddrinfo miss (measured 2026-10-01,
  `docs/experiments/2026-10-01-local-domain-negative-answers.md`). A bound
  socket that does not answer yet behaves exactly like no listener, so the
  startup window between the DNS bind and `start_services` needs no
  "not ready" answer either. The guest agent's own DNS server
  (`guest/arcbox-agent/src/dns_server.rs`) keeps NXDOMAIN: its clients are
  Linux resolvers, which have no mDNS leg.
- **The resolver-domain owner also registers its names with mDNSResponder**
  (`arcbox-daemon/src/mdns/`, started from `recovery::run` under the same
  `owns_dns_resolver` predicate that installs `/etc/resolver/<domain>`, and
  only for a domain ending in `.local`). Unicast and Bonjour answer from
  one table: `NetworkManager::subscribe_dns_changes` streams every
  `register_dns`/`deregister_dns`/`replace_dns_projection` as a `DnsChange`, and
  `local_dns_entries` is the snapshot a lagging subscriber resyncs from. The
  records are `kDNSServiceFlagsUnique` on the **loopback interface**
  (`if_nametoindex("lo0")`): nothing leaves the host, and mDNSResponder
  answers a query for a type the name lacks (AAAA for an IPv4 name) at once,
  which it does not do for `kDNSServiceInterfaceIndexLocalOnly` records (a
  Mac without the resolver file then waits 5 s on every AAAA; measured
  2026-10-01). A second owner of a name gets `NameConflict`, which is why
  only the resolver owner registers (a development daemon shares
  `host.arcbox.local` with production).
  Every Bonjour operation needs the Local Network privilege (TN3179):
  mDNSResponder answers `kDNSServiceErr_PolicyDenied` (-65570) within a
  millisecond for a process without it, the mirror logs that once and
  retries every 60 s, and the unicast server keeps answering. `launchd`
  daemons, root and Terminal.app children are exempt; a `launchd` *agent*
  (the production daemon under `SMAppService`) inherits ArcBox.app's
  privilege, so the first registration raises the system's Local Network
  alert for ArcBox once, and a daemon started from a third-party terminal
  inherits that terminal's. The privilege follows the *responsible app
  bundle*: a Homebrew `python3` execs `Python.app`, so a Python probe is
  "Python" in System Settings, not the terminal it ran from. The same
  privilege gates the *clients*: on macOS 15+ a process without it cannot
  resolve any `.local` name, through the resolver file or otherwise
  (measured 2026-10-01: getaddrinfo fails in 0.00 s while `dscacheutil`,
  which asks root's opendirectoryd, answers), so "`curl foo.arcbox.local`
  fails instantly from my terminal" is that terminal's Local Network
  setting, not a daemon bug.
- **`SetupStatus.vm_running` is owned by `services::vm_running_loop`**, which
  mirrors `VmLifecycleState::is_ready` (readiness level 2 below) off
  `Runtime::subscribe_system_vm_state`. WHY: it used to be set once by
  cold-start recovery on a successful guest query and never cleared, so it
  read `true` for the rest of the daemon's life after any stop (CORE-70). A
  second writer re-introduces that class of drift — the loop owns both
  edges. It is armed from `init_runtime` *before* `Runtime::init`, not from
  `start_services`: the VM goes ready partway through `init`, so a later
  start would report it down for the whole dockerd wait. And it is only as
  good as the lifecycle state — an unmanaged guest crash leaves that
  `Running` (ABX-414: no `HealthMonitor` loop), so the flag says "the daemon
  believes the VM is up", not "the VM answered just now". Regression:
  `vm_running_loop_follows_the_lifecycle_both_ways`.
- Startup-cancellation invariant: the flock (`daemon_lock`) and
  `early_runtime` are held in `StartupHandles`, not only in pipeline-local
  context (`context.rs`, `main::run` keeps a clone). WHY: a signal can drop
  the startup future mid-boot while the VM keeps booting in its own
  lifecycle tasks — `interrupt_startup` reaches that orphan VM through
  `handles.early_runtime` (`shutdown.rs`), and the flock must survive the
  cancellation or a concurrent daemon boots into the same disk images. Do
  not move these into pipeline-local state.
- `main::run` keeps a signal watcher armed for the entire startup window;
  the default SIGTERM disposition would otherwise kill the process mid-boot
  and orphan the VM + its Virtualization.framework helpers.

## Failure signatures (symptom → first commands → likely cause)

- Guest boots and daemon reaches READY, but every registry pull / TLS
  handshake fails certificate-validity checks. First: check the daemon log
  for `guest clock sync ping failed` (a warn, not an error);
  `rg -n "sync_guest_clock" engine/arcbox-engine/src/vm_lifecycle/boot.rs`.
  Likely cause: the HV backend has no RTC (ABX-416); the guest wall clock is
  pushed only by the best-effort post-readiness agent ping
  (`AgentPingRequest.timestamp_secs` → agent `clock_settime`). If that ping
  fails
  the guest sits at the kernel default epoch and boot still succeeds, so the
  symptom surfaces far downstream. Do NOT remove the ping until a PL031 RTC
  device lands.
- Large VM (>8 vCPUs) boot-wedges; guest PID 1 in D-state. First, while the
  VM is still alive, capture the live snapshot — do NOT do console-log
  archaeology (it produced multiple wrong root causes on ABX-386): call the
  `GetVirtioDebug` RPC (`SystemService`, served from `early_runtime`) or read
  the e2e-captured `virtio-debug.json`. Compare per-queue live avail/used
  indices. Likely cause: a per-queue register-file bound too small for
  one-blk-queue-per-vCPU (ABX-386 class; `MAX_VIRTQUEUES` lives in
  `virt/arcbox-vmm`). See `virt/AGENTS.md` "Debugging" and
  `virt/arcbox-vmm/AGENTS.md` "MMIO Register File".
- `docker` commands hang or hit a dead pooled connection right after a
  backend switch / VM restart. First:
  `rg -n "reset_if_restarted|restart_generation" app/arcbox-docker app/arcbox-core`.
  Likely cause: the proxy's restart-detection ordering broke (see Docker
  proxy contract below).
- Operations fail with stale-CID / connection-refused errors while both
  `MachineManager` state and `VmLifecycleState` still say `Running`. First:
  confirm whether the guest actually died (`GetVirtioDebug`, guest
  console). Likely cause: no crash auto-recovery — `HealthMonitor`'s
  monitoring loop is unimplemented (`vm_lifecycle/health.rs`: `interval` is
  stored, no loop consumes it) and `on_ensure_ready` (`actor.rs`)
  short-circuits on `is_ready()`, returning the cached CID without probing,
  so a dead guest never leaves `Running` (ABX-414).
- Daemon log's `ARCBOX_BUILD_SHA` ("arcbox-daemon starting", `main.rs`)
  doesn't match the commit you built ("which binary am I running?"). First:
  `cargo clean -p arcbox-daemon && cargo build -p arcbox-daemon`. Likely
  cause: `arcbox-daemon/build.rs` emits no `rerun-if-changed` on
  `.git/HEAD`, so a rebuild that changes no package source file reuses the
  cached SHA — a new commit alone logs a STALE sha (the build.rs comment
  claiming it stays "fresh" holds only when a package file also changed).
- Daemon logs "ArcBox daemon stopped" but the process never exits. First:
  look for a `mount_nfs` or `umount` child of the daemon. Likely cause: a
  new call site runs one of them outside `host_mount`. Both the
  `~/ArcBox/docker` export and the machine mounts go through `host_mount`,
  where each is a tokio child bounded by `MOUNT_ATTEMPT_TIMEOUT` (20 s per
  attempt) or `UNMOUNT_TIMEOUT` (5 s) with `kill_on_drop`; the old
  `spawn_blocking` remount with no per-attempt bound was this signature
  (fixed 2026-10-03).
- Daemon never logs "ArcBox daemon stopped" after the VM stopped. First:
  sample the daemon for a thread inside `stat`/`getattrlist`. Likely cause:
  something `stat`'d an NFS mount point whose server is gone
  (`canonicalize`, `metadata`, `exists` on `~/ArcBox/docker` or a machine
  mount), which blocks until the mount's `deadtimeout`. Resolve a mount
  point with `host_mount::current_mount_info` — a wrapper over
  `arcbox_core::host_mount::MountTable`, which `abctl uninstall` reads too —
  which reads the mount table and never touches the point, and escalate to
  `umount -f` when a plain `umount` is refused (`3044b3f7`, 2026-10-03).

## Machine root mounts

- **A running distro machine's `/` is mounted read-write on the host**, at
  `~/ArcBox/machines/<name>`, under the one host mount root `~/ArcBox` that
  also holds the docker export at `docker/` (`ARCBOX_HOST_MOUNT_DIR` moves
  the root; the e2e harness and the dev daemons point it into their data
  dir; ADR 0003), by `arcbox-daemon/src/machine_mount`. The
  `startup::host_mounts` step of `release_stale_resources` first releases
  whatever a previous daemon left under the root — and the pre-0003 layout,
  `~/ArcBox` itself and `~/ArcBoxMachines/<name>` — reading the mount table
  rather than `stat`'ing dead mount points. The agent serves NFSv3 from an
  `nfs3_server` in its own process and the host's NFS client reaches it
  over the bridge NIC, not a vsock relay (ADR 0002). `mount_machine` and
  `unmount_machine` are the only entry points and both are idempotent; the
  loop follows the event bus the way `machine_dns` does and re-derives the
  mount set from the machine records, so one pass repairs a lagged receiver.
- `Event::MachineStopping { name }` is internal: `stop`, `graceful_stop`
  and `remove` publish it before the VM goes down so the unmount still
  reaches a live server, and it is not mapped onto the public event stream.
  Daemon shutdown unmounts everything before it stops the machines for the
  same reason. A release can fail (the client still has requests in flight,
  seen 2026-10-04); the entry then stays in the registry and a background
  task retries past the mount's `deadtimeout` (60 s). Entries carry a
  generation so a retry never unmounts a newer incarnation of the name.
- The export hides `/arcbox` (ArcBox's own VirtioFS shares) and never puts
  the Mac's AppleDouble `._` sidecars on disk: it translates them into
  `user.*` extended attributes on the target file
  (`guest/arcbox-agent/src/machine_export/vfs/sidecar/`). macOS cannot
  store an xattr on NFSv3 and stamps `com.apple.provenance` on every file a
  downloaded app writes, so without that every host write littered the
  machine with `._*` files and git took a `._pack-*.idx` for a pack index.
  Regression signature: `ls -a` inside the machine shows `._` files next to
  files the Mac wrote.

## Backend transport & agent

- `MachineManager::connect_agent` yields different transports by backend:
  blocking on the HV AF_UNIX socketpair, async on VZ AF_VSOCK and on Linux.
  Branch on `AgentClient::is_blocking()`
  (`engine/arcbox-engine/src/agent_client.rs`); calling a `*_blocking` RPC on VZ (or
  an async RPC on HV) fails deterministically. `sync_guest_clock`
  (`vm_lifecycle/boot.rs`) is the reference pattern: `spawn_blocking` the
  connect, then dispatch `ping_blocking` vs `ping` on `is_blocking()`.
- **Streaming RPCs do not exist on the HV transport.** `AgentTransport::Blocking`
  refuses `async_send`/`async_recv`, so a streaming RPC the daemon issues
  unconditionally at startup kills every HV boot — the sandbox cleanup replay
  did exactly that (`Failed to initialize sandbox cleanup … streaming RPCs not
  supported on blocking transport`) until `init_runtime` gated it on
  `backend.supports_nested_virt()`; HV runs no sandboxes, so there is nothing
  to replay. A new startup RPC either has a blocking variant or a per-backend
  skip. CI runs no HV daemon-level scenario, so the check is manual:
  `ARCBOX_VM_BACKEND=hv cargo test -p arcbox-e2e --test boot_assets -- --ignored`.
- **On VZ, one guest→host vsock stream the host stops reading stalls the
  whole VM**: every new vsock connection to it times out until that stream is
  read or closed (measured 2026-09-26: before machine exec had a window,
  `abctl machine ping` failed during `ssh … 'cat /dev/zero' | sleep 40`;
  `docker version` still hangs during `docker run … cat /dev/zero | sleep 30`).
  The host must never stop draining a guest stream,
  so a streaming RPC needs an application-level window, not a bounded
  channel. Machine exec has one: the host grants output in encoded-frame bytes
  (`OUTPUT_WINDOW`, `engine/arcbox-engine/src/agent_client/machine_exec.rs`),
  the agent grants stdin (`STDIN_WINDOW`,
  `guest/arcbox-agent/src/agent/linux/machine_exec/flow.rs`), and neither side
  sends past its window, so both can always keep reading. An agent that
  grants no window is refused at exec time ("restart the machine to update
  it"). The Docker API channel (2375) has one too since protocol v5:
  `HalfCloseStream` grants each direction 1 MiB in-band (`WINDOW`, header
  top bit set) and keeps a reader task on the connection, so a paused
  `docker attach` backs up into dockerd's socket, not the vsock. The sandbox
  streams open with `SANDBOX_STREAM_WINDOW` (4 MiB, `wire.rs`) returned by
  `SandboxStreamWindow` frames; the guest's `sandbox/window.rs` never sends
  past it and the host's `agent_client/sandbox_stream.rs` returns it as the
  consumer takes frames. A writer whose peer has gone fails with
  `BrokenPipe` instead of waiting for a grant that cannot come — without
  that, dropping an attach hung the container's stop. The Kubernetes
  (16443) and NFS (2049) relays wrap the same `HalfCloseStream` around
  their TCP bytes since v6, so every guest→host vsock the daemon opens is
  windowed.

## VM lifecycle internals (`engine/arcbox-engine/src/vm_lifecycle`)

- `VmLifecycleManager` is a thin facade over a single actor
  (`actor.rs`): mutations go through `Command` variants on an `mpsc`
  channel; reads (`state`, `is_running`) come lock-free from a `watch`
  channel and must never block. Slow I/O (create/start/agent-wait/stop) runs
  in preemptible sub-tasks (`boot.rs`) so `force_stop` can preempt an
  in-flight boot or graceful stop. Adding a synchronous mutation or blocking
  the actor loop breaks force-stop preemption.
- The actor is spawned lazily on the first async facade call (`ensure_actor`,
  `mod.rs`). WHY: constructors are synchronous and may run outside a tokio
  runtime (e.g. `Runtime::new` in tests) where `tokio::spawn` panics.
- `DEFAULT_STARTUP_TIMEOUT_SECS = 90` is deliberately generous (erofs rootfs
  + large `docker.img` mount under CPU/I/O pressure). A tight 30s budget
  raced the cold-boot path into "timeout waiting for agent" loops — do not
  tighten it toward the <1.5s cold-boot target.
- **The idle balloon never shrinks, on either backend, by design** — the
  gate is `BalloonDeps::reclaim_capable` (`balloon/controller.rs`), false on
  both, and load-bearing (full evidence in `balloon/mod.rs` docs). VZ
  (measured 2026-07-29, macOS 26.4): inflation releases NOTHING host-side
  (15.35 GB inflated, daemon `phys_footprint` byte-identical, pages
  *compressed as live data* under real host pressure) — guest RAM lives in
  Apple's XPC process, so measure that process, not the daemon, and nothing
  the daemon does can release it. HV (since 2026-09-25): the guest's free
  page reporting returns idle memory continuously with no host-side target —
  the device releases each reported range by refreshing its stage-2 mapping
  (`virt/arcbox-vmm/AGENTS.md` "Releasing guest RAM") — so a host-driven
  shrink would only starve a guest that was already giving the memory back.
  Host footprint is NOT the configured `memory_mb` — guest RAM is committed
  lazily, so the cost is the high-water mark of guest-*touched* pages (a
  fresh idle 16 GB VM ≈ 718MB); on VZ that mark only ratchets upward, on HV
  it follows the guest's free memory back down. Do not flip a backend to
  reclaim-capable without a measured host `phys_footprint` drop on inflate.
- `set_resources` has the same shape as `set_backend` below: it changes the
  CPU/memory the next (re)boot creates the machine with, the boot's drift check
  recreates a machine that differs, and `Runtime::resize_system_vm` is the
  apply-now path — validate against the host, write `[vm]` in the user's
  `config.toml` (`config::persist`, in place via `toml_edit`), `shutdown`,
  `set_resources`, `ensure_ready`. Persist *before* the restart: a size the
  daemon applied but forgot on its next start is worse than one it refused.
- `set_backend` only changes the backend used on the next (re)boot; it does
  NOT stop or restart a running VM. To apply immediately the caller forces a
  recreate via `Runtime::switch_system_vm_backend`. The backend is seeded
  from the persisted machine, so it survives daemon restarts.
  WARNING: `Runtime::switch_system_vm_backend` (`runtime.rs`) runs
  `shutdown` → `set_backend` → `ensure_ready` with NO serialization against
  a concurrent `ensure_ready`; a boot landing between the stop and
  `set_backend` boots the OLD backend and the switch still reports success
  (ABX-414).
- Three readiness levels, never read one as another — each gates a
  different call surface. (1) `MachineManager` `MachineState::Running`
  (`machine.rs::start`): for the System VM (`distro: None`) this flips the
  instant the VM *process* starts, BEFORE the agent answers — the agent
  wait (`wait_for_machine_ready`) is gated on `distro.is_some()` and never
  runs for the System VM, so it means only "VM process exists," which is
  exactly what `connect_agent`/`connect_vsock_port` gate on. (2)
  `VmLifecycleState::Running`: the agent replied to a ping. (3)
  docker-ready: guest dockerd answers `/_ping` (`GuestDockerBackend`
  watch). A consumer reading `MachineManager` `Running` as "usable" issues
  RPCs into a VM whose agent/dockerd is not up. The `MachineManager`
  `MachineState` lives outside the lifecycle actor on purpose (physical vs
  logical layering) — do not unify them.
- **A distro Machine is not usable when its agent answers — its own init
  has not run yet.** The boot shim runs `machine-init`, backgrounds the
  agent, and only *then* `exec`s the distro's `/sbin/init`
  (`boot-assets/.../machine-init.sh`), so the distro's networking comes up
  after the agent is already serving and typically reconfigures the
  interface from scratch — measured on alpine as a ~20 ms window with zero
  routes, ~100 ms after `Start` would otherwise have returned (CORE-66).
  `wait_for_machine_ready` therefore also gates on
  `SystemInfo.distro_init_pending`, which the agent reads from a sentinel
  (`guest/arcbox-agent/src/boot_done.rs`) written by a hook `machine_init`
  installs into the distro — a static systemd unit ordered
  `After=multi-user.target` and pulled in by a `multi-user.target.d` drop-in,
  an openrc service that `depend()`s `after *`, or a sysvinit inittab `wait`
  entry appended after the `rc N` lines; runit and BusyBox-only images get no
  hook, and readiness does not wait for them. Never give the systemd unit an
  `[Install]` section: a first boot applies the preset policy, and on
  `disable *` distros (Fedora, the RHEL family, openSUSE) that removed the
  `.wants` link before the unit ever ran, so every start burned the 60 s
  readiness timeout. Do NOT replace the hook with an
  inspection of the init system's runtime state: `/run/openrc/rc.starting` is
  absent both *before* openrc runs and after it finishes, so that check
  reports "settled" during exactly the window it exists to catch — a version
  built that way shipped and never fired once. Unit ordering is a public
  contract; a runtime directory's layout is not. Same shape as Lima's
  `/run/lima-boot-done` and multipass's cloud-init `boot-finished`; cloud-init
  itself is unavailable because the mirrored images are the linuxcontainers
  `default` variant, which does not ship it. Two consequences worth knowing:
  the field is phrased as *pending* so an agent predating it decodes false and
  behaves exactly as before; and readiness now costs what the distro's boot
  costs — 2–5 s on alpine, debian, fedora and ubuntu, up to ~10 s on arch and
  rocky (measured 2026-09-26) — because that is when the machine actually
  becomes usable.
- **Distro machines boot with `net.ifnames=0`, and their agent serves RPC
  only.** The mirrored images' own network config (networkd `eth0.network`,
  netplan, `ifcfg-eth0`, ifupdown) names `eth0`, the name a container's NIC
  has. With udev renaming the virtio NIC to `enp0s1`, that config matched
  nothing: the distro never took over DHCP, systemd-resolved had no upstream,
  and ubuntu's start took ~14 s instead of ~4 s. The flag is part of the
  machine cmdline, which is fixed at create, so existing machines keep the old
  naming until they are recreated. The agent tells a machine from the System
  VM by `arcbox.machine_rootfs=` on the kernel cmdline (`agent::Guest`) and
  then starts none of the System VM's services (`guest/AGENTS.md`).
  `SystemInfo.ip_addresses` comes from `getifaddrs`, never from `hostname`:
  NixOS and Oracle ship none, and BusyBox's `-i` resolves the host name
  through DNS, which behind a proxy reported a fake IP. Not ArcBox bugs, and
  not mirrored: the kali and openSUSE Tumbleweed `default` images ship no
  udev, so no device unit ever appears in a VM, and systemd waits 90 s for
  `dev-hvc0.device` (the `serial-getty@hvc0` that `console=hvc0` generates)
  before `multi-user.target` — seen on kali's console; openSUSE times out
  the same way. The boot-test
  loop that found all of this: `ARCBOX_MACHINE_IMAGE_BASE=<dir>` pointing at
  a local `machine-images sync` output (the directory, not its `index.json`),
  and `abctl machine exec` into the machine while `start` is still waiting.
- **A running machine is published as `<hostname>.arcbox.local` at its
  bridge NIC address** (`arcbox-daemon/src/machine_dns.rs`,
  `arcbox-core/src/runtime/machine_dns.rs`), the System VM as
  `default.arcbox.local`. The hostname is `machine_hostname(name)` (`_` and
  `.` become `-`; `create` refuses a name that cannot become a DNS label),
  the same string the shim gets on the cmdline, so the guest and the Mac
  agree. The loop follows machine events and re-derives the answer from
  `MachineInfo.bridge_ip_address` each time: distro machines record it in
  `wait_for_machine_ready`, the System VM in the lifecycle boot's
  `record_bridge_address` (before the actor hears `AgentReady`, so the
  `MachineStarted` it publishes finds the record filled); every stop path
  clears it. The record, not the event, is the truth, which is also what
  repairs a lagged receiver with one pass. Entries carry the `machine:`
  owner prefix and `registered_container_ids` excludes it — without that
  the Docker host reconciler tears every machine down as a vanished
  container within one interval. The entry goes through
  `Runtime::register_dns`, so the resolver owner's mDNS mirror (above)
  announces `<hostname>.arcbox.local` on `lo0` like a container's name and
  withdraws it when the machine stops: `dns-sd -G v4 <hostname>.arcbox.local`
  shows the record on interface 1 while the machine runs (verified
  2026-10-04 with a development daemon on an isolated `.local` domain).
  Regression signature: `dig <name>.arcbox.local` at the daemon's DNS port
  answers NODATA (NOERROR, no answer) for a running machine whose `inspect`
  shows a bridge address.
- **`restart_generation` reports departures, not arrivals.** It is bumped on
  VM *stop* (`Effect::BumpGeneration`, fired from `stopping` on
  `VmEvent::Stopped`), so a task that waits for it to advance wakes at the
  START of the gap where no guest exists. Under
  `switch_system_vm_backend` it then races the reboot with the same
  `DEFAULT_STARTUP_TIMEOUT_SECS` budget the boot itself gets; after a plain
  stop with the daemon still alive, no guest is coming at all. Anything that
  must act when the VM comes *up* watches
  `VmLifecycleManager::subscribe_state` (`Runtime::subscribe_system_vm_state`)
  instead: wait for `VmLifecycleState::is_ready`, do the work, then wait for
  it to clear. Reference consumer: the `~/ArcBox/docker` export reconcile
  (`arcbox-daemon/src/nfs_mount.rs`), whose per-incarnation supervisor loop
  also carries the companion rule — one pass's failure must be retried, not
  propagated out of the loop, or every later incarnation inherits the broken
  state (ABX-426).
- Two startup timeouts cover SEQUENTIAL phases; don't conflate them.
  `DEFAULT_STARTUP_TIMEOUT_SECS = 90` (`mod.rs`) budgets VM boot → agent
  ready; `ContainerRuntimeConfig::startup_timeout_ms = 150_000`
  (`config.rs`) budgets agent ready → dockerd `/_ping`. Residual skew: the
  guest watch loop (`guest/arcbox-agent/src/agent/linux/rpc.rs`
  `handle_watch_readiness`) emits `RuntimeFailed` at exactly its own
  deadline, which equals the host's recv deadline, so the host can drop the
  stream before the failure detail lands (ABX-414).
- **Clone, export and import act on a stopped machine's `data.img` and
  nothing else** (`engine/arcbox-engine/src/machine/{clone,transfer}.rs`,
  `machine/archive/`). The rootfs is a shared read-only image, so a clone
  is one `clonefile(2)`; export snapshots the disk the same way under the
  registry lock and writes the archive from the snapshot without it; import
  restores into `machines/.import-*` and moves the disk into place under
  the lock like `create` (`.export-*`/`.import-*` leftovers are swept when
  the manager starts). A running machine is refused: its btrfs volume has
  dirty pages the host cannot see and no quiesce path through the agent.
  The archive is a `.tar.zst` with `manifest.json` first (the settings plus
  the `MachineImageManifest` of the image it boots) and `data.img` as a
  hand-written GNU sparse 1.0 entry, so a mostly-empty disk costs its
  extents, not its size. Import needs the same image (distro, version,
  rootfs digest, this host's arch) in the local image registry and never
  pulls. A clone or an import gets its own `arcbox.machine_name=` on the
  cmdline, which the guest applies on every boot (`machine_identity`), so
  the source's name never leaks through the overlay's `/etc/hostname`.
- `MachineManager::set_resources` writes the new CPU/memory to the VM config
  and the persisted record at once; a stopped machine boots with it next, a
  running one keeps its size and the result says `restart_required`. It
  restarts nothing. The default machine is `[machine] default_machine` in
  the user's `config.toml` (`config::persist::set_default_machine`, in
  place via `toml_edit`), and the CLI's optional `[NAME]` resolves to it
  (`arcbox-cli/src/commands/machine/target.rs`).

## Docker proxy ↔ lifecycle contract (cross-crate)

`VmLifecycleManager::restart_generation()` is bumped on every VM stop
(the `fetch_add` lives in `actor.rs`'s `Effect::BumpGeneration` arm;
`mod.rs` holds only the read accessor). The Docker proxy compares it via
the request path to detect a
System VM restart (backend switch / recovery) and reset stale state
(`arcbox-docker/src/proxy/state.rs`). Reading a stop-edge counter is correct
*here* precisely because the proxy acts on its next request, which by
definition arrives once the VM is back — do not copy the pattern into a task
that must act at the restart itself (see "reports departures, not arrivals"
above). When editing either side, keep in lockstep:

- `reset_if_restarted` must drop the pooled connections BEFORE flipping
  cached readiness to `Unverified`. WHY: a concurrent verifier that sees
  `Unverified` dials `_ping` immediately and must use the fresh client;
  resetting after would race it onto a connection to the dead VM.
- Endpoint readiness is verified with an HTTP `GET /_ping` against guest
  dockerd (ABX-408), not mere socket-connectability. Do not substitute a
  connect check.
- Both the request path (`ensure_endpoint_verified`) and the host-networking
  reconciler go through the same `reset_if_restarted` so they react to a
  restart through one pool.
- A malformed `/containers/json` response must fail the query, including a missing or empty container ID.
  Only a valid empty array permits reconciliation to remove every registered container.
- Use `arcbox_dns::container::ContainerMetadata` for Docker inspect identity, default aliases, and IPv4 selection.
- Register container DNS through `Runtime::register_container_dns`. Reject invalid inspect snapshots before changing host state. Preserve previous ownership if post-mutation inspect fails.
  The primary address is the first valid IPv4 address in network-name order; ignore the deprecated top-level `NetworkSettings.IPAddress`.
- Cold-start recovery must obtain `guest_query::running_container_snapshot` before publishing DNS or listeners. A failed inspect or mismatched full ID rejects the complete snapshot.

## Host port forwarding & image pull (`arcbox-docker`)

- Preserve equivalent healthy listeners during refresh. Compare the machine, effective host address, both ports, and protocol. Empty bindings withdraw previous listeners. Retain cancellation and join ownership until cleanup completes; configure `PortForwarder` rules before starting the complete set.
- Published container ports are forwarded in userspace: container inspect →
  `parse_port_bindings` → `PortForwarder` binds a host `TcpListener`/`UdpSocket`
  per rule (`virt/arcbox-net/src/port_forward.rs`), reachable via loopback. No
  privileged helper is involved, so a high/ephemeral host port is safe to
  publish under an isolated test daemon (it touches none of the three e2e
  host-globals). A host port below 1024 binds as non-root only on `0.0.0.0`:
  XNU asks for the reserved-port privilege only when the bind names a specific
  address (measured 2026-09-26 as uid 501: `0.0.0.0:999` binds,
  `127.0.0.1:999` is EACCES, TCP and UDP alike). There is no helper fallback,
  so a low port works in the default LAN-exposed mode and fails in loopback
  mode (`expose_ports_to_lan = false`); keep published test ports ephemeral.
- The host bind address of a publish with no particular address (`-p 8080:80`,
  `-p 0.0.0.0:8080:80`) follows `[docker] expose_ports_to_lan`
  (`DockerConfig::default_publish_address`): every interface by default,
  loopback when off. A binding that names an address is honoured either way,
  and sandbox exposures are loopback-only regardless. The relay then dials the
  guest at its *uplink* address, which dockerd's own DNAT for an
  address-pinned binding does not match — the guest agent mirrors those
  (`guest/AGENTS.md`); without the mirror `0.0.0.0` publishes work and
  `127.0.0.1:` publishes RST.
- Image pull is NOT an ArcBox code path: `POST /images/create` (docker pull) is
  proxied verbatim to guest dockerd, which does the registry pull. This is what
  `runtime/AGENTS.md` means by "the pull path elsewhere" — there is no host-side
  pull module to call; drive it through the Docker API proxy.

## Kubernetes LoadBalancer forwarding

- k3s runs servicelb (traefik stays disabled), so a LoadBalancer Service gets
  a svclb pod and the node IP, 10.0.2.2, as its ingress. The daemon
  (`arcbox-daemon/src/kubernetes_lb.rs`) polls the agent's
  `KubernetesLoadBalancers` every 2 s over one kept connection, only while the
  VM is ready *and* the Kubernetes hold is set, and applies each listing
  through `Runtime::apply_kubernetes_load_balancers`
  (`arcbox-core/src/runtime/kubernetes_lb.rs`): one host listener per port of
  a Service that has an ingress, keyed `k8s:<ns>/<name>:<port>/<PROTO>`, bound
  on `default_publish_address()`. The relay's traffic to `10.0.2.2:<port>` is
  DNATed by kube-proxy's loadbalancer-IP rule, so unlike Docker's
  address-pinned publishes it needs no guest mirror.
- Every host listener that is not a container's needs an owner key
  `Runtime::is_container_owner` rejects; otherwise the Docker host-networking
  reconcile treats it as a vanished container's and closes it.
- Stop and delete release the hold *before* closing the listeners, and
  `apply` does nothing without the hold, so a listing that raced a stop cannot
  reopen them. `abctl kubernetes status` prints each port's outcome
  (forwarded, pending, skipped, failed).

## SSH server (`arcbox-ssh`)

- `ssh [user@]<machine>@arcbox` reaches a loopback server in the daemon
  (`app/arcbox-ssh`, wired in `arcbox-daemon/src/ssh_service.rs`). It binds
  like the Kubernetes proxy: `--ssh-port` (0 for any) must bind or startup
  fails, while the default 16022 (`SSH_HOST_PORT`) is best-effort. The host
  key and the one accepted client key are generated once under
  `<data_dir>/ssh/`, next to the `config` and a `known_hosts` that pins the
  host key; `abctl ssh install` adds one `Include` line to `~/.ssh/config`.
- Sessions run on the machine exec path (login sessions, signals, PTY
  resize), `direct-tcpip` on `MachineTcpConnectRequest`, and `sftp` on the
  machine's own `sftp-server` (absent from the mirrored images until the user
  installs it).
- Never block a russh `Handler` callback on the process: russh returns window
  to the client as soon as data arrives, so the handler is the only
  backpressure, and the same connection carries all output. Input goes
  through `input::SessionInput` and output through `Outbound`; blocking the
  handler instead deadlocked `cat big | ssh m cat | slow`.

## Boot-asset pin (`engine/arcbox-image`)

- `assets.lock` is embedded at COMPILE TIME
  (`include_str!("../../../../assets.lock")`, `lockfile.rs`): the daemon
  verifies against whatever `assets.lock` existed when it was built.
  Changing the boot manifest/rootfs requires editing the repo `assets.lock`
  AND rebuilding the daemon — editing the file alone changes nothing.
- The `manifest_sha256` pin is verified on BOTH `get_assets` and
  `prepare_binaries` (`verify_manifest_pin`, `provider.rs`), fail-closed on
  mismatch. A pin MISMATCH makes `resolve_desired_boot` error; `boot.rs`
  catches it and downgrades to a `could not resolve desired boot params;
  skipping drift check` warn, then boots on the PERSISTED cmdline (drift
  recreate skipped). A MISSING pin is a loud warn + skipped verification,
  kept deliberately so local dev can boot unreleased assets (#369). Symptom
  of an accidental stale/missing pin: boots keep an old kernel/cmdline with
  only that warn — grep the daemon log for it.

## Uninstall contract (`arcbox-cli` `commands/uninstall/`)

- `uninstall/inventory.rs` and `docs/data-directories.md` "11. Uninstall" are
  one contract: a new path ArcBox writes on the host lands in both, with its
  owner check. A privileged path is removed only when it passes the helper's
  ownership rule, and `is_arcbox_owned` requires an ArcBox bundle name, not
  only the `xbin` layout: OrbStack links its CLI tools from the same layout,
  and a layout-only check replaced and deleted its links (#715).
- Stop the daemon by launchd label or by the PID in `daemon.lock`, never by
  process name. `pkill -f com.apple.Virtualization.VirtualMachine` stopped
  every VZ guest on the Mac, other tools' included (#716). The daemon stops
  its own System VM on SIGTERM; a free flock means the process is gone.
- Every step reports `Done`, `Skipped(why)` or an error, and the command exits
  non-zero when any step failed. Never print a checkmark for a step whose
  body discarded its result (#716).
- A CLI test that reaches `setup::profile` must inject the home through
  `setup::Integration::under`. `profile_path()` probes the real login shell's
  `ZDOTDIR`, and a test that resolved it from the environment rewrote a
  developer's `~/.config/zsh/.zprofile` (2026-10-01).

## Extending checklists

- Changing a phase's ordering or adding a phase: update the chain in
  `main::start`, the context types in `context.rs`, and
  `docs/daemon-lifecycle.md` together; re-check the gRPC-before-slow-phases
  and lock-before-gRPC constraints.
- Adding a diagnostic RPC: serve it from `early_runtime`, not
  `shared_runtime`, and add a test that it answers while `shared_runtime` is
  empty.
- Touching the daemon lock / socket paths / spawn handoff: the CLI's
  matching logic (`arcbox-cli` `daemon-spawn.lock` + `daemon.lock` handoff)
  must change with it — see `docs/daemon-lifecycle.md` "Spawn serialization".
- Validate untrusted RPC/request inputs once at the daemon boundary. The
  checked-arithmetic rule for virtqueue/ring values (ring GPA, descriptor
  field, queue index) lives in `virt/AGENTS.md` "Guest-controlled input" —
  that surface belongs to `virt/`, not this layer.
- A background loop that talks to a guest agent follows `kubernetes_lb.rs`
  / `disk_reclaim.rs`: gate on `subscribe_system_vm_state`, dispatch on the
  transport (`Runtime::trim_machine_disk` is the pattern for a unary RPC
  that must also work on the HV blocking socketpair), and keep the
  scheduling behind a trait so the timing is unit-tested with
  `start_paused`.

## Validation ladder (cheapest first)

VZ is the oracle backend: HV-only red points at the HV implementation;
double red (HV and VZ) points above the hypervisor. Run cheapest first:

1. Crate unit tests for the touched crate.
2. Daemon-level e2e with `ARCBOX_VM_BACKEND=hv`:
   `cargo test -p arcbox-e2e --test virtio_debug -- --ignored` (live
   `GetVirtioDebug` snapshot) and `--test boot_assets -- --ignored`. The
   e2e targets are `#[ignore]`d — without `-- --ignored` the run reports
   "0 tests run" and validates nothing.
3. Race-class fixes: `cargo xtask e2e --repeat N` (prebuilds once, archives
   per-run logs/metrics, preserves failed data dirs).

e2e realities (see `tests/e2e/AGENTS.md`): readiness only via
`WatchSetupStatus`; failures self-preserve forensics (data dir kept,
`virtio-debug.json` captured while the VM is alive, `metrics.json` phase
timings) — read those before re-running; a stale staged `arcbox-agent`
fails boots confusingly (newest mtime wins); point `ARCBOX_E2E_IMAGE` at a
reachable mirror rather than weakening a test.

## Known gaps / open issues

- ABX-415: daemon SIGSEGV on SIGTERM teardown after successful runs. The
  daemon's `shutdown.rs` only triggers the VM stop; the suspect crash site
  is the HV teardown ordering (`virt/arcbox-vmm/AGENTS.md` "Teardown
  ordering") — do not misdiagnose it as a new regression from your change.
- ABX-416: no PL031 RTC on HV (see the clock-sync failure signature above).
- Current HV daemon perf is far from targets (root AGENTS.md table):
  daemon-ready ~11s (target <1.5s), idle CPU ~3.87% (<0.05%), idle RSS
  ~1.04GB (<150MB). These are known baselines, not per-change regressions.
- The `kick_broadcasts` / `unpark_broadcasts` snapshot fields are retired
  and read 0 (2026-09-30): no io worker kicks vCPUs after an interrupt any
  more (see `virt/arcbox-vmm/AGENTS.md` "Async-Worker Completion Contract").
  HV idle CPU is ~6.7%, ~5.6 points of it the `rx-inject` thread's yield/poll
  loop.
- ABX-413: tgz-packaged docker-tools are verified only via their sha
  sidecar at download time; the EXTRACTED binary is never re-hashed.
- ABX-414: lifecycle-hardening umbrella — the three-level readiness split,
  the unimplemented `HealthMonitor` loop (no crash auto-recovery), the
  unserialized `switch_system_vm_backend`, and the guest-vs-host readiness
  deadline skew (all detailed above).
- ABX-417: the `arcbox-boot` registry crate still carries the upstream
  fixed-temp-name download bug, and the staged `arcbox-agent` binary is
  verified by existence + exec-bit only (no hash).
