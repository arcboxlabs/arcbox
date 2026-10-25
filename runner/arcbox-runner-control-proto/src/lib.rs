//! Generated tonic client/server stubs for the runner agent's local
//! control-plane API.
//!
//! Served by `arcbox-runner-agent` on `~/.arcbox/runner/agent.sock`; consumed
//! by the `arcbox-runner-agent` CLI and the desktop app. Internal-only —
//! unlike `arcbox-runner-proto`, this contract is never published.

/// Runner agent control-plane protocol (`arcbox.runner.control.v1` package).
// `tonic_build` turns the `.proto` message comments into Rust doc comments;
// some first paragraphs exceed Clippy's length limit. This is generated code
// whose wording is owned by the proto, so the doc-style lint doesn't apply.
#[allow(clippy::too_long_first_doc_paragraph)]
pub mod v1 {
    tonic::include_proto!("arcbox.runner.control.v1");
}
