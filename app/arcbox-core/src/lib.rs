//! # arcbox-core
//!
//! Core orchestration layer for `ArcBox`.
//!
//! [`Runtime`] composes host services around the engine's [`MachineManager`],
//! [`VmLifecycleManager`], and [`AgentClient`]. These engine types remain
//! available here as compatibility re-exports.
//!
//! ## Architecture
//!
//! ```text
//! arcbox-daemon / arcbox-api / arcbox-docker
//!                     |
//!                     v
//!            arcbox-core::Runtime
//!                     |
//!                     +-- arcbox-engine: machine/VM lifecycle and agent clients
//!                     +-- arcbox-image: boot assets and distribution images
//!                     +-- arcbox-net: host networking and port forwarding
//! ```
#[cfg(target_os = "macos")]
pub mod bridge_discovery;
pub mod config;
pub mod container_backend;
pub mod error;
pub mod host_mount;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod migration;
#[cfg(target_os = "macos")]
pub mod route_reconciler;
pub mod runtime;
pub mod stats_hub;

// Image management and the engine core moved to the engine layer
// (arcbox-image, arcbox-engine); the module paths and crate-root items
// below are compatibility re-exports.
pub use arcbox_engine::{agent_client, event, machine, persistence, trace, vm, vm_lifecycle};
pub use arcbox_image::{boot_assets, machine_image, remote_image};

pub use arcbox_computer::NestedVirtCapability;
pub use arcbox_engine::agent_client::{
    AgentClient, ExecSessionInput, ExecSessionOutput, WriteFileChunk,
};
pub use arcbox_image::boot_assets::{
    BootAssetConfig, BootAssetManifest, BootAssetProvider, BootAssets, DownloadProgress,
    PreparePhase, boot_asset_version,
};
pub use arcbox_vmm::{DeviceDebug, QueueDebug, VmBackend};
pub use config::{Config, ContainerRuntimeConfig};
pub use error::{CoreError, Result};
pub use machine::MachineManager;
#[cfg(target_os = "macos")]
pub use macos::{
    ImageReference, MacImage, MacImageManager, MacImageMeta, MacInstanceDisks, MacMachineConfig,
    MacMachineInfo, MacMachineManager, MacVm, PullStage, RemoteLocation, RemoteSource,
    ResolvedImage,
};
#[cfg(feature = "macos-ipsw-install")]
pub use macos::{PullPhase, PullSource};
pub use migration::MigrationManager;
pub use runtime::{HostCapacity, MachineResources, SystemVmResources};
pub use runtime::{
    InitProgress, Runtime, SandboxPortExposure, SandboxPortMapping, SandboxPortProtocol,
};
pub use vm::{HostNetwork, SharedDirConfig, VmConfig, VmManager};
pub use vm_lifecycle::{
    ActivityScope, DEFAULT_MACHINE_NAME, DefaultVmConfig, HealthMonitor, VmLifecycleConfig,
    VmLifecycleManager, VmLifecycleState,
};
