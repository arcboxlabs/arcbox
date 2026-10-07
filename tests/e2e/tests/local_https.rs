//! Exercise automatic container HTTPS with a Keychain CA and guest-owned leaf keys.
//! Requests cross the guest bridge to port 443. No host routes or trust settings change.

#![cfg(target_os = "macos")]

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, ensure};
use arcbox_e2e::docker::{docker_output, docker_output_with_input, ensure_image};
use arcbox_e2e::scenario::run_vz_scenario;
use arcbox_local_ca::KeychainAuthority;

const MARKER: &str = "keychain-https-ok";

#[test]
#[ignore = "boots a VZ System VM and creates an isolated Keychain CA"]
fn keychain_ca_serves_container_https() -> Result<()> {
    run_vz_scenario("local_https", |daemon, data_dir, _metrics| {
        daemon.wait_ready_blocking(Duration::from_secs(180))?;
        struct Cleanup<'a>(&'a Path);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                KeychainAuthority::delete(&self.0.join("tls")).expect("delete isolated test CA");
            }
        }
        let _cleanup = Cleanup(data_dir);
        let ca = std::fs::read(data_dir.join("tls/ca.pem"))?;
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
        for name in [
            "keychain-web.arcbox.local",
            "alias.arcbox.local",
            "keychain-web.arcbox.local",
        ] {
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
        Ok(())
    })
}
