//! Opt-in checks against an existing Docker engine and offline images.

use std::process::{Command, Output};

use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use super::*;
use crate::proxy::HalfCloseStream;

struct Docker {
    host: String,
    network: String,
    containers: Vec<String>,
}

impl Docker {
    async fn run(&self, args: &[&str]) -> Output {
        tokio::process::Command::new("docker")
            .args(["--host", &self.host])
            .args(args)
            .kill_on_drop(true)
            .output()
            .await
            .unwrap()
    }

    async fn ok(&self, args: &[&str]) -> Vec<u8> {
        let output = self.run(args).await;
        assert!(
            output.status.success(),
            "docker {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    async fn create(&mut self, image: &str, options: &[&str], command: &[&str]) -> String {
        let mut args = vec!["create", "--pull=never", "--network", &self.network];
        args.extend_from_slice(options);
        args.push(image);
        args.extend_from_slice(command);
        let id = String::from_utf8(self.ok(&args).await).unwrap();
        let id = id.trim().to_owned();
        self.containers.push(id.clone());
        id
    }

    async fn copy(&self, source: &Path, id: &str, destination: &str) {
        self.ok(&[
            "cp",
            source.to_str().unwrap(),
            &format!("{id}:{destination}"),
        ])
        .await;
    }

    async fn bundle(&self, id: &str) -> Vec<u8> {
        let archive = self.ok(&["cp", &format!("{id}:{}", BUNDLES[0]), "-"]).await;
        untar(&archive).1
    }

    async fn curl(&self, id: &str) -> Output {
        self.run(&[
            "exec",
            id,
            "curl",
            "--silent",
            "--show-error",
            "--noproxy",
            "*",
            "--max-time",
            "5",
            "--retry",
            "5",
            "--retry-connrefused",
            "--retry-max-time",
            "10",
            "https://web.arcbox.local:8443",
        ])
        .await
    }

    async fn cleanup(&mut self) {
        for id in &self.containers {
            self.ok(&["rm", "--force", "--volumes", id]).await;
        }
        self.containers.clear();
        self.ok(&["network", "rm", &self.network]).await;
        self.network.clear();
    }
}

impl Drop for Docker {
    fn drop(&mut self) {
        // A failed assertion must not leave test containers or their network behind.
        let mut commands: Vec<_> = self
            .containers
            .iter()
            .map(|id| vec!["rm", "--force", "--volumes", id.as_str()])
            .collect();
        if !self.network.is_empty() {
            commands.push(vec!["network", "rm", &self.network]);
        }
        for args in commands {
            match Command::new("docker")
                .args(["--host", &self.host])
                .args(&args)
                .output()
            {
                Ok(output) if output.status.success() => {}
                result => eprintln!("Docker fixture cleanup failed for {args:?}: {result:?}"),
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ARCBOX_CA_TEST_DOCKER_HOST, CLIENT_IMAGE, and SERVER_IMAGE; creates isolated containers"]
async fn real_docker_injection_enables_default_https_trust_and_preserves_opt_out() {
    let required = |suffix| {
        std::env::var(format!("ARCBOX_CA_TEST_{suffix}"))
            .expect("set the Docker host and both existing offline image names")
    };
    let host = required("DOCKER_HOST");
    let client_image = required("CLIENT_IMAGE");
    let server_image = required("SERVER_IMAGE");
    let upstream = PathBuf::from(
        host.strip_prefix("unix://")
            .expect("use a Unix Docker host"),
    );
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("docker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let stop = CancellationToken::new();
    let bridge_stop = stop.clone();
    // The adapter exposes the real Docker engine over the production framed transport.
    let bridge = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                () = bridge_stop.cancelled() => {
                    connections.shutdown().await;
                    break;
                }
                accepted = listener.accept() => {
                    let (socket, _) = accepted.unwrap();
                    let mut upstream = UnixStream::connect(&upstream).await.unwrap();
                    connections.spawn(async move {
                        tokio::io::copy_bidirectional(
                            &mut HalfCloseStream::new(socket), &mut upstream,
                        ).await.unwrap();
                    });
                }
                Some(result) = connections.join_next() => result.unwrap(),
            }
        }
    }));
    let state = guest::state(socket, directory.path());
    let mut docker = Docker {
        host,
        network: format!("arcbox-ca-test-{}", uuid::Uuid::new_v4()),
        containers: Vec::new(),
    };
    docker
        .ok(&["network", "create", "--internal", &docker.network])
        .await;
    let server = docker
        .create(
            &server_image,
            &["--network-alias", "web.arcbox.local"],
            &[
                "caddy",
                "run",
                "--config",
                "/test.Caddyfile",
                "--adapter",
                "caddyfile",
            ],
        )
        .await;
    let client = docker.create(&client_image, &[], &["sleep", "300"]).await;
    let opted_out = docker
        .create(
            &client_image,
            &["--label", "dev.arcbox.add-ca-certificates=false"],
            &["sleep", "300"],
        )
        .await;
    let (root, issuer) = super::super::test_support::root_identity();
    let pem = root.pem().into_bytes();
    let key = rcgen::KeyPair::generate().unwrap();
    let mut leaf = rcgen::CertificateParams::new(vec!["web.arcbox.local".into()]).unwrap();
    leaf.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    for (name, contents) in [
        ("leaf.pem", leaf.signed_by(&key, &issuer).unwrap().pem()),
        ("leaf.key", key.serialize_pem()),
        ("test.Caddyfile", "https://web.arcbox.local:8443 {\n tls /leaf.pem /leaf.key\n respond \"arcbox-container-trust\"\n}\n".into()),
    ] {
        let path = directory.path().join(name);
        std::fs::write(&path, contents).unwrap();
        docker.copy(&path, &server, &format!("/{name}")).await;
    }
    let original = docker.bundle(&client).await;
    assert_eq!(docker.bundle(&opted_out).await, original);
    assert_eq!(
        reconcile_with_pem(&state, &client, &pem).await.unwrap(),
        Vec::<String>::new()
    );
    let injected = docker.bundle(&client).await;
    assert!(injected.starts_with(&original));
    assert!(
        injected
            .windows(pem.trim_ascii().len())
            .any(|bytes| bytes == pem.trim_ascii())
    );
    assert_eq!(
        reconcile_with_pem(&state, &client, &pem).await.unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(docker.bundle(&client).await, injected);
    assert_eq!(
        reconcile_with_pem(&state, &opted_out, &pem).await.unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(docker.bundle(&opted_out).await, original);
    docker.ok(&["start", &server, &client, &opted_out]).await;
    let response = docker.curl(&client).await;
    assert!(
        response.status.success(),
        "{}",
        String::from_utf8_lossy(&response.stderr)
    );
    assert_eq!(response.stdout, b"arcbox-container-trust");
    let response = docker.curl(&opted_out).await;
    assert_eq!(
        response.status.code(),
        Some(60),
        "{}",
        String::from_utf8_lossy(&response.stderr)
    );
    assert_eq!(docker.bundle(&opted_out).await, original);
    stop.cancel();
    bridge.await.unwrap();
    drop(state);
    docker.cleanup().await;
}
