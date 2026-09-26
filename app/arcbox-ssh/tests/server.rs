//! The SSH server against an in-process russh client and a scripted machine,
//! so the protocol mapping is checked without a VM.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use arcbox_connect::v1::{MachineExecOutput, MachineExecRequest};
use arcbox_engine::agent_client::ExecSessionInput;
use arcbox_ssh::{CLIENT_KEY_FILE, ExecOutput, MachineHost, SshKeys, SshServer};
use russh::keys::{Algorithm, PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use russh::{ChannelMsg, ChannelOpenFailure, Sig, client};
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

/// A scripted session's output.
struct Scripted(mpsc::Receiver<arcbox_engine::Result<MachineExecOutput>>);

impl ExecOutput for Scripted {
    fn recv(
        &mut self,
    ) -> impl Future<Output = Option<arcbox_engine::Result<MachineExecOutput>>> + Send {
        self.0.recv()
    }
}

impl MachineHost for ScriptedMachine {
    type Output = Scripted;

    async fn exec(
        &self,
        machine: &str,
        request: MachineExecRequest,
        mut input: mpsc::Receiver<ExecSessionInput>,
    ) -> anyhow::Result<Scripted> {
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
                "wait-signal" => loop {
                    match input.recv().await {
                        Some(ExecSessionInput::Signal(name)) => break exit(-1, &name),
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
        Ok(Scripted(rx))
    }

    /// A peer that greets, then echoes until the client stops sending —
    /// or, as host `closer`, stops sending first and then waits for the
    /// client to stop too.
    async fn connect_tcp(
        &self,
        machine: &str,
        host: &str,
        port: u16,
        mut input: mpsc::Receiver<ExecSessionInput>,
    ) -> anyhow::Result<Scripted> {
        anyhow::ensure!(host != "refused", "connect to {host}:{port}: refused");
        let greeting = format!("{machine} {host}:{port}\n");
        let closer = host == "closer";
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            let _ = tx.send(Ok(output("stdout", greeting.as_bytes()))).await;
            if closer {
                let _ = tx.send(Ok(eof())).await;
            }
            while let Some(ExecSessionInput::Stdin(data)) = input.recv().await {
                if data.is_empty() {
                    break;
                }
                if !closer {
                    let _ = tx.send(Ok(output("stdout", &data))).await;
                }
            }
            if !closer {
                let _ = tx.send(Ok(eof())).await;
            }
            let _ = tx.send(Ok(exit(0, ""))).await;
        });
        Ok(Scripted(rx))
    }
}

fn eof() -> MachineExecOutput {
    MachineExecOutput {
        eof: true,
        ..Default::default()
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
async fn a_signal_reaches_the_process_and_its_death_is_reported() {
    let fixture = start_server().await;
    let (handle, _) = login(fixture.addr, "ubuntu", fixture.client_key).await;

    let mut channel = handle.channel_open_session().await.unwrap();
    channel.exec(true, "wait-signal").await.unwrap();
    // USR2 has no russh variant: it must still travel by name both ways.
    channel
        .signal(Sig::Custom("USR2".to_owned()))
        .await
        .unwrap();
    let messages = drain(&mut channel).await;
    let signal = messages.iter().find_map(|m| match m {
        ChannelMsg::ExitSignal { signal_name, .. } => Some(format!("{signal_name:?}")),
        _ => None,
    });
    assert_eq!(signal.as_deref(), Some(r#"Custom("USR2")"#));
    assert_eq!(exit_status(&messages), None);
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

#[tokio::test]
async fn a_forward_reaches_a_port_in_the_machine() {
    let fixture = start_server().await;
    let (handle, _) = login(fixture.addr, "dev@ubuntu", fixture.client_key).await;

    let mut channel = handle
        .channel_open_direct_tcpip("localhost", 8080, "127.0.0.1", 50000)
        .await
        .unwrap();
    channel.data(&b"ping"[..]).await.unwrap();
    channel.eof().await.unwrap();
    let messages = drain(&mut channel).await;
    assert_eq!(data(&messages, None), b"ubuntu localhost:8080\nping");
    assert!(messages.iter().any(|m| matches!(m, ChannelMsg::Eof)));
    assert_eq!(exit_status(&messages), None, "a forward has no exit status");
}

#[tokio::test]
async fn a_forward_the_machine_cannot_connect_is_refused() {
    let fixture = start_server().await;
    let (handle, _) = login(fixture.addr, "ubuntu", fixture.client_key).await;

    let refused = handle
        .channel_open_direct_tcpip("refused", 80, "127.0.0.1", 50000)
        .await
        .expect_err("the channel must not open");
    assert!(
        matches!(
            refused,
            russh::Error::ChannelOpenFailure(ChannelOpenFailure::ConnectFailed)
        ),
        "{refused:?}"
    );
    // The connection carries on.
    let mut channel = handle.channel_open_session().await.unwrap();
    channel.exec(true, "exit 3").await.unwrap();
    assert_eq!(exit_status(&drain(&mut channel).await), Some(3));
}

#[tokio::test]
async fn a_peer_that_stops_sending_reaches_the_client_as_eof() {
    let fixture = start_server().await;
    let (handle, _) = login(fixture.addr, "ubuntu", fixture.client_key).await;

    let mut channel = handle
        .channel_open_direct_tcpip("closer", 80, "127.0.0.1", 50000)
        .await
        .unwrap();
    // The peer's EOF arrives while the client has not stopped sending...
    let mut received = Vec::new();
    loop {
        match channel.wait().await.expect("EOF before the channel closes") {
            ChannelMsg::Data { data } => received.extend_from_slice(&data),
            ChannelMsg::Eof => break,
            ChannelMsg::Close => panic!("closed before the client stopped sending"),
            _ => {}
        }
    }
    assert_eq!(received, b"ubuntu closer:80\n");
    // ...which it still may, until it stops too.
    channel.data(&b"late"[..]).await.unwrap();
    channel.eof().await.unwrap();
    assert!(matches!(
        drain(&mut channel).await.last(),
        Some(ChannelMsg::Close)
    ));
}
