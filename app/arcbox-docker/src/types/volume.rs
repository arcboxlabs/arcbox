//! Volume-related Docker API types.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Volume summary.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VolumeSummary {
    /// Name.
    pub name: String,
    /// Driver.
    pub driver: String,
    /// Mountpoint.
    pub mountpoint: String,
    /// Created at.
    pub created_at: String,
    /// Labels.
    pub labels: HashMap<String, String>,
    /// Scope.
    pub scope: String,
    /// Options.
    pub options: HashMap<String, String>,
}

/// Volume list response.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VolumeListResponse {
    /// Volumes.
    pub volumes: Vec<VolumeSummary>,
    /// Warnings.
    pub warnings: Vec<String>,
}

/// Volume create request.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VolumeCreateRequest {
    /// Name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Driver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// Driver options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver_opts: Option<HashMap<String, String>>,
    /// Labels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<HashMap<String, String>>,
}

/// Volume prune response.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VolumePruneResponse {
    /// Volumes deleted.
    pub volumes_deleted: Vec<String>,
    /// Space reclaimed in bytes.
    pub space_reclaimed: u64,
}
