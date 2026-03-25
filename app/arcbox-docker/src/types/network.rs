//! Network-related Docker API types.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Network summary.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct NetworkSummary {
    /// Name.
    pub name: String,
    /// ID.
    pub id: String,
    /// Created.
    pub created: String,
    /// Scope.
    pub scope: String,
    /// Driver.
    pub driver: String,
    /// Enable IPv6.
    #[serde(rename = "EnableIPv6")]
    pub enable_ipv6: bool,
    /// Internal.
    pub internal: bool,
    /// Attachable.
    pub attachable: bool,
    /// Ingress.
    pub ingress: bool,
    /// Labels.
    pub labels: HashMap<String, String>,
}

/// Network create request.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct NetworkCreateRequest {
    /// Name.
    pub name: String,
    /// Driver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// Internal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub internal: Option<bool>,
    /// Attachable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachable: Option<bool>,
    /// Labels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<HashMap<String, String>>,
}

/// Network create response.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct NetworkCreateResponse {
    /// Network ID.
    pub id: String,
    /// Warning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}
