//! arcbox-helper — privileged helper daemon for host mutations.
//!
//! Runs as a root launchd daemon with socket activation. launchd creates the
//! socket at `/var/run/arcbox-helper.sock` and starts this process on-demand
//! when the ArcBox daemon connects.

mod server;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "--help" | "-h" => {
                eprintln!(
                    "arcbox-helper {}\nPrivileged helper daemon for host mutations (routes, DNS, sockets)",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            "--version" | "-V" => {
                eprintln!("arcbox-helper {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => {}
        }
    }

    // Helper runs as root — write logs to /var/log/arcbox/helper.log.
    let _log_guard = arcbox_logging::init(arcbox_logging::LogConfig {
        log_dir: std::path::PathBuf::from(arcbox_constants::paths::privileged_log::HELPER_LOG_DIR),
        file_name: arcbox_constants::paths::privileged_log::HELPER_LOG.to_string(),
        default_filter: "arcbox_helper=info".to_string(),
        foreground: true, // Also log to stderr (captured by launchd).
        ..arcbox_logging::LogConfig::default()
    });

    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(server::run());

    _log_guard.flush();
    result
}
