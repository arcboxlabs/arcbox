//! The SSH server against an in-process russh client and a scripted machine,
//! so the protocol mapping is checked without a VM.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use arcbox_connect::v1::{MachineExecOutput, MachineExecRequest};
use arcbox_engine::agent_client::ExecSessionInput;
use arcbox_ssh::{CLIENT_KEY_FILE, ExecOutput, MachineHost, SshKeys, SshServer};
use russh::keys::{Algorithm, PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use russh::{ChannelMsg, client};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

/// A machine that runs nothing: it answers from the command line.
#[derive(Default)]
struct ScriptedMachine {
    requests: Mutex<Vec<(String, MachineExecRequest)>>,
    /// Told when a session's input side closes.
    input_closed: Mutex<Option<oneshot::Sender<()>>>,
}

fn output(stream: &str, data: &[u8]) -> MachineExecOutput {
    MachineExecOutput {
        stream: stream.to_owned(),
        data: data.to_vec(),
        ..Default::default()
    }
}

fn exit(code: i32, signal: &str) -> MachineExecOutput {
    MachineExecOutput {
        done: true,
        exit_code: code,
        exit_signal: signal.to_owned(),
        ..Default::default()
    }
}

impl MachineHost for ScriptedMachine {
    async fn exec(
        &self,
        machine: &str,
        request: MachineExecRequest,
        mut input: mpsc::Receiver<ExecSessionInput>,
    ) -> anyhow::Result<ExecOutput> {
        anyhow::ensure!(machine != "missing", "no machine named 'missing'");
        self.requests
            .lock()
            .unwrap()
            .push((machine.to_owned(), request.clone()));
        let input_closed = self.input_closed.lock().unwrap().take();
        let command = request.cmd.first().cloned().unwrap_or_default();
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            let last = match command.as_str() {
                // Echo stdin until EOF.
                "cat" => loop {
                    match input.recv().await {
                        Some(ExecSessionInput::Stdin(data)) if data.is_empty() => {
                            break exit(0, "");
                        }
                        Some(ExecSessionInput::Stdin(data)) => {
                            let _ = tx.send(Ok(output("stdout", &data))).await;
                        }
                        Some(_) => {}
                        None => {
                            if let Some(closed) = input_closed {
                                let _ = closed.send(());
                            }
                            return;
                        }
                    }
                },
                "wait-resize" => loop {
                    match input.recv().await {
                        Some(ExecSessionInput::Resize { width, height }) => {
                            let size = format!("{width}x{height}");
                            let _ = tx.send(Ok(output("stdout", size.as_bytes()))).await;
                            break exit(0, "");
                        }
                        Some(_) => {}
                        None => return,
                    }
                },
                "stderr" => {
                    let _ = tx.send(Ok(output("stderr", b"oops"))).await;
                    exit(1, "")
                }
                other => exit(
                    other
                        .strip_prefix("exit ")
                        .map_or(0, |c| c.parse().unwrap()),
                    "",
                ),
            };
            let _ = tx.send(Ok(last)).await;
        });
        Ok(rx)
    }
}

struct TrustingClient;

impl client::Handler for TrustingClient {
    type Error = russh::Error;

    async fn check_server_key(&mut self, _key: &PublicKey) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

struct Fixture {
    machine: Arc<ScriptedMachine>,
    addr: SocketAddr,
    client_key: Arc<PrivateKey>,
    _dir: tempfile::TempDir,
}

async fn start_server() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let keys = SshKeys::load_or_generate(dir.path()).unwrap();
    let pem = std::fs::read_to_string(dir.path().join(CLIENT_KEY_FILE)).unwrap();
    let client_key = Arc::new(PrivateKey::from_openssh(pem).unwrap());
    let machine = Arc::new(ScriptedMachine::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = SshServer::new(Arc::clone(&machine), keys);
    tokio::spawn(server.serve(listener, std::future::pending()));
    Fixture {
        machine,
        addr,
        client_key,
        _dir: dir,
    }
}

async fn login(
    addr: SocketAddr,
    user: &str,
    key: Arc<PrivateKey>,
) -> (client::Handle<TrustingClient>, bool) {
    let config = Arc::new(client::Config::default());
    let mut handle = client::connect(config, addr, TrustingClient).await.unwrap();
    let auth = handle
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(key, None))
        .await
        .unwrap();
    (handle, auth.success())
}

/// Everything the server sends on a channel until it closes it.
async fn drain(channel: &mut russh::Channel<client::Msg>) -> Vec<ChannelMsg> {
    let mut messages = Vec::new();
    while let Some(message) = channel.wait().await {
        let closed = matches!(message, ChannelMsg::Close);
        messages.push(message);
        if closed {
            break;
        }
    }
    messages
}

fn exit_status(messages: &[ChannelMsg]) -> Option<u32> {
    messages.iter().find_map(|m| match m {
        ChannelMsg::ExitStatus { exit_status } => Some(*exit_status),
        _ => None,
    })
}

fn data(messages: &[ChannelMsg], ext: Option<u32>) -> Vec<u8> {
    messages
        .iter()
        .flat_map(|m| match (m, ext) {
            (ChannelMsg::Data { data }, None) => data.to_vec(),
            (ChannelMsg::ExtendedData { data, ext: e }, Some(want)) if *e == want => data.to_vec(),
            _ => Vec::new(),
        })
        .collect()
}

#[tokio::test]
async fn exec_runs_a_login_command_in_the_selected_machine() {
    let fixture = start_server().await;
    let (handle, ok) = login(fixture.addr, "dev@ubuntu", fixture.client_key).await;
    assert!(ok);

    let mut channel = handle.channel_open_session().await.unwrap();
    channel.set_env(true, "LANG", "C.UTF-8").await.unwrap();
    channel.exec(true, "exit 7").await.unwrap();
    let messages = drain(&mut channel).await;
    assert_eq!(exit_status(&messages), Some(7));

    let (machine, request) = fixture.machine.requests.lock().unwrap()[0].clone();
    assert_eq!(machine, "ubuntu");
    assert_eq!(request.user, "dev");
    assert_eq!(request.cmd, ["exit 7"]);
    assert!(request.login && request.attach_stdin && !request.tty);
    assert_eq!(request.env.get("LANG").map(String::as_str), Some("C.UTF-8"));
    assert!(request.env.contains_key("SSH_CONNECTION"));
}

#[tokio::test]
async fn stdin_reaches_the_process_and_eof_ends_it() {
    let fixture = start_server().await;
    let (handle, _) = login(fixture.addr, "ubuntu", fixture.client_key).await;

    let mut channel = handle.channel_open_session().await.unwrap();
    channel.exec(true, "cat").await.unwrap();
    channel.data(&b"hello"[..]).await.unwrap();
    channel.eof().await.unwrap();
    let messages = drain(&mut channel).await;
    assert_eq!(data(&messages, None), b"hello");
    assert_eq!(exit_status(&messages), Some(0));
}

#[tokio::test]
async fn stderr_arrives_as_extended_data() {
    let fixture = start_server().await;
    let (handle, _) = login(fixture.addr, "ubuntu", fixture.client_key).await;

    let mut channel = handle.channel_open_session().await.unwrap();
    channel.exec(true, "stderr").await.unwrap();
    let messages = drain(&mut channel).await;
    assert_eq!(data(&messages, Some(1)), b"oops");
    assert_eq!(exit_status(&messages), Some(1));
}

#[tokio::test]
async fn a_shell_with_a_pty_gets_the_terminal_it_asked_for() {
    let fixture = start_server().await;
    let (handle, _) = login(fixture.addr, "ubuntu", fixture.client_key).await;

    let mut channel = handle.channel_open_session().await.unwrap();
    channel
        .request_pty(true, "xterm-256color", 100, 30, 0, 0, &[])
        .await
        .unwrap();
    channel.request_shell(true).await.unwrap();
    drain(&mut channel).await;

    let (_, request) = fixture.machine.requests.lock().unwrap()[0].clone();
    assert!(request.tty && request.cmd.is_empty());
    assert_eq!((request.tty_size.width, request.tty_size.height), (100, 30));
    assert_eq!(
        request.env.get("TERM").map(String::as_str),
        Some("xterm-256color")
    );
}

#[tokio::test]
async fn window_changes_resize_the_terminal() {
    let fixture = start_server().await;
    let (handle, _) = login(fixture.addr, "ubuntu", fixture.client_key).await;

    let mut channel = handle.channel_open_session().await.unwrap();
    channel
        .request_pty(true, "xterm", 80, 24, 0, 0, &[])
        .await
        .unwrap();
    channel.exec(true, "wait-resize").await.unwrap();
    channel.window_change(120, 40, 0, 0).await.unwrap();
    let messages = drain(&mut channel).await;
    assert_eq!(data(&messages, None), b"120x40");
}

#[tokio::test]
async fn a_machine_that_cannot_run_says_why_and_exits_255() {
    let fixture = start_server().await;
    let (handle, _) = login(fixture.addr, "missing", fixture.client_key).await;

    let mut channel = handle.channel_open_session().await.unwrap();
    channel.exec(true, "true").await.unwrap();
    let messages = drain(&mut channel).await;
    let stderr = String::from_utf8(data(&messages, Some(1))).unwrap();
    assert_eq!(stderr, "arcbox: no machine named 'missing'\n");
    assert_eq!(exit_status(&messages), Some(255));
}

#[tokio::test]
async fn closing_the_channel_ends_the_session_in_the_machine() {
    let fixture = start_server().await;
    let (closed_tx, closed_rx) = oneshot::channel();
    *fixture.machine.input_closed.lock().unwrap() = Some(closed_tx);
    let (handle, _) = login(fixture.addr, "ubuntu", fixture.client_key).await;

    let channel = handle.channel_open_session().await.unwrap();
    channel.exec(true, "cat").await.unwrap();
    channel.close().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), closed_rx)
        .await
        .expect("the session's input must close with the channel")
        .unwrap();
}

#[tokio::test]
async fn any_other_key_is_refused() {
    let fixture = start_server().await;
    let stranger = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let (_, ok) = login(fixture.addr, "ubuntu", Arc::new(stranger)).await;
    assert!(!ok);
}

#[tokio::test]
async fn a_user_name_naming_no_machine_is_refused() {
    let fixture = start_server().await;
    let (_, ok) = login(fixture.addr, "dev@", fixture.client_key).await;
    assert!(!ok);
}
