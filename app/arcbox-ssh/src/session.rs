//! A session channel: the requests that shape its process (`pty-req`,
//! `env`), the one that starts it (`shell`, `exec`), and then the process's
//! output streaming back.

use std::collections::HashMap;

use arcbox_connect::v1::{MachineExecOutput, MachineExecRequest, TerminalSize};
use arcbox_engine::agent_client::ExecSessionInput;
use russh::ChannelWriteHalf;
use russh::server::Msg;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::host::ExecOutput;
use crate::target::Target;

/// SSH extended-data stream number of stderr (RFC 4254 §5.2).
const STDERR: u32 = 1;
/// Exit status reported when a session could not run or lost its process.
const EXIT_FAILURE: u32 = 255;

/// What a session runs once started.
pub enum Program {
    /// The account's login shell (`shell`).
    Shell,
    /// A command line for the account's shell (`exec`, and subsystems).
    Command(String),
}

/// The terminal a `pty-req` asked for.
struct Pty {
    term: String,
    cols: u32,
    rows: u32,
}

pub struct SessionChannel {
    /// Output side, until the process starts and its output task takes it.
    writer: Option<ChannelWriteHalf<Msg>>,
    pty: Option<Pty>,
    env: HashMap<String, String>,
    /// Input side of the running process.
    input: Option<mpsc::Sender<ExecSessionInput>>,
    /// The output task. Aborted when the channel goes away — closed by the
    /// client, or with the whole connection — which drops the session's
    /// output receiver and so ends the process in the machine.
    output_task: Option<AbortHandle>,
}

impl SessionChannel {
    pub fn new(writer: ChannelWriteHalf<Msg>) -> Self {
        Self {
            writer: Some(writer),
            pty: None,
            env: HashMap::new(),
            input: None,
            output_task: None,
        }
    }

    /// Whether a process was started (or failed to start) on this channel.
    pub fn started(&self) -> bool {
        self.writer.is_none()
    }

    /// Records a `pty-req`; refused once the process runs.
    pub fn request_pty(&mut self, term: &str, cols: u32, rows: u32) -> bool {
        if self.started() {
            return false;
        }
        self.pty = Some(Pty {
            term: term.to_owned(),
            cols,
            rows,
        });
        true
    }

    /// Records an `env` request; refused once the process runs, or for a
    /// name no environment can hold.
    pub fn request_env(&mut self, name: &str, value: &str) -> bool {
        let valid = !name.is_empty() && !name.contains(['=', '\0']) && !value.contains('\0');
        if self.started() || !valid {
            return false;
        }
        self.env.insert(name.to_owned(), value.to_owned());
        true
    }

    /// The machine exec request that runs `program` for `target` as a login
    /// session with this channel's terminal and environment; `ssh_env`
    /// (the `SSH_*` variables) goes on top of the client's.
    pub fn exec_request(
        &self,
        target: &Target,
        program: Program,
        ssh_env: &[(String, String)],
    ) -> MachineExecRequest {
        let mut env = self.env.clone();
        env.extend(ssh_env.iter().cloned());
        if let Some(pty) = &self.pty {
            env.insert("TERM".to_owned(), pty.term.clone());
        }
        let tty_size = self
            .pty
            .as_ref()
            .filter(|pty| pty.cols > 0 && pty.rows > 0)
            .map(|pty| TerminalSize {
                width: pty.cols,
                height: pty.rows,
                ..Default::default()
            });
        MachineExecRequest {
            id: target.machine.clone(),
            cmd: match program {
                Program::Shell => Vec::new(),
                Program::Command(line) => vec![line],
            },
            user: target.user.clone().unwrap_or_default(),
            env: env.into_iter().collect(),
            tty: self.pty.is_some(),
            tty_size: tty_size.into(),
            attach_stdin: true,
            login: true,
            ..Default::default()
        }
    }

    /// Hands the channel to its process: from here on the client's input
    /// goes to `input`, and a task streams `output` back until the exit
    /// status. A process that failed to start reports why on stderr and
    /// exits 255, the way sshd reports a shell it could not exec.
    pub fn start(&mut self, started: anyhow::Result<(ExecOutput, mpsc::Sender<ExecSessionInput>)>) {
        let Some(writer) = self.writer.take() else {
            return;
        };
        let newline = if self.pty.is_some() { "\r\n" } else { "\n" };
        let task = match started {
            Ok((output, input)) => {
                self.input = Some(input);
                tokio::spawn(forward_output(output, writer, newline))
            }
            Err(e) => {
                let exit = Exit::Failed(format!("{e:#}"));
                tokio::spawn(finish(writer, exit, newline))
            }
        };
        self.output_task = Some(task.abort_handle());
    }

    /// Where the client's input for the running process goes.
    pub fn input(&self) -> Option<mpsc::Sender<ExecSessionInput>> {
        self.input.clone()
    }
}

impl Drop for SessionChannel {
    fn drop(&mut self) {
        if let Some(task) = &self.output_task {
            task.abort();
        }
    }
}

/// How a session ended.
enum Exit {
    Status(u32),
    /// The session broke before the process reported an exit.
    Failed(String),
}

impl From<&MachineExecOutput> for Exit {
    fn from(last: &MachineExecOutput) -> Self {
        Self::Status(u32::try_from(last.exit_code).unwrap_or(EXIT_FAILURE))
    }
}

/// Streams the process's output to the client — stderr as extended data —
/// then reports how it ended and closes the channel.
async fn forward_output(
    mut output: ExecOutput,
    writer: ChannelWriteHalf<Msg>,
    newline: &'static str,
) {
    let exit = loop {
        match output.recv().await {
            Some(Ok(mut frame)) => {
                if !frame.data.is_empty() {
                    let data = std::mem::take(&mut frame.data);
                    let sent = if frame.stream == "stderr" {
                        writer.extended_data_bytes(STDERR, data).await
                    } else {
                        writer.data_bytes(data).await
                    };
                    if sent.is_err() {
                        // The connection is gone; nobody is left to tell.
                        return;
                    }
                }
                if frame.done {
                    break Exit::from(&frame);
                }
            }
            Some(Err(e)) => break Exit::Failed(e.to_string()),
            None => break Exit::Failed("the machine session ended without an exit status".into()),
        }
    };
    finish(writer, exit, newline).await;
}

/// Reports `exit` and closes the channel. Send failures are ignored: they
/// only mean the client already went away.
async fn finish(writer: ChannelWriteHalf<Msg>, exit: Exit, newline: &str) {
    match exit {
        Exit::Status(code) => {
            let _ = writer.exit_status(code).await;
        }
        Exit::Failed(message) => {
            let message = format!("arcbox: {message}{newline}");
            let _ = writer.extended_data_bytes(STDERR, message).await;
            let _ = writer.exit_status(EXIT_FAILURE).await;
        }
    }
    let _ = writer.eof().await;
    let _ = writer.close().await;
}
