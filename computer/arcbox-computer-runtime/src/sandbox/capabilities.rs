//! What this manager's location can do, and why a pause cannot run now.
//!
//! `GetCapabilities` reports per run location and backend (host sandboxes
//! design, "Capabilities and API"); this is the runtime's half of that
//! answer, derived from the driver it was given and its [`Placement`].
//! The pause refusals live here too, because "pause available now" and
//! "why this `Pause` is refused" must be the same answer.

use super::*;
use crate::environment::RunLocation;

/// What a manager's location can do, as `GetCapabilities` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputerCapabilities {
    /// Where the Computers run.
    pub location: RunLocation,
    /// The driver they run under (`"fc"`, `"vz"`, `"hv"`).
    pub backend: &'static str,
    /// Whether Computers here can pause and resume at all.
    pub pause_resume: bool,
    /// Why a pause cannot run right now, when it cannot — `None` when one
    /// can. Unlike [`Self::pause_resume`] this changes over time: a VZ
    /// manager reports its locked session here.
    pub pause_unavailable: Option<String>,
    /// Whether restores can claim pre-warmed VMMs (only a driver that can
    /// spawn a VMM ahead of its guest).
    pub warm_prespawn: bool,
    /// Whether a Computer's ports can be published on the host.
    pub port_publishing: bool,
    /// Whether a Computer can be created with bind mounts. No location
    /// can yet: the in-sandbox agent mounts no VirtioFS share.
    pub bind_mounts: bool,
}

/// The `FAILED_PRECONDITION` detail for a pause of a Computer with bind
/// mounts (D3).
pub const BIND_MOUNTS_CANNOT_PAUSE: &str =
    "a sandbox with bind mounts cannot pause: VirtioFS state is not checkpointed";

impl ComputerManager {
    /// What this manager's location can do, asked now.
    #[must_use]
    pub fn capabilities(&self) -> ComputerCapabilities {
        let driver = &*self.services.driver;
        let claimed = driver.capabilities();
        let placement = &self.services.placement;
        let pause_resume = claimed.checkpoint && relocatable_checkpoints(&self.config, driver);
        let pause_unavailable = if pause_resume {
            placement.checkpoint_available().err()
        } else {
            Some(format!(
                "the `{}` driver cannot checkpoint sandboxes in this configuration",
                driver.name()
            ))
        };
        ComputerCapabilities {
            location: placement.location,
            backend: driver.name(),
            pause_resume,
            pause_unavailable,
            warm_prespawn: driver.prepare().is_some() && pause_resume,
            port_publishing: match placement.location {
                // Guest DNAT plus a host listener, outside the driver.
                RunLocation::Nested => true,
                RunLocation::Host => claimed.port_forward || placement.network_port_forward,
            },
            bind_mounts: false,
        }
    }
}

/// Refuse a checkpoint that cannot be taken now, or a pause of a computer
/// with bind mounts — the two `FAILED_PRECONDITION`s a pause can answer
/// with beyond its state machine.
pub fn capture_precondition(placement: &Placement, pausing: bool, bind_mounts: bool) -> Result<()> {
    if pausing && bind_mounts {
        return Err(ComputerError::FailedPrecondition(
            BIND_MOUNTS_CANNOT_PAUSE.into(),
        ));
    }
    placement
        .checkpoint_available()
        .map_err(ComputerError::FailedPrecondition)
}

/// The label a checkpoint records its macOS build under (D6).
pub const OS_BUILD_LABEL: &str = "arcbox.os_build";

/// Refuse to restore a checkpoint another macOS build wrote (D6). A
/// checkpoint without the label predates the binding — or comes from the
/// nested location, which records none — and is let through.
pub fn check_os_build(placement: &Placement, meta: &SnapshotMeta) -> Result<()> {
    let (Some(here), Some(there)) = (&placement.os_build, meta.labels.get(OS_BUILD_LABEL)) else {
        return Ok(());
    };
    if here == there {
        return Ok(());
    }
    Err(ComputerError::FailedPrecondition(format!(
        "checkpoint {} was written on macOS build {there} and this Mac runs {here}; a \
         checkpoint restores only on the build that wrote it, and nothing it retained was \
         touched",
        meta.id
    )))
}
