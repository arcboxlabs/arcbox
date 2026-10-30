//! Exercise trust authorization and durable installation through real RPC frames.

use std::{fs, os::fd::AsRawFd as _, path::Path};

use arcbox_connect::v1::{
    ReconcileGuestTrustRequest, ReconcileGuestTrustResponse,
    reconcile_guest_trust_request::Desired, reconcile_guest_trust_response::Applied,
};
use arcbox_local_ca::PublicAuthority;
use tokio::io::AsyncWriteExt as _;

use super::*;
use crate::agent::{Services, guest_trust::Store};
use crate::rpc::MessageType;

// Public test root only; its temporary signing key was deleted after issuance.
const CA_PEM: &[u8] = include_bytes!("guest_trust_ca.pem");

fn services(path: &Path) -> Services {
    Services {
        trust: Some(Store::new(path.to_path_buf())),
        ..Default::default()
    }
}

fn payload(desired: Option<Desired>) -> Vec<u8> {
    ReconcileGuestTrustRequest {
        desired,
        ..Default::default()
    }
    .encode_to_vec()
}

async fn exchange(
    services: Services,
    guest: Guest,
    cid: u32,
    payload: &[u8],
) -> (MessageType, Vec<u8>) {
    let (mut client, server) = tokio::io::duplex(8192);
    let dispatcher =
        tokio::spawn(async move { handle_connection(server, guest, &services, cid).await });
    let (kind, trace, response) = tokio::time::timeout(Duration::from_secs(3), async {
        write_message(
            &mut client,
            MessageType::ReconcileGuestTrustRequest,
            "guest-trust",
            payload,
        )
        .await
        .unwrap();
        read_message(&mut client).await.unwrap()
    })
    .await
    .unwrap();
    assert_eq!(trace, "guest-trust");
    drop(client);
    dispatcher.await.unwrap().unwrap();
    (kind, response)
}

#[tokio::test]
async fn host_acknowledges_installed_bytes_and_explicit_removal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("system-bundle.pem");
    let baseline = [b"# Unmarked root\n".as_slice(), CA_PEM].concat();
    fs::write(&path, &baseline).unwrap();
    let services = services(&path);
    let root = PublicAuthority::from_pem(CA_PEM).unwrap();
    let install = payload(Some(Desired::CertificatePem(CA_PEM.to_vec())));
    let (kind, response) = exchange(
        services.clone(),
        Guest::SystemVm,
        tokio_vsock::VMADDR_CID_HOST,
        &install,
    )
    .await;
    assert_eq!(kind, MessageType::ReconcileGuestTrustResponse);
    assert_eq!(
        ReconcileGuestTrustResponse::decode_from_slice(&response)
            .unwrap()
            .applied,
        Some(Applied::InstalledSha256(root.fingerprint_sha256()))
    );
    let installed = fs::read(&path).unwrap();
    assert!(installed.starts_with(&baseline));
    let text = std::str::from_utf8(&installed).unwrap();
    let owned = text
        .split_once("\n# BEGIN ARCBOX LOCAL CA\n")
        .unwrap()
        .1
        .strip_suffix("# END ARCBOX LOCAL CA\n")
        .unwrap();
    assert_eq!(
        PublicAuthority::from_pem(owned.as_bytes()).unwrap().der(),
        root.der()
    );

    let tail = b"# User additions\n";
    fs::write(&path, [installed.as_slice(), tail].concat()).unwrap();
    let remove = payload(Some(Desired::Remove(Default::default())));
    let (kind, response) = exchange(
        services,
        Guest::SystemVm,
        tokio_vsock::VMADDR_CID_HOST,
        &remove,
    )
    .await;
    assert_eq!(kind, MessageType::ReconcileGuestTrustResponse);
    assert_eq!(
        ReconcileGuestTrustResponse::decode_from_slice(&response)
            .unwrap()
            .applied,
        Some(Applied::Removed(Default::default()))
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        [baseline.as_slice(), tail].concat()
    );
}

#[tokio::test]
async fn rejection_frames_preserve_the_store() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("system-bundle.pem");
    let original = b"# Existing distro roots\n";
    fs::write(&path, original).unwrap();
    let services = services(&path);
    let host = tokio_vsock::VMADDR_CID_HOST;
    for (guest, cid, request, code) in [
        (Guest::SystemVm, host + 1, vec![0xff], 403),
        (Guest::StorageRecovery, host, vec![0xff], 403),
        (Guest::SystemVm, host, vec![0xff], 400),
        (Guest::SystemVm, host, payload(None), 400),
        (
            Guest::SystemVm,
            host,
            payload(Some(Desired::CertificatePem(b"invalid CA".to_vec()))),
            400,
        ),
    ] {
        let (kind, response) = exchange(services.clone(), guest, cid, &request).await;
        assert_eq!(kind, MessageType::Error);
        assert_eq!(ErrorResponse::decode(&response).unwrap().code, code);
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}

#[tokio::test]
async fn unavailable_or_failed_stores_never_acknowledge_installation() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing.pem");
    let path = directory.path().join("readable.pem");
    fs::write(&path, b"# Existing roots\n").unwrap();
    let file = fs::File::open(&path).unwrap();
    // Procfs exposes the temporary file for reading but forbids atomic replacement.
    let read_only = format!("/proc/self/fd/{}", file.as_raw_fd());
    let install = payload(Some(Desired::CertificatePem(CA_PEM.to_vec())));
    for (services, guest, code) in [
        (Services::default(), Guest::DistroMachine, 503),
        (services(&missing), Guest::SystemVm, 500),
        (services(Path::new(&read_only)), Guest::SystemVm, 500),
    ] {
        let (kind, response) =
            exchange(services, guest, tokio_vsock::VMADDR_CID_HOST, &install).await;
        assert_eq!(kind, MessageType::Error);
        assert_eq!(ErrorResponse::decode(&response).unwrap().code, code);
        assert!(!missing.exists());
        assert_eq!(fs::read(&path).unwrap(), b"# Existing roots\n");
    }
}

#[derive(Default)]
struct ReadGate {
    paused: std::sync::atomic::AtomicBool,
    waker: futures::task::AtomicWaker,
}

struct PausedRead<R> {
    inner: R,
    gate: std::sync::Arc<ReadGate>,
}

impl<R: AsyncRead + Unpin> AsyncRead for PausedRead<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.gate.waker.register(context.waker());
        if self.gate.paused.load(std::sync::atomic::Ordering::Acquire) {
            return std::task::Poll::Pending;
        }
        std::pin::Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

#[tokio::test]
async fn buffered_request_from_an_older_connection_cannot_overwrite_a_newer_root() {
    use std::sync::{Arc, atomic::Ordering};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("system-bundle.pem");
    fs::write(&path, b"# Distro roots\n").unwrap();
    let services = services(&path);
    let old_services = services.clone();
    let (mut old, server) = tokio::io::duplex(8192);
    let (reader, writer) = tokio::io::split(server);
    let gate = Arc::new(ReadGate::default());
    let server = tokio::io::join(
        PausedRead {
            inner: reader,
            gate: Arc::clone(&gate),
        },
        writer,
    );
    let dispatcher = tokio::spawn(async move {
        handle_connection(
            server,
            Guest::SystemVm,
            &old_services,
            tokio_vsock::VMADDR_CID_HOST,
        )
        .await
    });
    write_message(&mut old, MessageType::PingRequest, "old-ping", &[])
        .await
        .unwrap();
    let (kind, _, _) = tokio::time::timeout(Duration::from_secs(3), read_message(&mut old))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kind, MessageType::PingResponse);
    gate.paused.store(true, Ordering::Release);
    write_message(
        &mut old,
        MessageType::ReconcileGuestTrustRequest,
        "old-trust",
        &payload(Some(Desired::CertificatePem(CA_PEM.to_vec()))),
    )
    .await
    .unwrap();
    old.shutdown().await.unwrap();
    let newer_pem = include_bytes!("guest_trust_next_ca.pem");
    let (kind, _) = exchange(
        services,
        Guest::SystemVm,
        tokio_vsock::VMADDR_CID_HOST,
        &payload(Some(Desired::CertificatePem(newer_pem.to_vec()))),
    )
    .await;
    assert_eq!(kind, MessageType::ReconcileGuestTrustResponse);
    let installed = fs::read(&path).unwrap();
    gate.paused.store(false, Ordering::Release);
    gate.waker.wake();
    let (kind, trace, response) =
        tokio::time::timeout(Duration::from_secs(3), read_message(&mut old))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(kind, MessageType::Error);
    assert_eq!(trace, "old-trust");
    assert_eq!(ErrorResponse::decode(&response).unwrap().code, 409);
    assert_eq!(fs::read(&path).unwrap(), installed);
    assert!(
        std::str::from_utf8(&installed)
            .unwrap()
            .contains(std::str::from_utf8(newer_pem).unwrap().trim())
    );
    drop(old);
    dispatcher.await.unwrap().unwrap();
}
