//! The `sftp` subsystem: the machine's own `sftp-server`, run as the
//! login's command the way sshd runs it. scp in OpenSSH 9+ speaks SFTP too.

use arcbox_connect::v1::MachineExecRequest;
use tokio::sync::mpsc;

use crate::host::{ExecOutput, MachineHost};

/// Where distributions install `sftp-server`: Debian and Ubuntu, Fedora
/// and RHEL, Arch and Alpine.
const SFTP_SERVERS: &[&str] = &[
    "/usr/lib/openssh/sftp-server",
    "/usr/libexec/openssh/sftp-server",
    "/usr/lib/ssh/sftp-server",
    "/usr/libexec/sftp-server",
];

/// The path of `machine`'s `sftp-server`, if it has one.
///
/// # Errors
///
/// Returns an error if the machine cannot run the lookup.
pub async fn find_sftp_server<H: MachineHost>(
    host: &H,
    machine: &str,
) -> anyhow::Result<Option<String>> {
    let script = format!(
        "for p in {}; do if [ -x \"$p\" ]; then echo \"$p\"; exit 0; fi; done; exit 1",
        SFTP_SERVERS.join(" ")
    );
    let request = MachineExecRequest {
        id: machine.to_owned(),
        cmd: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
        ..Default::default()
    };
    let (_, no_input) = mpsc::channel(1);
    let mut output = host.exec(machine, request, no_input).await?;
    let mut found = Vec::new();
    while let Some(frame) = output.recv().await {
        let frame = frame?;
        if frame.stream == "stdout" {
            found.extend_from_slice(&frame.data);
        }
        if frame.done {
            let path = String::from_utf8_lossy(&found).trim().to_owned();
            return Ok((frame.exit_code == 0 && !path.is_empty()).then_some(path));
        }
    }
    anyhow::bail!("the machine session ended without an exit status")
}
