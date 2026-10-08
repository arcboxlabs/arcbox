//! Serialize guest-owned trust bundle writes through durable installation.

mod machine;

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use arcbox_connect::v1::{
    ReconcileGuestTrustRequest, ReconcileGuestTrustResponse,
    reconcile_guest_trust_request::Desired, reconcile_guest_trust_response::Applied,
};
use arcbox_local_ca::{PublicAuthority, install_trust_bundle};
use tokio::sync::Mutex;

use crate::rpc::{ErrorResponse, RpcResponse};

#[derive(Clone)]
pub(super) struct Store {
    state: Arc<Mutex<State>>,
    next_connection: Arc<AtomicU64>,
    #[cfg(test)]
    gate: Arc<std::sync::Mutex<Option<WriteGate>>>,
}

struct State {
    destination: Destination,
    accepted: u64,
}

enum Destination {
    Bundle(PathBuf),
    Machine(PathBuf),
}

impl Destination {
    fn install(&self, authority: Option<&PublicAuthority>) -> anyhow::Result<()> {
        match self {
            Self::Bundle(path) => Ok(install_trust_bundle(path, authority)?),
            Self::Machine(root) => machine::install(root, authority),
        }
    }
}

#[cfg(test)]
struct WriteGate {
    entered: tokio::sync::oneshot::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

impl Store {
    pub(super) fn new(path: PathBuf) -> Self {
        Self::with_destination(Destination::Bundle(path))
    }

    pub(super) fn machine(root: PathBuf) -> Self {
        Self::with_destination(Destination::Machine(root))
    }

    fn with_destination(destination: Destination) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                destination,
                accepted: 0,
            })),
            next_connection: Arc::default(),
            #[cfg(test)]
            gate: Arc::default(),
        }
    }

    pub(super) fn connection_order(&self) -> anyhow::Result<u64> {
        self.next_connection
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |order| {
                order.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("guest trust connection order exhausted"))
    }

    pub(super) async fn reconcile(
        &self,
        order: u64,
        request: ReconcileGuestTrustRequest,
    ) -> RpcResponse {
        let Some(desired) = request.desired else {
            return RpcResponse::Error(ErrorResponse::new(
                400,
                "guest trust desired state is unset",
            ));
        };
        // A closed older connection may still dispatch its buffered request after a newer one.
        let mut state = Arc::clone(&self.state).lock_owned().await;
        if order < state.accepted {
            return RpcResponse::Error(ErrorResponse::new(409, "guest trust connection is stale"));
        }
        state.accepted = order;
        #[cfg(test)]
        let gate = self.gate.lock().unwrap().take();
        let result = tokio::task::spawn_blocking(move || -> Result<Applied, ErrorResponse> {
            #[cfg(test)]
            if let Some(gate) = gate {
                gate.entered.send(()).unwrap();
                gate.resume.recv().unwrap();
            }
            let authority = match desired {
                Desired::CertificatePem(pem) => Some(
                    PublicAuthority::from_pem(&pem)
                        .map_err(|error| ErrorResponse::new(400, error.to_string()))?,
                ),
                Desired::Remove(_) => None,
            };
            state
                .destination
                .install(authority.as_ref())
                .map_err(|error| {
                    let code = if matches!(
                        error.downcast_ref::<arcbox_local_ca::Error>(),
                        Some(arcbox_local_ca::Error::InvalidAuthority(_))
                    ) {
                        400
                    } else {
                        500
                    };
                    ErrorResponse::new(code, format!("{error:#}"))
                })?;
            Ok(match authority {
                Some(authority) => Applied::InstalledSha256(authority.fingerprint_sha256()),
                None => Applied::Removed(Default::default()),
            })
        })
        .await;
        match result {
            Ok(Ok(applied)) => RpcResponse::ReconcileGuestTrust(ReconcileGuestTrustResponse {
                applied: Some(applied),
                ..Default::default()
            }),
            Ok(Err(error)) => RpcResponse::Error(error),
            Err(error) => RpcResponse::Error(ErrorResponse::new(500, error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_retains_serialization_until_the_old_write_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bundle.pem");
        std::fs::write(&path, b"# Distro roots\n").unwrap();
        let old_pem = include_bytes!("linux/rpc/guest_trust_ca.pem");
        let new_pem = include_bytes!("linux/rpc/guest_trust_next_ca.pem");
        let request = |pem: &[u8]| ReconcileGuestTrustRequest {
            desired: Some(Desired::CertificatePem(pem.to_vec())),
            ..Default::default()
        };
        let store = Store::new(path.clone());
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, resume) = std::sync::mpsc::channel();
        *store.gate.lock().unwrap() = Some(WriteGate { entered, resume });
        let old_store = store.clone();
        let old_order = store.connection_order().unwrap();
        let new_order = store.connection_order().unwrap();
        let old_request = request(old_pem);
        let old = tokio::spawn(async move { old_store.reconcile(old_order, old_request).await });
        tokio::time::timeout(std::time::Duration::from_secs(3), waiting)
            .await
            .unwrap()
            .unwrap();
        old.abort();
        assert!(old.await.unwrap_err().is_cancelled());
        assert!(store.state.try_lock().is_err());
        let mut newer = std::pin::pin!(store.reconcile(new_order, request(new_pem)));
        assert!(futures::poll!(newer.as_mut()).is_pending());
        release.send(()).unwrap();
        let response = tokio::time::timeout(std::time::Duration::from_secs(3), newer)
            .await
            .unwrap();
        assert!(matches!(response, RpcResponse::ReconcileGuestTrust(_)));
        let installed = std::fs::read_to_string(&path).unwrap();
        assert!(installed.contains(std::str::from_utf8(new_pem).unwrap().trim()));
        assert!(!installed.contains(std::str::from_utf8(old_pem).unwrap().trim()));
    }

    #[tokio::test]
    async fn a_late_older_connection_cannot_replace_a_newer_root() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bundle.pem");
        std::fs::write(&path, b"# Distro roots\n").unwrap();
        let store = Store::new(path.clone());
        let old = store.connection_order().unwrap();
        let new = store.connection_order().unwrap();
        let request = |pem: &[u8]| ReconcileGuestTrustRequest {
            desired: Some(Desired::CertificatePem(pem.to_vec())),
            ..Default::default()
        };
        let response = store
            .reconcile(
                new,
                request(include_bytes!("linux/rpc/guest_trust_next_ca.pem")),
            )
            .await;
        assert!(matches!(response, RpcResponse::ReconcileGuestTrust(_)));
        let installed = std::fs::read(&path).unwrap();
        let response = store
            .reconcile(old, request(include_bytes!("linux/rpc/guest_trust_ca.pem")))
            .await;
        assert!(matches!(response, RpcResponse::Error(error) if error.code == 409));
        assert_eq!(std::fs::read(&path).unwrap(), installed);
    }
}
