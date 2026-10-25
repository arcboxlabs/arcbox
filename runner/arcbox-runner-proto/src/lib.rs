//! Generated tonic client stubs for the ArcBox Runners gateway contract.
//!
//! The schema is the public module published at `buf.build/arcboxlabs/runner`
//! (`RunnerGatewayService`: `Enroll` + `Attach`). The internal dispatch service
//! is intentionally absent — it lives in the platform's unpublished module.

/// Runner gateway protocol (`arcbox.runner.v1` package).
// `tonic_build` turns the `.proto` message comments into Rust doc comments;
// some first paragraphs exceed Clippy's length limit. This is generated code
// whose wording is owned by the proto, so the doc-style lint doesn't apply.
#[allow(clippy::too_long_first_doc_paragraph)]
pub mod v1 {
    tonic::include_proto!("arcbox.runner.v1");
}
