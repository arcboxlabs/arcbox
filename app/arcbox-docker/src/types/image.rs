//! Image-related Docker API types.

use super::ContainerConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Image summary.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImageSummary {
    /// Image ID.
    pub id: String,
    /// Parent ID.
    pub parent_id: String,
    /// Repo tags.
    pub repo_tags: Vec<String>,
    /// Repo digests.
    pub repo_digests: Vec<String>,
    /// Created timestamp.
    pub created: i64,
    /// Size.
    pub size: i64,
    /// Virtual size.
    pub virtual_size: i64,
    /// Shared size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_size: Option<i64>,
    /// Labels.
    pub labels: HashMap<String, String>,
    /// Number of containers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub containers: Option<i64>,
}

/// Image inspect response.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImageInspectResponse {
    /// Image ID.
    pub id: String,
    /// Repo tags.
    pub repo_tags: Vec<String>,
    /// Repo digests.
    pub repo_digests: Vec<String>,
    /// Parent.
    pub parent: String,
    /// Comment.
    pub comment: String,
    /// Created.
    pub created: String,
    /// Author.
    pub author: String,
    /// Architecture.
    pub architecture: String,
    /// OS.
    pub os: String,
    /// Size.
    pub size: i64,
    /// Virtual size.
    pub virtual_size: i64,
    /// Config.
    pub config: ContainerConfig,
    /// Root FS.
    pub root_fs: RootFS,
}

/// Root filesystem info.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RootFS {
    /// Type (usually "layers").
    #[serde(rename = "Type")]
    pub root_type: String,
    /// Layer digests.
    pub layers: Vec<String>,
}

/// Image delete response.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImageDeleteResponse {
    /// Deleted image ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<String>,
    /// Untagged reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub untagged: Option<String>,
}
