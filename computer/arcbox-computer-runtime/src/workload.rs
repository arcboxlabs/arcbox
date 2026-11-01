//! The workload contract: what runs in a computer, as an OCI runtime spec.
//!
//! An image's config (Entrypoint, Cmd, Env, WorkingDir, User) and the create
//! request become one `config.json`, modeled by `arcbox-oci`
//! ([`bundle_spec`]). [`Workload::from_spec`] checks that spec against what a
//! computer can honor and translates it into the [`StartCommand`] the guest
//! agent runs.
//!
//! The VM is the isolation boundary (ADR 0010, decision 9): nothing inside
//! it enforces OCI namespaces, seccomp, capabilities, rlimits, devices or
//! hooks, so a spec that asks for one is rejected rather than run without
//! it. `Spec::validate` checks none of these, which is why the check lives
//! here. OCI's `Status` has no paused state; pause belongs to
//! `ComputerManager`, and nothing here models lifecycle.
//!
//! [`StartCommand`]: crate::StartCommand

mod check;
mod spec;
mod start;

pub use check::Rejected;
pub use spec::{ImageAccounts, WorkloadRequest, bundle_spec};
pub use start::Workload;

use crate::error::ComputerError;

/// Why a workload cannot run as specified.
#[derive(Debug, thiserror::Error)]
pub enum WorkloadError {
    /// The spec fails `arcbox-oci`'s own validation.
    #[error("invalid OCI config: {0}")]
    Invalid(#[from] arcbox_oci::OciError),
    /// The spec asks for isolation the VM boundary replaces.
    #[error("OCI {0} are not enforced inside a computer; the VM is the isolation boundary")]
    Rejected(Rejected),
    /// The spec sets a field the guest agent cannot carry out.
    #[error("OCI field {0} is not supported in a computer")]
    Unsupported(String),
    /// The user or group does not resolve against the image.
    #[error("{0}")]
    User(String),
}

impl From<WorkloadError> for ComputerError {
    fn from(error: WorkloadError) -> Self {
        Self::Config(error.to_string())
    }
}
