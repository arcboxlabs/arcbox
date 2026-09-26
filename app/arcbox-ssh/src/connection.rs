//! One client connection: public-key authentication against the daemon's
//! client key, then the connection's session channels.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use arcbox_engine::agent_client::ExecSessionInput;
use russh::keys::PublicKey;
use russh::server::{Auth, ChannelOpenHandle, Handler, Msg, Session};
use russh::{Channel, ChannelId, Pty, Sig};
use tokio::sync::mpsc;

use crate::host::MachineHost;
use crate::session::{Program, SessionChannel};
use crate::signal;
use crate::target::Target;

/// Input messages buffered toward one session's process. When they are all
/// in flight the connection waits for the process: russh returns window to
/// the client as soon as data arrives, so holding up the handler is the only
/// backpressure a client sees.
const INPUT_CAPACITY: usize = 64;

pub struct Connection<H> {
    host: Arc<H>,
    client_key: Arc<PublicKey>,
    /// `SSH_CLIENT` and `SSH_CONNECTION`, the way sshd sets them.
    ssh_env: Vec<(String, String)>,
    /// The login's machine and account, once authenticated.
    target: Option<Target>,
    sessions: HashMap<ChannelId, SessionChannel>,
}

impl<H: MachineHost> Connection<H> {
    pub fn new(
        host: Arc<H>,
        client_key: Arc<PublicKey>,
        peer: Option<SocketAddr>,
        local: SocketAddr,
    ) -> Self {
        let ssh_env = peer
            .map(|peer| {
                vec![
                    (
                        "SSH_CLIENT".to_owned(),
                        format!("{} {} {}", peer.ip(), peer.port(), local.port()),
                    ),
                    (
                        "SSH_CONNECTION".to_owned(),
                        format!(
                            "{} {} {} {}",
                            peer.ip(),
                            peer.port(),
                            local.ip(),
                            local.port()
                        ),
                    ),
                ]
            })
            .unwrap_or_default();
        Self {
            host,
            client_key,
            ssh_env,
            target: None,
            sessions: HashMap::new(),
        }
    }

    /// The target `user` selects, when `key` is the daemon's client key.
    fn authorize(&self, user: &str, key: &PublicKey) -> Option<Target> {
        if key.key_data() != self.client_key.key_data() {
            return None;
        }
        user.parse().ok()
    }

    /// Starts `program` on `channel` in the login's machine.
    async fn start(
        &mut self,
        channel: ChannelId,
        program: Program,
        session: &mut Session,
    ) -> Result<(), russh::Error> {
        let (Some(target), Some(state)) = (&self.target, self.sessions.get_mut(&channel)) else {
            return session.channel_failure(channel);
        };
        if state.started() {
            return session.channel_failure(channel);
        }
        let request = state.exec_request(target, program, &self.ssh_env);
        let (input, input_rx) = mpsc::channel(INPUT_CAPACITY);
        let started = self
            .host
            .exec(&target.machine, request, input_rx)
            .await
            .map(|output| (output, input));
        if let Err(e) = &started {
            tracing::info!(%target, error = %format!("{e:#}"), "ssh session could not start");
        }
        // Accepted either way: a failure reaches the client on stderr with
        // exit status 255, which says more than a refused request would.
        session.channel_success(channel)?;
        state.start(started, session.handle());
        Ok(())
    }

    /// Passes `input` to `channel`'s process, waiting while its buffer is
    /// full. Input for a channel without a process is dropped.
    async fn send_input(&self, channel: ChannelId, input: ExecSessionInput) {
        if let Some(sender) = self.sessions.get(&channel).and_then(SessionChannel::input) {
            // A closed receiver only means the process already exited.
            let _ = sender.send(input).await;
        }
    }
}

impl<H: MachineHost> Handler for Connection<H> {
    type Error = russh::Error;

    async fn auth_publickey_offered(
        &mut self,
        user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(auth(self.authorize(user, key).is_some()))
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        self.target = self.authorize(user, key);
        Ok(auth(self.target.is_some()))
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Every message for the channel also reaches this handler's
        // callbacks, so the read half would have no reader.
        let (_, writer) = channel.split();
        self.sessions
            .insert(writer.id(), SessionChannel::new(writer));
        // Accepting goes through the session's own queue, which this
        // callback is holding up: hand it off instead of waiting on a queue
        // that may be full.
        tokio::spawn(reply.accept());
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let accepted = self
            .sessions
            .get_mut(&channel)
            .is_some_and(|state| state.request_pty(term, col_width, row_height));
        reply(session, channel, accepted)
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        variable_name: &str,
        variable_value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let accepted = self
            .sessions
            .get_mut(&channel)
            .is_some_and(|state| state.request_env(variable_name, variable_value));
        reply(session, channel, accepted)
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.start(channel, Program::Shell, session).await
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        self.start(channel, Program::Command(command), session)
            .await
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let resize = ExecSessionInput::Resize {
            width: u16::try_from(col_width).unwrap_or(u16::MAX),
            height: u16::try_from(row_height).unwrap_or(u16::MAX),
        };
        self.send_input(channel, resize).await;
        Ok(())
    }

    async fn signal(
        &mut self,
        channel: ChannelId,
        signal: Sig,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let name = signal::name(&signal).to_owned();
        self.send_input(channel, ExecSessionInput::Signal(name))
            .await;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // An empty stdin message means EOF downstream; an empty SSH data
        // packet means nothing.
        if !data.is_empty() {
            self.send_input(channel, ExecSessionInput::Stdin(data.to_vec()))
                .await;
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.send_input(channel, ExecSessionInput::Stdin(Vec::new()))
            .await;
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.sessions.remove(&channel);
        Ok(())
    }
}

fn auth(accepted: bool) -> Auth {
    if accepted {
        Auth::Accept
    } else {
        Auth::reject()
    }
}

fn reply(session: &mut Session, channel: ChannelId, accepted: bool) -> Result<(), russh::Error> {
    if accepted {
        session.channel_success(channel)
    } else {
        session.channel_failure(channel)
    }
}
