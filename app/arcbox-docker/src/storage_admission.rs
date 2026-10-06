//! Rejects known writes while the guest reports protected persistent storage.

use std::sync::Arc;

use arcbox_connect::v1::{StorageHealth, storage_volume_health};
use arcbox_core::Runtime;
use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;

use crate::error::{DockerError, Result};

pub async fn admit(
    State(runtime): State<Arc<Runtime>>,
    request: Request,
    next: Next,
) -> Result<Response> {
    check_request(
        request.method(),
        request.uri().path(),
        runtime.storage_writes_protected(),
        runtime.system_storage_health().as_ref(),
    )?;
    Ok(next.run(request).await)
}

fn check_request(
    method: &Method,
    path: &str,
    protected: bool,
    health: Option<&StorageHealth>,
) -> Result<()> {
    let writes = writes_storage(method, path);
    let mutation = matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) || writes;
    if protected && mutation && !is_control_or_observation(method, path) {
        return Err(DockerError::StorageProtected(
            "Docker storage is protected during or after a storage check. Wait for an active operation, or run `abctl disk recover` to check and resume writes.".into(),
        ));
    }
    if !writes {
        return Ok(());
    }
    let Some(volume) = health.and_then(|health| {
        health.volumes.iter().find(|volume| {
            matches!(
                volume.role.as_known(),
                Some(storage_volume_health::Role::Data | storage_volume_health::Role::Metadata)
            ) && matches!(
                volume.state.as_known(),
                Some(
                    storage_volume_health::State::ReadOnly
                        | storage_volume_health::State::Unavailable
                )
            )
        })
    }) else {
        // Missing observations do not establish a storage fault. Docker remains
        // responsible for reporting operation failures.
        return Ok(());
    };
    let role = match volume.role.as_known() {
        Some(storage_volume_health::Role::Data) => "data",
        Some(storage_volume_health::Role::Metadata) => "metadata",
        _ => "persistent",
    };
    let state = if volume.state == storage_volume_health::State::ReadOnly {
        "read-only"
    } else {
        "unavailable"
    };
    Err(DockerError::StorageProtected(format!(
        "Docker {role} storage is {state}. Run `abctl disk check` to preserve and inspect the disks; the check stops the System VM."
    )))
}

fn is_control_or_observation(method: &Method, path: &str) -> bool {
    *method == Method::POST
        && (path == "/auth"
            || path.strip_prefix("/containers/").is_some_and(|tail| {
                matches!(tail.rsplit_once('/'), Some((_, "stop" | "kill" | "wait")))
            }))
}

/// The path has already passed through the Docker API version stripper.
fn writes_storage(method: &Method, path: &str) -> bool {
    // WebSocket attach uses GET but can send stdin to a process that writes files.
    if *method == Method::GET {
        return path.starts_with("/containers/") && path.ends_with("/attach/ws");
    }
    if *method == Method::POST {
        if matches!(
            path,
            "/build"
                | "/build/prune"
                | "/builder/prune"
                | "/commit"
                | "/containers/create"
                | "/containers/prune"
                | "/images/create"
                | "/images/load"
                | "/images/prune"
                | "/volumes/create"
                | "/volumes/prune"
                | "/networks/create"
                | "/networks/prune"
        ) {
            return true;
        }
        if let Some(tail) = path.strip_prefix("/containers/") {
            return matches!(
                tail.rsplit_once('/'),
                Some((
                    _,
                    "start"
                        | "restart"
                        | "update"
                        | "rename"
                        | "exec"
                        | "attach"
                        | "pause"
                        | "unpause"
                ))
            );
        }
        if let Some(tail) = path.strip_prefix("/images/") {
            return tail.ends_with("/tag");
        }
        if let Some(tail) = path.strip_prefix("/networks/") {
            return matches!(tail.rsplit_once('/'), Some((_, "connect" | "disconnect")));
        }
        return path.starts_with("/exec/") && path.ends_with("/start");
    }
    if *method == Method::DELETE {
        return ["/containers/", "/images/", "/volumes/", "/networks/"]
            .iter()
            .any(|prefix| path.starts_with(prefix));
    }
    *method == Method::PUT
        && ((path.starts_with("/containers/") && path.ends_with("/archive"))
            || path.starts_with("/volumes/"))
}

#[cfg(test)]
mod tests {
    use arcbox_connect::v1::StorageVolumeHealth;

    use super::*;

    fn health(state: storage_volume_health::State) -> StorageHealth {
        StorageHealth {
            volumes: vec![StorageVolumeHealth {
                role: storage_volume_health::Role::Metadata.into(),
                state: state.into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn faults_block_data_and_metadata_mutations_but_keep_reads_and_stop() {
        for state in [
            storage_volume_health::State::ReadOnly,
            storage_volume_health::State::Unavailable,
        ] {
            let health = health(state);
            for (method, path) in [
                (Method::POST, "/containers/create"),
                (Method::POST, "/containers/c/start"),
                (Method::POST, "/build"),
                (Method::POST, "/images/create"),
                (Method::POST, "/images/registry/project/image/tag"),
                (Method::POST, "/volumes/create"),
                (Method::POST, "/networks/n/connect"),
                (Method::POST, "/exec/e/start"),
                (Method::POST, "/containers/c/attach"),
                (Method::GET, "/containers/c/attach/ws"),
                (Method::PUT, "/containers/c/archive"),
                (Method::DELETE, "/containers/c"),
                (Method::DELETE, "/images/registry/project/image"),
                (Method::DELETE, "/volumes/v"),
            ] {
                assert!(
                    check_request(&method, path, false, Some(&health)).is_err(),
                    "{method} {path}"
                );
            }
            for (method, path) in [
                (Method::GET, "/containers/json"),
                (Method::GET, "/containers/c/logs"),
                (Method::GET, "/system/df"),
                (Method::GET, "/events"),
                (Method::HEAD, "/_ping"),
                (Method::POST, "/containers/c/stop"),
                (Method::POST, "/containers/c/kill"),
                (Method::POST, "/containers/c/wait"),
                (Method::POST, "/auth"),
            ] {
                assert!(
                    check_request(&method, path, false, Some(&health)).is_ok(),
                    "{method} {path}"
                );
            }
        }
    }

    #[test]
    fn unknown_observations_do_not_invent_a_fault() {
        for health in [
            None,
            Some(health(storage_volume_health::State::StateUnspecified)),
            Some(health(storage_volume_health::State::MountedReadWrite)),
            Some(health(storage_volume_health::State::NotConfigured)),
        ] {
            assert!(
                check_request(&Method::POST, "/containers/create", false, health.as_ref()).is_ok()
            );
        }
        let unknown = StorageHealth {
            volumes: vec![StorageVolumeHealth {
                role: storage_volume_health::Role::Data.into(),
                state: 99.into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(check_request(&Method::POST, "/containers/create", false, Some(&unknown)).is_ok());
    }

    #[test]
    fn recovery_or_hold_blocks_unknown_mutations_without_a_health_snapshot() {
        assert!(check_request(&Method::POST, "/new/write-api", true, None).is_err());
        assert!(check_request(&Method::GET, "/containers/c/attach/ws", true, None).is_err());
        assert!(check_request(&Method::GET, "/containers/json", true, None).is_ok());
        assert!(check_request(&Method::POST, "/containers/c/stop", true, None).is_ok());
    }
}
