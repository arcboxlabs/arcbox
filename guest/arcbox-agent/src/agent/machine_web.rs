//! Session-owned machine policies, validated public roots, and exact private DNS claims.

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, ensure};
use arcbox_connect::v1::{MachineWebAck, OpenMachineWebRequest, ReplaceMachineWebRequest};
use arcbox_dns::{NamePattern, claims::Claim};
use arcbox_local_ca::PublicAuthority;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::dns_server::DnsRegistry;
use crate::domains::{EndpointFacts, WebServices};
use crate::rpc::{
    ErrorResponse, RpcRequest, RpcResponse, parse_request, read_message, write_response,
};

#[derive(Clone)]
pub(super) struct Receiver {
    registry: DnsRegistry,
    web: WebServices,
    state: Arc<Mutex<State>>,
    authority: watch::Sender<Option<Arc<PublicAuthority>>>,
}

#[derive(Default)]
struct State {
    active: bool,
    configuration: Option<Configuration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Configuration {
    facts: EndpointFacts,
    source_revision: u64,
}

impl Receiver {
    pub(super) fn new(registry: DnsRegistry, web: WebServices) -> Self {
        Self {
            registry,
            web,
            state: Arc::default(),
            authority: watch::channel(None).0,
        }
    }

    fn open(
        &self,
        open: OpenMachineWebRequest,
        bridge: Option<Ipv4Addr>,
    ) -> Result<Option<Session>> {
        ensure!(!open.machine_name.is_empty(), "machine name is required");
        let hostname: arcbox_constants::dns::Domain =
            open.hostname.parse().map_err(anyhow::Error::msg)?;
        let address = open.ipv4_address.parse::<Ipv4Addr>()?;
        ensure!(
            Some(address) == bridge,
            "machine bridge IPv4 does not match the live interface"
        );
        let mut state = self.state.lock().expect("machine web lock poisoned");
        if state.active {
            return Ok(None);
        }
        let facts = EndpointFacts::machine(open.machine_name, hostname.clone(), address);
        let owner = facts.owner().to_owned();
        self.registry
            .write()
            .expect("DNS registry lock poisoned")
            .replace(
                owner.clone(),
                vec![Claim::exclusive(
                    NamePattern::Exact(hostname),
                    owner,
                    "machine".into(),
                )],
                Some(address.into()),
            )?;
        state.configuration = Some(Configuration {
            facts,
            source_revision: 0,
        });
        state.active = true;
        Ok(Some(Session {
            receiver: self.clone(),
            cancel: CancellationToken::new(),
        }))
    }

    pub(super) async fn serve<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        &self,
        mut stream: S,
        trace: &str,
        open: OpenMachineWebRequest,
        bridge: Option<Ipv4Addr>,
    ) -> Result<()> {
        let Some(session) = self.open(open, bridge)? else {
            return write_response(
                &mut stream,
                &RpcResponse::Error(ErrorResponse::new(409, "machine web connection is busy")),
                trace,
            )
            .await;
        };
        let session = Arc::new(session);
        let _connection = Connection(Arc::clone(&session));
        let trace = trace.to_owned();
        // The owned task finishes shutdown even when the RPC caller is canceled.
        tokio::spawn(async move { session.serve(stream, &trace).await })
            .await
            .context("machine web connection task failed")?
    }
}

struct Connection(Arc<Session>);

impl Drop for Connection {
    fn drop(&mut self) {
        self.0.withdraw();
    }
}

struct Session {
    receiver: Receiver,
    cancel: CancellationToken,
}

impl Session {
    async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut stream: S,
        trace: &str,
    ) -> Result<()> {
        let protocol = async {
            write_response(
                &mut stream,
                &RpcResponse::OpenMachineWeb(Default::default()),
                trace,
            )
            .await?;
            loop {
                let (kind, trace, payload) = read_message(&mut stream).await?;
                let RpcRequest::ReplaceMachineWeb(request) = parse_request(kind, &payload)? else {
                    anyhow::bail!("machine web connection requires a complete replacement");
                };
                let source_revision = self.replace(request)?;
                write_response(
                    &mut stream,
                    &RpcResponse::MachineWebAck(MachineWebAck {
                        source_revision,
                        ..Default::default()
                    }),
                    &trace,
                )
                .await?;
            }
        };
        let result: Result<()> = tokio::select! {
            biased;
            () = self.cancel.cancelled() => Ok(()),
            result = protocol => result,
        };
        self.withdraw();
        let shutdown = stream
            .shutdown()
            .await
            .context("shut down machine web connection")
            .inspect_err(|error| tracing::error!(%error, "machine web shutdown failed"));
        // Retain exclusivity until the real stream has finished shutdown.
        match (result, shutdown) {
            (Err(error), Err(shutdown)) => {
                Err(error.context(format!("shutdown also failed: {shutdown:#}")))
            }
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
    fn replace(&self, request: ReplaceMachineWebRequest) -> Result<u64> {
        let mut state = self
            .receiver
            .state
            .lock()
            .expect("machine web lock poisoned");
        let configuration = state
            .configuration
            .as_mut()
            .context("machine web session ended")?;
        let revision = request.source_revision;
        ensure!(
            revision > configuration.source_revision,
            "source revision must increase"
        );
        let authority = request
            .ca_certificate_pem
            .as_deref()
            .map(PublicAuthority::from_pem)
            .transpose()?
            .map(Arc::new);
        configuration.facts.replace_machine_ports(&request.ports)?;
        self.receiver.web.clear();
        self.receiver.authority.send_replace(authority);
        configuration.source_revision = revision;
        Ok(revision)
    }

    fn withdraw(&self) {
        let mut state = self
            .receiver
            .state
            .lock()
            .expect("machine web lock poisoned");
        if let Some(configuration) = state.configuration.take() {
            self.receiver
                .registry
                .write()
                .expect("DNS registry lock poisoned")
                .remove(configuration.facts.owner());
        }
        self.receiver.web.clear();
        self.receiver.authority.send_replace(None);
        self.cancel.cancel();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.withdraw();
        self.receiver
            .state
            .lock()
            .expect("machine web lock poisoned")
            .active = false;
    }
}

#[cfg(test)]
mod tests;
