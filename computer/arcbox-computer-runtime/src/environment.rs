//! Environment-specific components a composer supplies to the sandbox stack.
//!
//! The manager runs in more than one environment — the ArcBox System VM
//! with its known busybox userland, or a stock distro on a bare-metal
//! node, or directly on the macOS host — and every part that differs is
//! supplied here rather than built in the code. There is no default and no
//! member is optional:
//! [`ComputerManager::new`](crate::ComputerManager::new) builds none of the
//! four, so what VMM its Computers run on is the composer's decision and
//! never a fallback taken here (vm-stack-redesign R3, charter D4).
//!
//! The System VM's composition is `arcbox_agent::sandbox::node_environment`
//! — the Firecracker driver from the config's `[firecracker]` section, the
//! TAP network from its `[network]` section, the `arcbox-vm-proto` agent
//! client, and a copy-on-write rootfs manager over busybox.

use std::sync::Arc;

use arcbox_vm_driver::VmDriver;
use arcbox_vm_driver::net::GuestNetwork;

use crate::agent::GuestAgentFactory;
use crate::snapshot_cow::CowManager;

/// What differs between hosts of the sandbox stack.
#[derive(Clone)]
pub struct NodeEnvironment {
    /// The VMM the sandboxes run under, behind the driver port
    /// (`arcbox_vm_driver::VmDriver`). It must offer `vsock`, which the
    /// guest agent is reached over. `Prepare` and `Staging` are optional: a
    /// driver without `Prepare` (VZ) starts its VMM at boot and gets no warm
    /// pool, and one without `Staging` (VZ, HV) is handed host paths. A
    /// driver missing what the agent's readiness gate needs is refused by
    /// [`ComputerManager::new`](crate::ComputerManager::new) rather than at
    /// the first boot.
    pub driver: Arc<dyn VmDriver>,
    /// What the sandboxes' NICs attach to, behind the guest-network port
    /// (`arcbox_vm_driver::net::GuestNetwork`). It must offer the
    /// `NetworkReconcile` capability: the quarantine ledger is how a host
    /// learns which addresses a previous process still holds, and a
    /// network without it is refused at construction rather than at the
    /// first cleanup ticket.
    pub network: Arc<dyn GuestNetwork>,
    /// How the runtime reaches the agent inside each sandbox, behind the
    /// guest-agent port ([`crate::agent::GuestAgentFactory`]). The
    /// reference is the `arcbox-vm-proto` client over the driver's vsock
    /// capability ([`crate::agent::VmProtoAgentFactory`]), which is what
    /// every Firecracker sandbox speaks; a composer whose Computers are
    /// not reachable that way supplies its own. The factory also decides
    /// what the readiness gate needs from the driver, so an environment
    /// whose driver cannot serve it is refused at construction rather than
    /// at the first boot.
    pub agent: Arc<dyn GuestAgentFactory>,
    /// The copy-on-write rootfs manager. Built by the composer, because
    /// what it needs — the data directory, the loop-device tooling of the
    /// host's userland, where its `dmsetup` lives — is exactly what
    /// differs between environments.
    pub cow_manager: Arc<CowManager>,
}

impl std::fmt::Debug for NodeEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeEnvironment")
            .field("driver", &self.driver.name())
            .finish_non_exhaustive()
    }
}

/// Where this manager's Computers run, and what that location adds to the
/// rules a checkpoint follows (host sandboxes design, "Run locations").
///
/// A daemon composes one manager per location: the System VM's agent runs
/// the nested one, the daemon itself the host one. Which manager a create
/// goes to is the caller's choice (D2); what a manager knows about its own
/// location is here. [`Placement::nested`] is what
/// [`ComputerManager::new`](crate::ComputerManager::new) assumes, and adds
/// nothing to what the nested path has always done.
#[derive(Clone, Default)]
pub struct Placement {
    /// Which location this is.
    pub location: RunLocation,
    /// The macOS build a checkpoint written here is bound to (D6), e.g.
    /// `25E5207k`. `Some` records it on every checkpoint and refuses to
    /// restore one written on another build; `None` records and checks
    /// nothing, which is the nested location's behavior.
    pub os_build: Option<String>,
    /// Whether a checkpoint may be taken right now. VZ saves need an
    /// unlocked session (ADR 0010, decision 6), so a composer running VZ
    /// supplies the driver's lock query here; `None` means a save is never refused for the moment it is asked.
    pub checkpoint_gate: Option<Arc<dyn CheckpointGate>>,
}

impl Placement {
    /// The nested location: no OS build binding, no checkpoint gate.
    #[must_use]
    pub const fn nested() -> Self {
        Self {
            location: RunLocation::Nested,
            os_build: None,
            checkpoint_gate: None,
        }
    }

    /// The host location, with checkpoints bound to `os_build`.
    #[must_use]
    pub fn host(os_build: impl Into<String>) -> Self {
        Self {
            location: RunLocation::Host,
            os_build: Some(os_build.into()),
            checkpoint_gate: None,
        }
    }

    /// Ask `gate` before every checkpoint.
    #[must_use]
    pub fn with_checkpoint_gate(mut self, gate: Arc<dyn CheckpointGate>) -> Self {
        self.checkpoint_gate = Some(gate);
        self
    }

    /// `Err` with the reason when a checkpoint cannot be taken right now.
    pub(crate) fn checkpoint_available(&self) -> Result<(), String> {
        self.checkpoint_gate
            .as_ref()
            .map_or(Ok(()), |gate| gate.checkpoint_available())
    }
}

impl std::fmt::Debug for Placement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Placement")
            .field("location", &self.location)
            .field("os_build", &self.os_build)
            .field("checkpoint_gate", &self.checkpoint_gate.is_some())
            .finish()
    }
}

/// A run location (D2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RunLocation {
    /// A Firecracker microVM nested inside the System VM.
    #[default]
    Nested,
    /// A VM of its own on the macOS host, under VZ or HV.
    Host,
}

impl RunLocation {
    /// The lowercase name the API and logs use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nested => "nested",
            Self::Host => "host",
        }
    }
}

/// Whether a checkpoint may be taken at this moment.
///
/// Asked before every capture, including the idle detector's pause, so a
/// save is never *scheduled* while it cannot succeed — the rule VZ's
/// locked session imposes. The composer adapts its driver's own query.
pub trait CheckpointGate: Send + Sync {
    /// `Ok` when a checkpoint can be taken now; `Err` names why not, in
    /// words fit for a `FAILED_PRECONDITION` detail.
    fn checkpoint_available(&self) -> Result<(), String>;
}
