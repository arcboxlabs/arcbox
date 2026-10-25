//! Generates the runner gateway client stubs from the vendored proto.
//!
//! The proto is the public `buf.build/arcboxlabs/runner` module, vendored under
//! `proto/` (refresh with `make runner-proto-sync`). The agent is a client of
//! the gateway, never a server — the server stubs are generated only so the
//! agent's tests can stand up a mock gateway.

fn main() {
    let proto = "proto/arcbox/runner/v1/runner.proto";

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&[proto], &["proto"])
        .expect("Failed to compile runner.proto");

    println!("cargo:rerun-if-changed={proto}");
}
