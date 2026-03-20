//! arcbox-helper — privileged helper daemon for host mutations.
//!
//! Runs as a root launchd daemon with socket activation. launchd creates the
//! socket at `/var/run/arcbox-helper.sock` and starts this process on-demand
//! when the ArcBox daemon connects.

mod server;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(server::run())
}
