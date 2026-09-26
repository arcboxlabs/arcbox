//! Agent main loop and request handling.
//!
//! The Agent listens on vsock port 1024 and handles RPC requests from the host.
//! It manages container lifecycle and executes commands in the guest VM.
//!
//! The actual implementation lives in [`linux`] (compiled on Linux guests) or
//! [`stub`] (a no-op kept buildable on non-Linux hosts for development).

use anyhow::Result;
use arcbox_constants::cmdline::MACHINE_ROOTFS_KEY;

pub mod ensure_runtime;
#[cfg(any(target_os = "linux", test))]
mod exec_error;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(not(target_os = "linux"))]
mod stub;

#[cfg(target_os = "linux")]
pub use linux::Agent;
#[cfg(target_os = "linux")]
pub use linux::container_network;

#[cfg(not(target_os = "linux"))]
pub use stub::Agent;

/// The guest the agent serves, which decides what runs beside the vsock RPC
/// listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guest {
    /// The System VM: dockerd and every service around it.
    SystemVm,
    /// A distro machine. Its own init owns its services, so the agent serves
    /// RPC and nothing else: the System VM's DNS server on `0.0.0.0:53`
    /// shadowed systemd-resolved's `127.0.0.53` stub (resolved turns the stub
    /// off, and every glibc lookup then reached a server that does not answer
    /// the internet), and its reconcilers would rewrite the machine's own
    /// NAT table the moment the user installed dockerd there.
    DistroMachine,
}

impl Guest {
    /// Reads the guest off the kernel command line, where the machine boot
    /// shim's contract key (`arcbox.machine_rootfs=`) marks a distro machine.
    pub fn detect() -> Self {
        match std::fs::read_to_string("/proc/cmdline") {
            Ok(cmdline) => Self::from_cmdline(&cmdline),
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the kernel cmdline; serving as the System VM");
                Self::SystemVm
            }
        }
    }

    fn from_cmdline(cmdline: &str) -> Self {
        if cmdline
            .split_whitespace()
            .any(|token| token.starts_with(MACHINE_ROOTFS_KEY))
        {
            Self::DistroMachine
        } else {
            Self::SystemVm
        }
    }
}

/// Runs the agent for `guest`.
pub async fn run(guest: Guest) -> Result<()> {
    let agent = Agent::new();
    agent.run(guest).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to parse Docker JSON log line for testing.
    fn parse_docker_log_line(line: &str, stdout: bool, stderr: bool) -> Option<String> {
        let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
        let stream = parsed.get("stream")?.as_str()?;
        let log = parsed.get("log")?.as_str()?;

        match stream {
            "stdout" if stdout => Some(log.to_string()),
            "stderr" if stderr => Some(log.to_string()),
            _ => None,
        }
    }

    #[test]
    fn test_parse_docker_log_stdout() {
        let line = r#"{"log":"hello world","stream":"stdout","time":"2024-01-08T12:00:00Z"}"#;

        let result = parse_docker_log_line(line, true, false);
        assert_eq!(result, Some("hello world".to_string()));

        // Should filter out when stdout=false
        let result = parse_docker_log_line(line, false, true);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_docker_log_stderr() {
        let line = r#"{"log":"error message","stream":"stderr","time":"2024-01-08T12:00:00Z"}"#;

        let result = parse_docker_log_line(line, false, true);
        assert_eq!(result, Some("error message".to_string()));

        // Should filter out when stderr=false
        let result = parse_docker_log_line(line, true, false);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_docker_log_both_streams() {
        let stdout_line = r#"{"log":"stdout msg","stream":"stdout","time":"2024-01-08T12:00:00Z"}"#;
        let stderr_line = r#"{"log":"stderr msg","stream":"stderr","time":"2024-01-08T12:00:00Z"}"#;

        // Both enabled
        assert_eq!(
            parse_docker_log_line(stdout_line, true, true),
            Some("stdout msg".to_string())
        );
        assert_eq!(
            parse_docker_log_line(stderr_line, true, true),
            Some("stderr msg".to_string())
        );
    }

    #[test]
    fn test_parse_docker_log_invalid_json() {
        let invalid = "not json";
        assert_eq!(parse_docker_log_line(invalid, true, true), None);

        let incomplete = r#"{"log":"test"}"#; // Missing stream field
        assert_eq!(parse_docker_log_line(incomplete, true, true), None);
    }

    #[test]
    fn test_parse_docker_log_special_characters() {
        // Test with escaped characters
        let line = r#"{"log":"line with \"quotes\" and \\backslash","stream":"stdout","time":"2024-01-08T12:00:00Z"}"#;

        let result = parse_docker_log_line(line, true, false);
        assert_eq!(
            result,
            Some(r#"line with "quotes" and \backslash"#.to_string())
        );
    }

    #[test]
    fn test_parse_docker_log_empty_content() {
        let line = r#"{"log":"","stream":"stdout","time":"2024-01-08T12:00:00Z"}"#;

        let result = parse_docker_log_line(line, true, false);
        assert_eq!(result, Some(String::new()));
    }

    #[test]
    fn test_parse_docker_log_multiline_content() {
        // Docker typically escapes newlines in log content
        let line = r#"{"log":"line1\\nline2","stream":"stdout","time":"2024-01-08T12:00:00Z"}"#;

        let result = parse_docker_log_line(line, true, false);
        assert!(result.is_some());
        // The escaped newline should be preserved
        assert!(result.unwrap().contains("\\n"));
    }

    #[test]
    fn test_agent_creation() {
        let _agent = Agent::new();
    }

    /// The host marks a distro machine only through the shim's cmdline
    /// contract; the System VM's cmdline carries `arcbox.*` keys too.
    #[test]
    fn the_machine_shim_key_selects_a_distro_machine() {
        let machine = "console=hvc0 root=/dev/vda ro rootfstype=erofs net.ifnames=0 \
                       init=/sbin/arcbox-machine-init arcbox.machine_rootfs=/dev/vdb \
                       arcbox.machine_data=/dev/vdc";
        assert_eq!(Guest::from_cmdline(machine), Guest::DistroMachine);
        let system_vm = "console=hvc0 root=/dev/vda ro arcbox.guest_docker_vsock_port=2375 \
                         arcbox.container_network=172.16.0.0/12";
        assert_eq!(Guest::from_cmdline(system_vm), Guest::SystemVm);
    }
}
