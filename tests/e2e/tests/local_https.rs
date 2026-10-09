//! Exercise automatic container HTTPS with a Keychain CA and guest-owned leaf keys.
//! Requests cross the guest bridge to port 443. CA trust settings remain unchanged.
//! Run without another ArcBox daemon: startup can reconcile host routes through the helper.
//! Default container trust, opt-out, and SNI ownership are checked separately.

#![cfg(target_os = "macos")]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use arcbox_constants::dns::Domain;
use arcbox_e2e::daemon::connect_unix;
use arcbox_e2e::docker::{
    docker_host, docker_output, docker_output_with_input, ensure_image, run_with_timeout,
};
use arcbox_e2e::scenario::run_vz_scenario;
use arcbox_grpc::SystemServiceClient;
use arcbox_local_ca::KeychainAuthority;
use arcbox_protocol::v1::{
    Empty, ListWebServicesRequest, local_https_status::TrustState, web_service::HttpsMode,
};

const MARKER: &str = "keychain-https-ok";

#[test]
#[ignore = "boots a VZ System VM and creates an isolated Keychain CA"]
fn keychain_ca_serves_container_https() -> Result<()> {
    let domain: Domain = std::env::var("ARCBOX_DNS_DOMAIN")
        .unwrap_or_else(|_| Domain::default().to_string())
        .parse()
        .map_err(anyhow::Error::msg)?;
    let container_name = format!("keychain-web.{domain}");
    let compose_name = format!("web.keychain.{domain}");
    let compose_url = format!("https://{compose_name}/");
    let wrong_name = format!("wrong-owner.{domain}");
    let wrong_url = format!("https://{wrong_name}/");
    let mut ca_dir = None;
    let result = run_vz_scenario("local_https", |daemon, data_dir, _metrics| {
        ca_dir = Some(data_dir.join("tls"));
        daemon.wait_ready_blocking(Duration::from_secs(180))?;
        let preparation = wait_https_preparation(&daemon.grpc_socket())?;
        let ca = std::fs::read(data_dir.join("tls/ca.pem"))
            .with_context(|| format!("read prepared CA; HTTPS status: {preparation}"))?;
        ensure!(
            !data_dir.join("tls/ca-key.pem").exists(),
            "shared CA key exists"
        );
        let image = std::env::var("ARCBOX_E2E_IMAGE").unwrap_or_else(|_| "alpine:latest".into());
        let curl_image = "curlimages/curl:latest";
        ensure_image(data_dir, &image)?;
        ensure_image(data_dir, curl_image)?;
        // Alpine's base BusyBox has nc but no httpd applet.
        let server = format!(
            "while true; do printf 'HTTP/1.1 200 OK\\r\\nContent-Length: {}\\r\\nConnection: close\\r\\n\\r\\n{MARKER}' | nc -l -p 8080; done",
            MARKER.len(),
        );
        docker_output(
            data_dir,
            &[
                "run",
                "-d",
                "--name",
                "keychain-web",
                "--label",
                "dev.arcbox.http-port=8080",
                "--label",
                "com.docker.compose.project=keychain",
                "--label",
                "com.docker.compose.service=web",
                &image,
                "sh",
                "-c",
                &server,
            ],
            Duration::from_secs(30),
        )?;
        let ip = docker_output(
            data_dir,
            &[
                "inspect",
                "-f",
                "{{.NetworkSettings.Networks.bridge.IPAddress}}",
                "keychain-web",
            ],
            Duration::from_secs(10),
        )?
        .trim()
        .parse::<std::net::Ipv4Addr>()?;
        // A second name requires another signing connection; repeating the first exercises the cache.
        for name in [&container_name, &compose_name, &container_name] {
            let script = format!(
                "cat >/tmp/ca.pem; curl --fail --silent --show-error --noproxy '*' --retry 10 --retry-all-errors --retry-delay 1 --retry-max-time 20 --max-time 10 --cacert /tmp/ca.pem --resolve {name}:443:{} https://{name}/",
                ip,
            );
            let response = docker_output_with_input(
                data_dir,
                &[
                    "run",
                    "--rm",
                    "-i",
                    "--entrypoint",
                    "sh",
                    curl_image,
                    "-c",
                    &script,
                ],
                &ca,
                Duration::from_secs(45),
            )?;
            ensure!(response.trim() == MARKER, "HTTPS returned {response:?}");
        }
        let response = docker_output(
            data_dir,
            &[
                "run",
                "--rm",
                curl_image,
                "--fail",
                "--silent",
                "--show-error",
                "--noproxy",
                "*",
                "--max-time",
                "10",
                &compose_url,
            ],
            Duration::from_secs(30),
        )?;
        ensure!(
            response.trim() == MARKER,
            "default container DNS and trust returned {response:?}"
        );
        verify_service_directory(&daemon.grpc_socket(), &container_name, &compose_name)?;
        let mut rejections = vec![
            (
                vec!["--label", "dev.arcbox.add-ca-certificates=false"],
                compose_url.as_str(),
                compose_name.as_str(),
                60,
            ),
            (Vec::new(), wrong_url.as_str(), wrong_name.as_str(), 35),
        ];
        if domain != Domain::default() {
            rejections.push((
                Vec::new(),
                "https://web.keychain.arcbox.local/",
                "web.keychain.arcbox.local",
                35,
            ));
        }
        for (options, url, name, expected_exit) in rejections {
            let output = run_with_timeout(
                Command::new("docker")
                    .env("DOCKER_HOST", docker_host(data_dir))
                    .args(["run", "--rm"])
                    .args(options)
                    .arg(curl_image)
                    .args([
                        "--fail",
                        "--silent",
                        "--show-error",
                        "--noproxy",
                        "*",
                        "--max-time",
                        "10",
                        "--resolve",
                        &format!("{name}:443:{ip}"),
                        url,
                    ]),
                Duration::from_secs(30),
            )?;
            ensure!(
                output.status.code() == Some(expected_exit),
                "{url} must fail with curl exit {expected_exit}, got {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            ensure!(output.stdout.is_empty(), "rejected HTTPS returned a body");
        }
        Ok(())
    });
    // The daemon must stop before deletion or background preparation can recreate the test CA.
    if let Some(dir) = ca_dir {
        KeychainAuthority::delete(&dir).context("delete isolated test CA")?;
    }
    result
}

fn verify_service_directory(socket: &Path, container_name: &str, compose_name: &str) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let mut client = SystemServiceClient::new(connect_unix(socket).await?);
            let listing = client
                .list_web_services(ListWebServicesRequest::default())
                .await?
                .into_inner();
            let service = listing
                .services
                .iter()
                .find(|service| service.domains.iter().any(|name| name == container_name))
                .context("HTTPS container is absent from the web service directory")?;
            ensure!(
                service.hostname == compose_name,
                "incorrect primary service name"
            );
            ensure!(
                service.http_url == format!("http://{compose_name}"),
                "incorrect HTTP URL"
            );
            ensure!(
                service.https_url == format!("https://{compose_name}"),
                "incorrect HTTPS URL"
            );
            ensure!(
                service.https_mode() == HttpsMode::Managed,
                "incorrect HTTPS mode"
            );
            Ok(())
        })
}

fn wait_https_preparation(socket: &Path) -> Result<String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(30), async {
                let mut client = SystemServiceClient::new(connect_unix(socket).await?);
                let mut status = client.watch_setup_status(Empty {}).await?.into_inner();
                while let Some(status) = status.message().await? {
                    if let Some(https) = status.local_https
                        && https.trust_state() != TrustState::Unspecified
                    {
                        return Ok(https.message);
                    }
                }
                anyhow::bail!("HTTPS status stream ended before CA preparation")
            })
            .await
            .context("HTTPS preparation timed out")?
        })
}
