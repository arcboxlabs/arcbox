//! `EnsureMachineExport` RPC handler: starts the machine root export on the
//! bridge NIC and answers with its endpoint.

use std::path::PathBuf;

use arcbox_connect::v1::{EnsureMachineExportRequest, EnsureMachineExportResponse};

use crate::agent::Guest;
use crate::machine_export::ExportConfig;
use crate::rpc::{ErrorResponse, RpcResponse};

/// Serves the machine root to the host. The System VM is refused: its data
/// is the read-only docker export behind `EnsureNfsExport`, and its root is
/// a read-only EROFS nobody needs mounted. A machine whose bridge NIC has no
/// address yet answers `EAGAIN` so the host retries rather than gives up.
pub(super) async fn handle_ensure_machine_export(
    req: EnsureMachineExportRequest,
    guest: Guest,
) -> RpcResponse {
    if guest != Guest::DistroMachine {
        return RpcResponse::Error(ErrorResponse::new(
            libc::EINVAL,
            "only a distro machine exports its root; the System VM's data is behind EnsureNfsExport",
        ));
    }
    let config = match ExportConfig::from_request(&req) {
        Ok(config) => config,
        Err(e) => return RpcResponse::Error(ErrorResponse::new(libc::EINVAL, e)),
    };
    let Some(bridge) = super::system_info::bridge_ipv4() else {
        return RpcResponse::Error(ErrorResponse::new(
            libc::EAGAIN,
            "the machine's bridge NIC has no address yet",
        ));
    };
    match crate::machine_export::ensure(PathBuf::from("/"), bridge, config).await {
        Ok(endpoint) => RpcResponse::EnsureMachineExport(EnsureMachineExportResponse {
            address: endpoint.address.to_string(),
            port: u32::from(endpoint.port),
            ..Default::default()
        }),
        Err(e) => RpcResponse::Error(ErrorResponse::new(libc::EIO, e)),
    }
}
