//! Read-only inspection of the bundled helper and the installed helper service.

use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use arcbox_constants::helper::{helper_needs_reinstall, parse_helper_version};
use arcbox_helper::client::{Client, ClientError};
use serde::Serialize;
use tokio::process::Command;
use tokio::time::timeout;

const READINESS_TIMEOUT: Duration = Duration::from_secs(5);

/// Reports helper readiness and resolver roots that require migration.
#[derive(Debug, Serialize)]
pub(super) struct Status {
    pub(super) helper_ready: bool,
    pub(super) legacy_roots: Vec<String>,
}

/// Inspects the supplied helper bundle and the canonical installed helper socket.
pub(super) async fn inspect(helper: &Path, roots: &[String]) -> Result<Status> {
    inspect_at(helper, roots, arcbox_helper::HELPER_SOCKET).await
}

async fn inspect_at(helper: &Path, roots: &[String], socket: &str) -> Result<Status> {
    let version = run(helper, "--version", &[]).await?;
    let bundled = parse_helper_version(&version)
        .with_context(|| format!("unrecognized bundled helper version: {version:?}"))?;
    let legacy_roots = run(helper, "--check-resolvers", roots)
        .await?
        .lines()
        .map(str::to_owned)
        .collect();
    Ok(Status {
        helper_ready: helper_ready(socket, bundled).await?,
        legacy_roots,
    })
}

async fn run(helper: &Path, argument: &str, roots: &[String]) -> Result<String> {
    let output = Command::new(helper)
        .arg(argument)
        .args(roots)
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("failed to execute {} {argument}", helper.display()))?;
    ensure!(
        output.status.success(),
        "{} {argument} failed ({}): {}",
        helper.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout)
        .with_context(|| format!("{} {argument} returned invalid UTF-8", helper.display()))
}

async fn helper_ready(socket: &str, bundled: (u64, u64, u64)) -> Result<bool> {
    let probe = async {
        let (client, _dispatch) = Client::connect_owned_to(socket).await?;
        let version = client.version().await?;
        Ok::<_, ClientError>(!helper_needs_reinstall(
            parse_helper_version(&version),
            Some(bundled),
        ))
    };
    match timeout(READINESS_TIMEOUT, probe).await {
        Ok(Ok(ready)) => Ok(ready),
        Err(_)
        | Ok(Err(
            ClientError::Rpc(_)
            | ClientError::UnrecognizedVersion(_)
            | ClientError::IncompatibleVersion { .. },
        )) => Ok(false),
        Ok(Err(ClientError::Connection(error)))
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(false)
        }
        Ok(Err(error)) => Err(error).context("failed to inspect the installed helper"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};

    const VERSION: &str = "printf 'arcbox-helper 1.2.0\\n'";

    fn helper(directory: &Path, version: &str, resolvers: &str) -> PathBuf {
        let path = directory.join("bundled-helper");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n--version) {version};;\n--check-resolvers) shift; {resolvers};;\n*) exit 64;;\nesac\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    async fn request(stream: &mut UnixStream) -> serde_json::Value {
        let size = stream.read_u32().await.unwrap();
        let mut bytes = vec![0; size as usize];
        stream.read_exact(&mut bytes).await.unwrap();
        let message: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            message["Request"]["message"],
            serde_json::json!({"Version": {}})
        );
        message["Request"]["id"].clone()
    }

    async fn reply(stream: &mut UnixStream, version: &str) {
        let id = request(stream).await;
        let response = serde_json::to_vec(&serde_json::json!({
            "request_id": id,
            "message": {"Ok": arcbox_helper::HelperServiceResponse::Version(version.into())},
        }))
        .unwrap();
        stream.write_u32(response.len() as u32).await.unwrap();
        stream.write_all(&response).await.unwrap();
    }

    async fn closed(stream: &mut UnixStream) {
        timeout(Duration::from_secs(6), stream.read_to_end(&mut Vec::new()))
            .await
            .unwrap()
            .unwrap();
    }

    fn serve(path: &Path, versions: Vec<&'static str>) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for version in versions {
                reply(&mut stream, version).await;
            }
            closed(&mut stream).await;
        })
    }

    #[tokio::test]
    async fn ready_helper_still_inspects_every_selected_resolver_and_closes_dispatch() {
        let directory = tempfile::tempdir().unwrap();
        let helper = helper(directory.path(), VERSION, "printf '%s\\n' \"$@\"");
        let socket = directory.path().join("helper.sock");
        let server = serve(&socket, vec!["arcbox-helper 1.2.0"; 2]);
        let roots = vec!["arcbox.local".into(), "other.local".into()];
        let status = inspect_at(&helper, &roots, socket.to_str().unwrap())
            .await
            .unwrap();
        assert!(status.helper_ready);
        assert_eq!(status.legacy_roots, roots);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn bundled_command_failures_are_inspection_errors() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("helper.sock");
        for (version, resolvers, expected) in [
            ("exit 7", "exit 0", "--version failed"),
            ("printf '\\377'", "exit 0", "invalid UTF-8"),
            (
                "printf 'broken'",
                "exit 0",
                "unrecognized bundled helper version",
            ),
            (VERSION, "exit 13", "--check-resolvers failed"),
            (VERSION, "printf '\\377'", "invalid UTF-8"),
        ] {
            let helper = helper(directory.path(), version, resolvers);
            let error = inspect_at(&helper, &[], socket.to_str().unwrap())
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
        let helper = helper(directory.path(), VERSION, "exit 0");
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = inspect_at(&helper, &[], socket.to_str().unwrap())
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            ErrorKind::PermissionDenied
        );
    }

    #[tokio::test]
    async fn readiness_requires_minimum_and_bundled_version_policy() {
        for (versions, bundled, expected) in [
            (vec!["0.4.24"], (1, 2, 0), false),
            (vec!["broken"], (1, 2, 0), false),
            (vec!["1.2.0", "1.2.0"], (1, 2, 1), false),
            (vec!["1.2.0", "2.0.0"], (1, 2, 0), false),
            (vec!["1.2.0", "broken"], (1, 2, 0), false),
            (vec!["1.2.0", "1.3.0"], (1, 2, 0), true),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("helper.sock");
            let server = serve(&socket, versions);
            assert_eq!(
                helper_ready(socket.to_str().unwrap(), bundled)
                    .await
                    .unwrap(),
                expected
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn unavailable_socket_is_not_ready_but_inspection_errors_propagate() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("helper.sock");
        assert!(
            !helper_ready(socket.to_str().unwrap(), (1, 2, 0))
                .await
                .unwrap()
        );
        drop(UnixListener::bind(&socket).unwrap());
        assert!(
            !helper_ready(socket.to_str().unwrap(), (1, 2, 0))
                .await
                .unwrap()
        );
        let file = directory.path().join("file");
        std::fs::write(&file, "").unwrap();
        let invalid = file.join("helper.sock");
        assert!(
            helper_ready(invalid.to_str().unwrap(), (1, 2, 0))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn incompatible_wire_is_not_ready() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("helper.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            request(&mut stream).await;
            stream.write_u32(1).await.unwrap();
            stream.write_all(&[0]).await.unwrap();
            closed(&mut stream).await;
        });
        assert!(
            !helper_ready(socket.to_str().unwrap(), (1, 2, 0))
                .await
                .unwrap()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn readiness_timeout_cancels_the_owned_dispatcher() {
        for compatible in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("helper.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                if compatible {
                    reply(&mut stream, "arcbox-helper 1.2.0").await;
                }
                request(&mut stream).await;
                closed(&mut stream).await;
            });
            let ready = timeout(
                Duration::from_secs(6),
                helper_ready(socket.to_str().unwrap(), (1, 2, 0)),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(!ready, "compatible={compatible}");
            server.await.unwrap();
        }
    }
}
