use super::*;
use arcbox_connect::v1::{
    AgentPingResponse, MachineWebAck, OpenMachineWebRequest, OpenMachineWebResponse,
};
use arcbox_constants::wire::{AGENT_PROTOCOL_VERSION, MessageType};
use arcbox_core::{Config, runtime::NetworkFixture};
use arcbox_transport::vsock::read_framed;
use buffa::Message;
use std::{
    os::fd::IntoRawFd as _,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::UnixStream,
    task::JoinHandle,
};

async fn receive<T: Message>(peer: &mut UnixStream, expected: MessageType) -> T {
    let frame = read_framed(peer).await.unwrap();
    assert_eq!(&frame[4..8], &(expected as u32).to_be_bytes());
    T::decode_from_slice(&frame[10..]).unwrap()
}

async fn respond(peer: &mut UnixStream, kind: MessageType, message: &impl Message) {
    peer.write_all(&AgentClient::build_message(
        kind,
        "",
        &message.encode_to_vec(),
    ))
    .await
    .unwrap();
}

struct Harness {
    peer: UnixStream,
    task: JoinHandle<Result<()>>,
    source: Arc<Mutex<CapturedMachineWeb>>,
    changed: watch::Sender<()>,
    dns: watch::Sender<u64>,
    authority: watch::Sender<Option<Vec<u8>>>,
    captures: Arc<AtomicUsize>,
    shutdown: CancellationToken,
}

impl Harness {
    async fn new(blocking: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(Config::for_test(directory.path()))
            .unwrap()
            .with_network_fixture(NetworkFixture::default());
        // The transport harness supplies snapshots; production capture has no fixture bypass.
        let snapshot = CapturedMachineWeb {
            guest: runtime
                .capture_web_guest(runtime.default_machine_name())
                .unwrap(),
            open: OpenMachineWebRequest {
                machine_name: "first_vm".into(),
                hostname: "first-vm.machine.arcbox.local".into(),
                ipv4_address: "192.168.64.8".into(),
                ..Default::default()
            },
            claim_revision: 7,
            ports: Default::default(),
        };
        let source = Arc::new(Mutex::new(snapshot.clone()));
        let captured = Arc::clone(&source);
        let captures = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&captures);
        let (host, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = if blocking {
            AgentClient::from_fd_blocking(7, host.into_raw_fd())
        } else {
            AgentClient::from_fd_async(7, host.into_raw_fd())
        }
        .unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut peer = UnixStream::from_std(peer).unwrap();
        let (changed, changes) = watch::channel(());
        let (dns, dns_changes) = watch::channel(0);
        let (authority, authority_changes) = watch::channel(None);
        let shutdown = CancellationToken::new();
        let cancelled = shutdown.clone();
        let task = tokio::spawn(async move {
            let updates = Updates {
                policies: changes,
                dns: dns_changes,
                authority: authority_changes,
                certificate: Clone::clone,
            };
            connected(client, snapshot, updates, &cancelled, || {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(captured.lock().unwrap().clone())
            })
            .await
        });
        let _: arcbox_connect::v1::AgentPingRequest =
            receive(&mut peer, MessageType::PingRequest).await;
        respond(
            &mut peer,
            MessageType::PingResponse,
            &AgentPingResponse {
                protocol_version: AGENT_PROTOCOL_VERSION,
                ..Default::default()
            },
        )
        .await;
        let opened: OpenMachineWebRequest =
            receive(&mut peer, MessageType::OpenMachineWebRequest).await;
        assert_eq!(opened, source.lock().unwrap().open);
        Self {
            peer,
            task,
            source,
            changed,
            dns,
            authority,
            captures,
            shutdown,
        }
    }

    async fn open(&mut self) {
        respond(
            &mut self.peer,
            MessageType::OpenMachineWebResponse,
            &OpenMachineWebResponse::default(),
        )
        .await;
    }
}

#[tokio::test]
async fn policies_replace_complete_pairs_without_reopening() {
    tokio::time::timeout(Duration::from_secs(3), async {
        for blocking in [true, false] {
            let mut harness = Harness::new(blocking).await;
            harness.open().await;
            for (revision, http, https) in
                [(1, "auto", "auto"), (2, "8080", "off"), (3, "8080", "8443")]
            {
                if revision > 1 {
                    let mut snapshot = harness.source.lock().unwrap();
                    snapshot.ports.http_port = http.parse().unwrap();
                    snapshot.ports.https_port = https.parse().unwrap();
                    harness.changed.send_replace(());
                }
                let request: ReplaceMachineWebRequest =
                    receive(&mut harness.peer, MessageType::ReplaceMachineWebRequest).await;
                assert_eq!(request.source_revision, revision);
                let ports =
                    arcbox_core::machine::MachineWebPorts::try_from(&*request.ports).unwrap();
                assert_eq!(ports.http_port.to_string(), http);
                assert_eq!(ports.https_port.to_string(), https);
                respond(
                    &mut harness.peer,
                    MessageType::MachineWebAck,
                    &MachineWebAck {
                        source_revision: revision,
                        ..Default::default()
                    },
                )
                .await;
            }
            harness.shutdown.cancel();
            harness.task.await.unwrap().unwrap();
            assert_eq!(harness.peer.read(&mut [0; 1]).await.unwrap(), 0);
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn root_changes_before_open_during_ack_and_while_idle_replace_complete_snapshots() {
    tokio::time::timeout(Duration::from_secs(3), async {
        for blocking in [true, false] {
            let mut harness = Harness::new(blocking).await;
            harness
                .authority
                .send_replace(Some(b"first public root".to_vec()));
            harness.open().await;
            for (revision, root) in [
                (1, Some(b"first public root".to_vec())),
                (2, Some(b"replacement public root".to_vec())),
                (3, None),
            ] {
                let request: ReplaceMachineWebRequest =
                    receive(&mut harness.peer, MessageType::ReplaceMachineWebRequest).await;
                assert_eq!(request.source_revision, revision);
                assert_eq!(request.ca_certificate_pem, root);
                assert_eq!(
                    request.ports,
                    MachineWebPorts::from(harness.source.lock().unwrap().ports).into()
                );
                if revision == 1 {
                    harness
                        .authority
                        .send_replace(Some(b"replacement public root".to_vec()));
                }
                let captures = harness.captures.load(Ordering::SeqCst);
                respond(
                    &mut harness.peer,
                    MessageType::MachineWebAck,
                    &MachineWebAck {
                        source_revision: revision,
                        ..Default::default()
                    },
                )
                .await;
                if revision == 2 {
                    // Wait for Ack validation and resampling before testing the idle CA watch.
                    while harness.captures.load(Ordering::SeqCst) < captures + 2 {
                        tokio::task::yield_now().await;
                    }
                    harness.authority.send_replace(None);
                }
            }
            harness.shutdown.cancel();
            harness.task.await.unwrap().unwrap();
            assert_eq!(harness.peer.read(&mut [0; 1]).await.unwrap(), 0);
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn identity_changes_drain_pending_open_and_ack() {
    tokio::time::timeout(Duration::from_secs(3), async {
        for pending_ack in [false, true] {
            let mut harness = Harness::new(true).await;
            if pending_ack {
                harness.open().await;
                let _: ReplaceMachineWebRequest =
                    receive(&mut harness.peer, MessageType::ReplaceMachineWebRequest).await;
            }
            harness.source.lock().unwrap().claim_revision += 1;
            harness.dns.send_replace(1);
            assert!(
                harness
                    .task
                    .await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("identity changed")
            );
            assert_eq!(harness.peer.read(&mut [0; 1]).await.unwrap(), 0);
        }
    })
    .await
    .unwrap();
}
