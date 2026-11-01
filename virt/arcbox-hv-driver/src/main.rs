//! `arcbox-hv-vmm`: the per-VM Hypervisor.framework process that
//! [`arcbox_hv_driver::HvDriver`] spawns.
//!
//! ```text
//! arcbox-hv-vmm --socket <runtime dir>/vmm.sock --id <vm id> [--detect-proxy]
//! ```
//!
//! It logs to stderr (the driver points that at `<runtime dir>/vmm.log`)
//! with `RUST_LOG` as the filter, `info` by default. The binary must be
//! signed with the `com.apple.security.hypervisor` entitlement.

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;

    use arcbox_hv_driver::serve;
    use tracing_subscriber::filter::EnvFilter;
    use tracing_subscriber::prelude::*;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .with_filter(filter),
        )
        .init();

    let args = match parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("arcbox-hv-vmm: {e}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("hv-vmm")
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("arcbox-hv-vmm: runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match serve::run(args, &runtime) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("arcbox-hv-vmm: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "macos")]
fn parse(mut argv: impl Iterator<Item = String>) -> Result<arcbox_hv_driver::serve::Args, String> {
    let (mut socket, mut id, mut detect_proxy) = (None, None, false);
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--socket" => socket = argv.next().map(std::path::PathBuf::from),
            "--id" => {
                let value = argv.next().ok_or("--id needs a value")?;
                id = Some(arcbox_vm_driver::VmId::new(value).map_err(|e| e.to_string())?);
            }
            "--detect-proxy" => detect_proxy = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(arcbox_hv_driver::serve::Args {
        socket: socket.ok_or("--socket is required")?,
        id: id.ok_or("--id is required")?,
        detect_proxy,
    })
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("arcbox-hv-vmm runs on macOS only");
    std::process::exit(1);
}
