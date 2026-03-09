//! ArcBox REST/HTTP API.
//!
//! This module provides a JSON REST API served over a Unix domain socket
//! (`~/.arcbox/api.sock`). It is designed for external SDK consumption
//! (TypeScript, Python, Swift) and the `arcbox-desktop-swift` application.
//!
//! Streaming RPCs use Server-Sent Events (SSE).

mod handlers;
mod sse;
pub(crate) mod types;

use crate::sandbox::AppState;
use axum::Router;
use std::sync::Arc;

/// Builds the complete REST API router.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new().nest("/v1", handlers::v1_routes(state))
}
