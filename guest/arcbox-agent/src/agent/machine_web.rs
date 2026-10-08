//! Exclusive machine configuration and exact private DNS ownership for one host connection.

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, ensure};
use arcbox_connect::v1::{MachineWebAck, OpenMachineWebRequest, ReplaceMachineWebRequest};
use arcbox_dns::{NamePattern, claims::Claim};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};

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
        }))
    }

    pub(super) async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: &mut S,
        trace: &str,
        open: OpenMachineWebRequest,
        bridge: Option<Ipv4Addr>,
    ) -> Result<()> {
        let Some(session) = self.open(open, bridge)? else {
            return write_response(
                stream,
                &RpcResponse::Error(ErrorResponse::new(409, "machine web connection is busy")),
                trace,
            )
            .await;
        };
        let result: Result<()> = async {
            write_response(
                stream,
                &RpcResponse::OpenMachineWeb(Default::default()),
                trace,
            )
            .await?;
            loop {
                let (kind, trace, payload) = read_message(stream).await?;
                let RpcRequest::ReplaceMachineWeb(request) = parse_request(kind, &payload)? else {
                    anyhow::bail!("machine web connection requires a complete replacement");
                };
                let source_revision = session.replace(request)?;
                write_response(
                    stream,
                    &RpcResponse::MachineWebAck(MachineWebAck {
                        source_revision,
                        ..Default::default()
                    }),
                    &trace,
                )
                .await?;
            }
        }
        .await;
        session.withdraw();
        let shutdown = stream
            .shutdown()
            .await
            .context("shut down machine web connection");
        // Retain exclusivity until the real stream has finished shutdown.
        match (result, shutdown) {
            (Err(error), Err(shutdown)) => {
                Err(error.context(format!("shutdown also failed: {shutdown:#}")))
            }
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
}

struct Session {
    receiver: Receiver,
}

impl Session {
    fn replace(&self, request: ReplaceMachineWebRequest) -> Result<u64> {
        let mut state = self
            .receiver
            .state
            .lock()
            .expect("machine web lock poisoned");
        let configuration = state
            .configuration
            .as_mut()
            .expect("session owns configuration");
        let revision = request.source_revision;
        ensure!(
            revision > configuration.source_revision,
            "source revision must increase"
        );
        configuration.facts.replace_machine_ports(&request.ports)?;
        self.receiver.web.clear();
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
