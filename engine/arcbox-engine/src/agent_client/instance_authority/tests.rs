use super::*;
use arcbox_transport::vsock::BlockingVsockTransport;
use std::{os::fd::IntoRawFd, os::unix::net::UnixStream, time::Duration};

fn blocking(stream: UnixStream) -> AgentTransport {
    // SAFETY: the transport receives ownership of the connected socket.
    AgentTransport::Blocking(unsafe {
        BlockingVsockTransport::from_raw_fd(stream.into_raw_fd()).unwrap()
    })
}

#[cfg(target_os = "macos")]
fn asynchronous(stream: UnixStream) -> AgentTransport {
    use arcbox_transport::vsock::{VsockAddr, VsockTransport};
    AgentTransport::Async(
        VsockTransport::from_raw_fd(stream.into_raw_fd(), VsockAddr::new(3, 1024)).unwrap(),
    )
}

fn connected(transport: fn(UnixStream) -> AgentTransport) -> (AgentClient, BlockingVsockTransport) {
    let (host, guest) = UnixStream::pair().unwrap();
    // SAFETY: the peer transport receives ownership of its connected socket.
    let guest = unsafe { BlockingVsockTransport::from_raw_fd(guest.into_raw_fd()).unwrap() };
    (
        AgentClient {
            cid: 3,
            transport: transport(host),
            connected: true,
            protocol_admitted: false,
        },
        guest,
    )
}

fn receive(peer: &mut BlockingVsockTransport, expected: MessageType) -> Vec<u8> {
    let raw = peer.recv(Instant::now() + Duration::from_secs(3)).unwrap();
    let (kind, trace, payload) = wire::parse_response(&raw).unwrap();
    assert_eq!(kind, expected as u32);
    assert_eq!(trace, "");
    payload
}

fn respond(peer: &mut BlockingVsockTransport, kind: MessageType, message: &impl Message) {
    peer.send(
        &wire::build_message(kind, "", &message.encode_to_vec()),
        Instant::now() + Duration::from_secs(3),
    )
    .unwrap();
}

fn transports() -> impl Iterator<Item = fn(UnixStream) -> AgentTransport> {
    [
        blocking as fn(UnixStream) -> AgentTransport,
        #[cfg(target_os = "macos")]
        asynchronous,
    ]
    .into_iter()
}

fn admission(peer: &mut BlockingVsockTransport) {
    receive(peer, MessageType::PingRequest);
    respond(
        peer,
        MessageType::PingResponse,
        &AgentPingResponse {
            protocol_version: arcbox_constants::wire::AGENT_PROTOCOL_VERSION,
            ..Default::default()
        },
    );
    receive(peer, MessageType::OpenInstanceAuthorityRequest);
    respond(
        peer,
        MessageType::OpenInstanceAuthorityResponse,
        &OpenInstanceAuthorityResponse::default(),
    );
}

#[tokio::test]
async fn acknowledged_snapshots_and_idle_exit_join_the_transport_workers() {
    for transport in transports() {
        for peer_closes in [false, true] {
            let (client, mut peer) = connected(transport);
            let (release, finish) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                admission(&mut peer);
                for authority_revision in [0, 12] {
                    let payload = receive(&mut peer, MessageType::ReplaceInstanceAuthorityRequest);
                    let snapshot =
                        ReplaceInstanceAuthorityRequest::decode_from_slice(&payload).unwrap();
                    assert_eq!(snapshot.authority_revision, authority_revision);
                    respond(
                        &mut peer,
                        MessageType::InstanceAuthorityAck,
                        &InstanceAuthorityAck {
                            authority_revision,
                            ..Default::default()
                        },
                    );
                }
                finish.recv_timeout(Duration::from_secs(3)).unwrap();
                if !peer_closes {
                    let error = peer
                        .recv(Instant::now() + Duration::from_secs(3))
                        .unwrap_err();
                    assert!(error.to_string().contains("eof"), "{error}");
                }
            });
            let (mut session, coordinator) = client.instance_authority().unwrap();
            session.opened().await.unwrap();
            for authority_revision in [0, 12] {
                session
                    .replace(ReplaceInstanceAuthorityRequest {
                        authority_revision,
                        ..Default::default()
                    })
                    .await
                    .unwrap();
            }
            release.send(()).unwrap();
            if !peer_closes {
                drop(session);
            }
            let result = tokio::time::timeout(Duration::from_secs(3), coordinator)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.is_err(), peer_closes);
            tokio::task::spawn_blocking(move || server.join().unwrap())
                .await
                .unwrap();
        }
    }
}

async fn stop_at_phase(
    transport: fn(UnixStream) -> AgentTransport,
    phase: MessageType,
    partial: bool,
    ending: &str,
) {
    let (client, mut peer) = connected(transport);
    let (started, waiting) = oneshot::channel();
    let eof = ending == "eof";
    let server = std::thread::spawn(move || {
        for (request, response, payload) in [
            (
                MessageType::PingRequest,
                MessageType::PingResponse,
                AgentPingResponse {
                    protocol_version: arcbox_constants::wire::AGENT_PROTOCOL_VERSION,
                    ..Default::default()
                }
                .encode_to_vec(),
            ),
            (
                MessageType::OpenInstanceAuthorityRequest,
                MessageType::OpenInstanceAuthorityResponse,
                vec![],
            ),
            (
                MessageType::ReplaceInstanceAuthorityRequest,
                MessageType::InstanceAuthorityAck,
                InstanceAuthorityAck {
                    authority_revision: 10,
                    ..Default::default()
                }
                .encode_to_vec(),
            ),
        ] {
            receive(&mut peer, request);
            let response = wire::build_message(response, "", &payload);
            if request == phase {
                if partial {
                    peer.send(&response[..3], Instant::now() + Duration::from_secs(3))
                        .unwrap();
                }
                started.send(()).unwrap();
                if !eof {
                    let error = peer
                        .recv(Instant::now() + Duration::from_secs(3))
                        .unwrap_err();
                    assert!(error.to_string().contains("eof"), "{error}");
                }
                return;
            }
            peer.send(&response, Instant::now() + Duration::from_secs(3))
                .unwrap();
        }
    });
    let (mut session, coordinator) = client.instance_authority().unwrap();
    {
        let operation = async {
            session.opened().await?;
            session
                .replace(ReplaceInstanceAuthorityRequest {
                    authority_revision: 10,
                    ..Default::default()
                })
                .await
        };
        tokio::pin!(operation);
        tokio::select! {
            biased;
            result = waiting => result.unwrap(),
            result = &mut operation => panic!("operation ended before the peer barrier: {result:?}"),
        }
        if ending == "timeout" {
            tokio::time::pause();
            // Tokio rounds deadlines to milliseconds; blocking readers inhibit auto-advance.
            tokio::time::advance(BLOCKING_RPC_TIMEOUT + Duration::from_millis(1)).await;
            assert!(operation.await.is_err());
            tokio::time::resume();
        } else if eof {
            assert!(operation.await.is_err());
        }
    }
    if ending == "cancel" {
        drop(session);
    }
    let result = tokio::time::timeout(Duration::from_secs(3), coordinator)
        .await
        .unwrap()
        .unwrap();
    if ending == "timeout" {
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("timed out"),
            "{phase:?}, partial={partial}: {error}"
        );
    } else {
        assert_eq!(result.is_err(), eof);
    }
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn every_protocol_wait_cancels_times_out_and_observes_eof() {
    for transport in transports() {
        for phase in [
            MessageType::PingRequest,
            MessageType::OpenInstanceAuthorityRequest,
            MessageType::ReplaceInstanceAuthorityRequest,
        ] {
            for partial in [false, true] {
                for ending in ["cancel", "timeout", "eof"] {
                    stop_at_phase(transport, phase, partial, ending).await;
                }
            }
        }
    }
}

#[tokio::test]
async fn invalid_or_unsolicited_acknowledgements_close_the_connection() {
    for transport in transports() {
        for (raw, expected) in [
            (
                wire::build_message(
                    MessageType::InstanceAuthorityAck,
                    "",
                    &InstanceAuthorityAck {
                        authority_revision: 9,
                        ..Default::default()
                    }
                    .encode_to_vec(),
                ),
                "different authority revision",
            ),
            (
                wire::build_message(MessageType::Empty, "", &[]),
                "unexpected response type",
            ),
            (
                Bytes::from_static(&[0, 0, 0, 6, 0, 0, 0x10, 0x19, 0, 1]),
                "trace length exceeds",
            ),
            (
                wire::build_message(MessageType::InstanceAuthorityAck, "", &[0x80]),
                "failed to decode",
            ),
            (
                wire::build_message(
                    MessageType::Error,
                    "",
                    &[0, 0, 1, 153, 0, 0, 0, 4, b'b', b'u', b's', b'y'],
                ),
                "busy",
            ),
            (
                wire::build_message(MessageType::InstanceAuthorityAck, "", &[]),
                "unsolicited",
            ),
        ] {
            let idle = expected == "unsolicited";
            let (client, mut peer) = connected(transport);
            let server = std::thread::spawn(move || {
                admission(&mut peer);
                if !idle {
                    receive(&mut peer, MessageType::ReplaceInstanceAuthorityRequest);
                }
                let raw = if idle { raw.repeat(3) } else { raw.to_vec() };
                peer.send(&raw, Instant::now() + Duration::from_secs(3))
                    .unwrap();
                let error = peer
                    .recv(Instant::now() + Duration::from_secs(3))
                    .unwrap_err();
                assert!(error.to_string().contains("eof"), "{error}");
            });
            let (mut session, coordinator) = client.instance_authority().unwrap();
            session.opened().await.unwrap();
            if !idle {
                assert!(
                    session
                        .replace(ReplaceInstanceAuthorityRequest {
                            authority_revision: 10,
                            ..Default::default()
                        })
                        .await
                        .is_err()
                );
            }
            let error = tokio::time::timeout(Duration::from_secs(3), coordinator)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            tokio::task::spawn_blocking(move || server.join().unwrap())
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn rejected_admission_never_opens_the_authority_channel() {
    for transport in transports() {
        let (client, mut peer) = connected(transport);
        let server = std::thread::spawn(move || {
            receive(&mut peer, MessageType::PingRequest);
            respond(
                &mut peer,
                MessageType::PingResponse,
                &AgentPingResponse::default(),
            );
            let error = peer
                .recv(Instant::now() + Duration::from_secs(3))
                .unwrap_err();
            assert!(error.to_string().contains("eof"), "{error}");
        });
        let (mut session, coordinator) = client.instance_authority().unwrap();
        assert!(session.opened().await.is_err());
        let error = tokio::time::timeout(Duration::from_secs(3), coordinator)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("did not complete a compatible handshake")
        );
        tokio::task::spawn_blocking(move || server.join().unwrap())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn cancellation_joins_a_sender_without_the_peer_draining_its_full_socket() {
    use std::{
        io::{Read, Write},
        os::fd::AsRawFd,
    };
    for transport in transports() {
        let (host, mut peer) = UnixStream::pair().unwrap();
        let buffer: libc::c_int = 4096;
        // SAFETY: host owns the socket; buffer and its exact size remain valid for this call.
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    host.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    std::ptr::from_ref(&buffer).cast(),
                    std::mem::size_of_val(&buffer) as libc::socklen_t,
                )
            },
            0
        );
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let client = AgentClient {
            cid: 3,
            transport: transport(host),
            connected: true,
            protocol_admitted: false,
        };
        let (started, waiting) = oneshot::channel();
        let (release, finish) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for (request, response, payload) in [
                (
                    MessageType::PingRequest,
                    MessageType::PingResponse,
                    AgentPingResponse {
                        protocol_version: arcbox_constants::wire::AGENT_PROTOCOL_VERSION,
                        ..Default::default()
                    }
                    .encode_to_vec(),
                ),
                (
                    MessageType::OpenInstanceAuthorityRequest,
                    MessageType::OpenInstanceAuthorityResponse,
                    vec![],
                ),
            ] {
                let mut length = [0; 4];
                peer.read_exact(&mut length).unwrap();
                let mut body = vec![0; u32::from_be_bytes(length) as usize];
                peer.read_exact(&mut body).unwrap();
                assert_eq!(
                    u32::from_be_bytes(body[..4].try_into().unwrap()),
                    request as u32
                );
                peer.write_all(&wire::build_message(response, "", &payload))
                    .unwrap();
            }
            let mut prefix = [0; 10];
            peer.read_exact(&mut prefix).unwrap();
            let expected = u32::from_be_bytes(prefix[..4].try_into().unwrap()) as usize;
            assert_eq!(
                u32::from_be_bytes(prefix[4..8].try_into().unwrap()),
                MessageType::ReplaceInstanceAuthorityRequest as u32
            );
            started.send(()).unwrap();
            // Keep the send buffer saturated until both host workers have exited.
            finish.recv_timeout(Duration::from_secs(3)).unwrap();
            let mut remainder = Vec::new();
            peer.read_to_end(&mut remainder).unwrap();
            assert!(
                6 + remainder.len() < expected,
                "the snapshot must still be incomplete at cancellation"
            );
        });
        let (mut session, coordinator) = client.instance_authority().unwrap();
        session.opened().await.unwrap();
        {
            let replacing = session.replace(ReplaceInstanceAuthorityRequest {
                authority_revision: 1,
                roots: (0..16_384)
                    .map(|index| arcbox_connect::v1::InstanceAuthorityRoot {
                        root: format!("service{index:05}.arcbox.local"),
                        target: arcbox_connect::v1::instance_authority_root::Target::Current.into(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            });
            tokio::pin!(replacing);
            tokio::select! {
                result = waiting => result.unwrap(),
                result = &mut replacing => panic!("snapshot escaped socket pressure: {result:?}"),
            }
        }
        drop(session);
        tokio::time::timeout(Duration::from_secs(3), coordinator)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        release.send(()).unwrap();
        tokio::task::spawn_blocking(move || server.join().unwrap())
            .await
            .unwrap();
    }
}
