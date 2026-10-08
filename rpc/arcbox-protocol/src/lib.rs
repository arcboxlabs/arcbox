//! # arcbox-protocol
//!
//! Protocol definitions for `ArcBox` communication.
//!
//! This crate defines the message types and service interfaces used for
//! communication between:
//!
//! - CLI <-> Daemon (ttrpc over Unix socket)
//! - Host <-> Guest (ttrpc over vsock)
//! - Docker CLI <-> Daemon (REST API, handled by arcbox-docker)
//!
//! ## Protocol Buffers
//!
//! The protocol is defined using Protocol Buffers for efficient serialization.
//! Message types are generated at build time from `.proto` files.
//!
//! All types are defined under the `arcbox.v1` package and re-exported here.
//!
//! ## Module Structure
//!
//! Types can be accessed via:
//! - `arcbox_protocol::v1::TypeName` - canonical path
//! - `arcbox_protocol::TypeName` - convenient re-exports
//! - `arcbox_protocol::agent::TypeName` - backward compatible submodules
//!
//! ## Services
//!
//! Service definitions are available for:
//! - Container lifecycle operations
//! - Image management
//! - Virtual machine management
//! - Guest agent operations
//! - Network, volume, system, and migration operations (API layer)

mod generated;

// Re-export the generated module as v1 (canonical path)
pub use generated::arcbox_v1 as v1;

// Re-export the sandbox.v1 generated module.
pub use generated::arcbox_sandbox_v1 as sandbox_v1;

// Well-known types (google.protobuf.Timestamp / Empty) are generated against
// pbjson-types (see build.rs); re-export the crate so consumers construct
// them without a direct dependency.
pub use pbjson_types;

/// Common types (from common.proto).
///
/// Re-exports all types for backward compatibility.
pub mod common {
    pub use super::v1::{
        Empty, KeyValue, MachineWebPorts, Mount, PortBinding, ResourceLimits, StorageHealth,
        StorageVolumeHealth, Timestamp, WebPortPolicy, storage_volume_health, web_port_policy,
    };
}

/// Machine types (from machine.proto).
///
/// Re-exports all machine-related types for backward compatibility.
pub mod machine {
    pub use super::v1::{
        CloneMachineRequest, CloneMachineResponse, CreateMachineRequest, CreateMachineResponse,
        DirectoryMount, ExportMachineRequest, ExportMachineResponse, ImportMachineRequest,
        ImportMachineResponse, InspectMachineRequest, ListMachinesRequest, ListMachinesResponse,
        MachineEvent, MachineEventsRequest, MachineExecOutput, MachineExecRequest, MachineHardware,
        MachineInfo, MachineNetwork, MachineOs, MachineStorage, MachineSummary,
        RemoveMachineRequest, SetDefaultMachineRequest, SetMachineResourcesRequest,
        SetMachineResourcesResponse, SetMachineWebPortsRequest, SshInfoRequest, SshInfoResponse,
        StartMachineRequest, StopMachineRequest,
    };
}

/// Container types (from container.proto).
///
/// Re-exports all container-related types for backward compatibility.
pub mod container {
    pub use super::v1::{
        AttachInput, AttachOutput, ContainerConfig, ContainerInfo, ContainerState,
        ContainerStatsRequest, ContainerStatsResponse, ContainerSummary, ContainerTopRequest,
        ContainerTopResponse, CreateContainerRequest, CreateContainerResponse, ExecCreateRequest,
        ExecCreateResponse, ExecOutput, ExecStartRequest, InspectContainerRequest,
        KillContainerRequest, ListContainersRequest, ListContainersResponse, LogEntry, LogsRequest,
        MountPoint, NetworkSettings, PauseContainerRequest, ProcessRow, RemoveContainerRequest,
        StartContainerRequest, StopContainerRequest, UnpauseContainerRequest, WaitContainerRequest,
        WaitContainerResponse,
    };
}

/// Image types (from image.proto).
///
/// Re-exports all image-related types for backward compatibility.
pub mod image {
    pub use super::v1::{
        BuildContext, BuildProgress, ExistsImageRequest, ExistsImageResponse, ImageConfig,
        ImageInfo, ImageSummary, InspectImageRequest, ListImagesRequest, ListImagesResponse,
        PullImageRequest, PullProgress, PushImageRequest, PushProgress, RemoveImageRequest,
        RemoveImageResponse, RootFs, TagImageRequest,
    };
}

/// Agent types (from agent.proto).
///
/// Re-exports all agent-related types for backward compatibility.
pub mod agent {
    pub use super::v1::{
        AgentPingRequest, AgentPingResponse, ContainerFsPathsRequest, ContainerFsPathsResponse,
        ContainerStats, ContainerWebService, DiskTrimRequest, DiskTrimResponse,
        EnsureMachineExportRequest, EnsureMachineExportResponse, EnsureNfsExportRequest,
        EnsureNfsExportResponse, GuestWebService, ImageFsPathsRequest, ImageFsPathsResponse,
        IndexWebService, KubernetesDeleteRequest, KubernetesDeleteResponse, KubernetesHostPort,
        KubernetesKubeconfigRequest, KubernetesKubeconfigResponse, KubernetesLoadBalancer,
        KubernetesLoadBalancersRequest, KubernetesLoadBalancersResponse, KubernetesServicePort,
        KubernetesStartRequest, KubernetesStartResponse, KubernetesStatusRequest,
        KubernetesStatusResponse, KubernetesStopRequest, KubernetesStopResponse,
        ListGuestWebServicesRequest, ListGuestWebServicesResponse, MachineStats, MachineWebService,
        MemoryPressureEvent, MmapReadFileRequest, MmapReadFileResponse, PortBindingsChanged,
        PortBindingsRemoved, ReadinessEvent, ReconcileHttpsAccessRequest,
        ReconcileHttpsAccessResponse, RuntimeEnsureRequest, RuntimeEnsureResponse,
        RuntimeStatusRequest, RuntimeStatusResponse, ServiceStatus, ShutdownRequest,
        ShutdownResponse, StorageCheckRequest, StorageCheckResponse, StorageCheckResult,
        SystemInfo, WatchMemoryPressureRequest, WatchReadinessRequest, WatchStatsRequest,
        container_web_service, guest_web_service, kubernetes_host_port, machine_web_service,
        memory_pressure_event, readiness_event, storage_check_request,
    };

    // Backward compatibility type aliases (short names without Agent prefix).
    pub type PingRequest = super::v1::AgentPingRequest;
    pub type PingResponse = super::v1::AgentPingResponse;
}

/// Kubernetes types (from kubernetes.proto).
pub mod kubernetes {
    pub use super::v1::{
        KubernetesDeleteRequest, KubernetesDeleteResponse, KubernetesHostPort,
        KubernetesKubeconfigRequest, KubernetesKubeconfigResponse, KubernetesLoadBalancer,
        KubernetesLoadBalancersRequest, KubernetesLoadBalancersResponse, KubernetesServicePort,
        KubernetesStartRequest, KubernetesStartResponse, KubernetesStatusRequest,
        KubernetesStatusResponse, KubernetesStopRequest, KubernetesStopResponse,
        kubernetes_host_port,
    };
}

/// API types (from api.proto).
///
/// Re-exports network, volume, system, and migration service types.
pub mod api {
    // Network service types
    pub use super::v1::{
        CreateNetworkRequest, CreateNetworkResponse, InspectNetworkRequest, IpamConfig, IpamSubnet,
        ListNetworksRequest, ListNetworksResponse, NetworkContainer, NetworkInfo, NetworkSummary,
        RemoveNetworkRequest,
    };

    // System service types
    pub use super::v1::{
        AcknowledgeHttpsAccessRequest, CommitLocalCaRotationRequest, Event, EventActor,
        EventsRequest, GetInfoRequest, GetInfoResponse, GetVersionRequest, GetVersionResponse,
        ListWebServicesRequest, ListWebServicesResponse, LocalCaRotation, LocalHttpsAccess,
        LocalHttpsStatus, PrepareLocalCaRotationRequest, PruneRequest, PruneResponse,
        ResolveContainerFsRequest, ResolveContainerFsResponse, ResolveImageFsRequest,
        ResolveImageFsResponse, SystemPingRequest, SystemPingResponse, WebService,
    };

    // Volume service types
    pub use super::v1::{
        CreateVolumeRequest, CreateVolumeResponse, InspectVolumeRequest, ListVolumesRequest,
        ListVolumesResponse, RemoveVolumeRequest, VolumeInfo, VolumeUsage,
    };

    // Migration service types
    pub use super::v1::{
        MigrationContainerMount, MigrationContainerNetworkAttachment, MigrationContainerPlan,
        MigrationContainerSpec, MigrationImagePlan, MigrationMountType, MigrationNetworkIpam,
        MigrationNetworkMode, MigrationNetworkPlan, MigrationPlan, MigrationPortPublish,
        MigrationReplacementSummary, MigrationRestartPolicy, MigrationRunningVolumeBlocker,
        MigrationSourceInfo, MigrationVolumePlan, PrepareMigrationRequest,
        PrepareMigrationResponse, RunMigrationEvent, RunMigrationRequest,
    };

    // Shell/interactive session types
    pub use super::v1::{ShellInput, ShellOutput, TerminalSize};
}

// Common types
pub use v1::{
    Empty, KeyValue, Mount, PortBinding, ResourceLimits, StorageHealth, StorageVolumeHealth,
    Timestamp, WatchStorageHealthRequest, storage_volume_health,
};

// Machine types
pub use v1::{
    CreateMachineRequest, CreateMachineResponse, DirectoryMount, InspectMachineRequest,
    ListMachinesRequest, ListMachinesResponse, MachineEvent, MachineEventsRequest,
    MachineExecOutput, MachineExecRequest, MachineHardware, MachineInfo, MachineNetwork, MachineOs,
    MachineStorage, MachineSummary, MachineWebPorts, RemoveMachineRequest,
    SetMachineWebPortsRequest, SshInfoRequest, SshInfoResponse, StartMachineRequest,
    StopMachineRequest, WebPortPolicy, web_port_policy,
};

// Container types
pub use v1::{
    AttachInput, AttachOutput, ContainerConfig, ContainerInfo, ContainerState,
    ContainerStatsRequest, ContainerStatsResponse, ContainerSummary, ContainerTopRequest,
    ContainerTopResponse, CreateContainerRequest, CreateContainerResponse, ExecCreateRequest,
    ExecCreateResponse, ExecOutput, ExecStartRequest, InspectContainerRequest,
    KillContainerRequest, ListContainersRequest, ListContainersResponse, LogEntry, LogsRequest,
    MountPoint, NetworkSettings, PauseContainerRequest, ProcessRow, RemoveContainerRequest,
    StartContainerRequest, StopContainerRequest, UnpauseContainerRequest, WaitContainerRequest,
    WaitContainerResponse,
};

// Image types
pub use v1::{
    BuildContext, BuildProgress, ExistsImageRequest, ExistsImageResponse, ImageConfig, ImageInfo,
    ImageSummary, InspectImageRequest, ListImagesRequest, ListImagesResponse, PullImageRequest,
    PullProgress, PushImageRequest, PushProgress, RemoveImageRequest, RemoveImageResponse, RootFs,
    TagImageRequest,
};

// Agent types
pub use v1::{
    AgentPingRequest, AgentPingResponse, ContainerWebService, GuestWebService, IndexWebService,
    KubernetesDeleteRequest, KubernetesDeleteResponse, KubernetesHostPort,
    KubernetesKubeconfigRequest, KubernetesKubeconfigResponse, KubernetesLoadBalancer,
    KubernetesLoadBalancersRequest, KubernetesLoadBalancersResponse, KubernetesServicePort,
    KubernetesStartRequest, KubernetesStartResponse, KubernetesStatusRequest,
    KubernetesStatusResponse, KubernetesStopRequest, KubernetesStopResponse,
    ListGuestWebServicesRequest, ListGuestWebServicesResponse, MachineWebService,
    PortBindingsChanged, PortBindingsRemoved, ReconcileHttpsAccessRequest,
    ReconcileHttpsAccessResponse, RuntimeEnsureRequest, RuntimeEnsureResponse,
    RuntimeStatusRequest, RuntimeStatusResponse, ServiceStatus, ShutdownRequest, ShutdownResponse,
    StorageCheckRequest, StorageCheckResponse, StorageCheckResult, SystemInfo,
    container_web_service, guest_web_service, machine_web_service, storage_check_request,
};

// API types - Network
pub use v1::{
    CreateNetworkRequest, CreateNetworkResponse, InspectNetworkRequest, IpamConfig, IpamSubnet,
    ListNetworksRequest, ListNetworksResponse, NetworkContainer, NetworkInfo, NetworkSummary,
    RemoveNetworkRequest,
};

// API types - System
pub use v1::{
    AcknowledgeHttpsAccessRequest, CommitLocalCaRotationRequest, Event, EventActor, EventsRequest,
    GetInfoRequest, GetInfoResponse, GetVersionRequest, GetVersionResponse, ListWebServicesRequest,
    ListWebServicesResponse, LocalCaRotation, LocalHttpsAccess, LocalHttpsStatus,
    PrepareLocalCaRotationRequest, PruneRequest, PruneResponse, ResolveContainerFsRequest,
    ResolveContainerFsResponse, ResolveImageFsRequest, ResolveImageFsResponse, SystemPingRequest,
    SystemPingResponse, WebService,
};

// API types - Volume
pub use v1::{
    CreateVolumeRequest, CreateVolumeResponse, InspectVolumeRequest, ListVolumesRequest,
    ListVolumesResponse, RemoveVolumeRequest, VolumeInfo, VolumeUsage,
};

// API types - Migration
pub use v1::{
    MigrationContainerMount, MigrationContainerNetworkAttachment, MigrationContainerPlan,
    MigrationContainerSpec, MigrationImagePlan, MigrationMountType, MigrationNetworkIpam,
    MigrationNetworkMode, MigrationNetworkPlan, MigrationPlan, MigrationPortPublish,
    MigrationReplacementSummary, MigrationRestartPolicy, MigrationRunningVolumeBlocker,
    MigrationSourceInfo, MigrationVolumePlan, PrepareMigrationRequest, PrepareMigrationResponse,
    RunMigrationEvent, RunMigrationRequest,
};

// API types - Shell
pub use v1::{ShellInput, ShellOutput, TerminalSize};

/// Backward compatibility: Ping request (alias for `AgentPingRequest`).
pub type PingRequest = AgentPingRequest;

/// Backward compatibility: Ping response (alias for `AgentPingResponse`).
pub type PingResponse = AgentPingResponse;
