//! Collect Docker command output and reap the child before reporting cancellation.

use std::io;
use std::process::{Output, Stdio};

use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::error::{MigrationError, Result};

pub(super) async fn run(mut command: Command, cancelled: &CancellationToken) -> Result<Output> {
    if cancelled.is_cancelled() {
        return Err(interrupted());
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(command_error)?;
    let mut stdout_pipe = child.stdout.take().expect("stdout is configured as piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr is configured as piped");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let completed = tokio::select! {
        biased;
        output = async {
            tokio::join!(
                child.wait(),
                stdout_pipe.read_to_end(&mut stdout),
                stderr_pipe.read_to_end(&mut stderr),
            )
        } => Some(output),
        () = cancelled.cancelled() => None,
    };
    let Some((waited, stdout_read, stderr_read)) = completed else {
        // Descendants can retain the pipes after the direct child exits.
        drop(stdout_pipe);
        drop(stderr_pipe);
        let killed = child.start_kill();
        let waited = child.wait().await;
        return match (killed, waited) {
            (Ok(()), Ok(_)) => Err(interrupted()),
            (Err(kill), Err(wait)) => Err(MigrationError::Docker(format!(
                "failed to kill docker: {kill}; failed to reap docker: {wait}"
            ))),
            (Err(error), Ok(_)) => Err(MigrationError::Docker(format!(
                "failed to kill docker: {error}"
            ))),
            (Ok(()), Err(error)) => Err(MigrationError::Docker(format!(
                "failed to reap docker: {error}"
            ))),
        };
    };
    let status = waited.map_err(command_error)?;
    stdout_read.map_err(command_error)?;
    stderr_read.map_err(command_error)?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn interrupted() -> MigrationError {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "migration preparation cancelled",
    )
    .into()
}

fn command_error(error: io::Error) -> MigrationError {
    MigrationError::Docker(format!("failed to run docker: {error}"))
}
