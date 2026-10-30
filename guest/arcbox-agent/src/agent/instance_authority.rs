//! Exclusive host connection ownership for the shared guest DNS registry.

use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, ensure};
use arcbox_connect::v1::instance_authority_root::Target;
use arcbox_connect::v1::{InstanceAuthorityAck, ReplaceInstanceAuthorityRequest};
use arcbox_dns::registry::{InstanceAuthority, InstanceTarget};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::dns_server::DnsRegistry;
use crate::rpc::{
    ErrorResponse, RpcRequest, RpcResponse, parse_request, read_message, write_response,
};

/// Connection admission and the shared registry for one System VM.
#[derive(Clone)]
pub(super) struct Receiver {
    registry: DnsRegistry,
    active: Arc<Mutex<bool>>,
}

impl Receiver {
    /// Binds connection ownership to the System VM's existing registry.
    pub(super) fn new(registry: DnsRegistry) -> Self {
        Self {
            registry,
            active: Arc::new(Mutex::new(false)),
        }
    }

    fn open(&self) -> Option<Session> {
        let mut active = self
            .active
            .lock()
            .expect("authority receiver lock poisoned");
        if *active {
            return None;
        }
        *active = true;
        Some(Session {
            receiver: self.clone(),
            revision: None,
        })
    }

    /// Owns the stream until loss, cancellation, or an invalid replacement revokes authority.
    pub(super) async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: &mut S,
        trace: &str,
    ) -> Result<()> {
        let Some(mut session) = self.open() else {
            return write_response(
                stream,
                &RpcResponse::Error(ErrorResponse::new(
                    409,
                    "instance authority connection is busy",
                )),
                trace,
            )
            .await;
        };
        write_response(
            stream,
            &RpcResponse::OpenInstanceAuthority(Default::default()),
            trace,
        )
        .await?;
        loop {
            let (kind, trace, payload) = read_message(stream).await?;
            let RpcRequest::ReplaceInstanceAuthority(request) = parse_request(kind, &payload)?
            else {
                anyhow::bail!("authority connection requires a complete replacement");
            };
            let authority_revision = session.replace(request)?;
            write_response(
                stream,
                &RpcResponse::InstanceAuthorityAck(InstanceAuthorityAck {
                    authority_revision,
                    ..Default::default()
                }),
                &trace,
            )
            .await?;
        }
    }
}

struct Session {
    receiver: Receiver,
    revision: Option<u64>,
}

impl Session {
    fn replace(&mut self, request: ReplaceInstanceAuthorityRequest) -> Result<u64> {
        ensure!(
            self.revision
                .is_none_or(|last| request.authority_revision > last),
            "authority revision must increase within the connection"
        );
        let mut roots = request
            .roots
            .into_iter()
            .map(|wire| {
                let root = wire
                    .root
                    .to_ascii_lowercase()
                    .parse()
                    .map_err(anyhow::Error::msg)?;
                let target = match (wire.target.as_known(), wire.dns_port) {
                    (Some(Target::Current), 0) => InstanceTarget::Current,
                    (Some(Target::Unavailable), 0) => InstanceTarget::Unavailable,
                    (Some(Target::Other), port) => {
                        let port = u16::try_from(port).context("invalid host DNS port")?;
                        ensure!(port != 0, "host DNS port must be nonzero");
                        InstanceTarget::Other { port }
                    }
                    _ => anyhow::bail!("invalid instance target or host DNS port"),
                };
                Ok(InstanceAuthority { root, target })
            })
            .collect::<Result<Vec<_>>>()?;
        roots.sort_by(|a, b| a.root.as_str().cmp(b.root.as_str()));
        ensure!(
            !roots.windows(2).any(|pair| pair[0].root == pair[1].root),
            "duplicate instance root"
        );
        let mut registry = self
            .receiver
            .registry
            .write()
            .expect("DNS registry lock poisoned");
        if self.revision.is_some() {
            // A coalesced source ABA may have identical roots but a different lifetime.
            registry.invalidate_instance_authorities();
        }
        registry.replace_instance_authorities(roots);
        self.revision = Some(request.authority_revision);
        Ok(request.authority_revision)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let mut active = self
            .receiver
            .active
            .lock()
            .expect("authority receiver lock poisoned");
        self.receiver
            .registry
            .write()
            .expect("DNS registry lock poisoned")
            .invalidate_instance_authorities();
        *active = false;
    }
}
