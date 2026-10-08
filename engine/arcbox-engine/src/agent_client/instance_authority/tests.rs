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

#[tokio::test]
async fn acknowledged_snapshots_and_idle_exit_join_the_transport_workers() {
    for transport in [
        blocking as fn(UnixStream) -> AgentTransport,
        #[cfg(target_os = "macos")]
        asynchronous,
    ] {
        for peer_closes in [false, true] {
            let (client, mut peer) = connected(transport);
            let (release, finish) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                receive(&mut peer, MessageType::PingRequest);
                respond(
                    &mut peer,
                    MessageType::PingResponse,
                    &AgentPingResponse {
                        protocol_version: arcbox_constants::wire::AGENT_PROTOCOL_VERSION,
                        ..Default::default()
                    },
                );
                receive(&mut peer, MessageType::OpenInstanceAuthorityRequest);
                respond(
                    &mut peer,
                    MessageType::OpenInstanceAuthorityResponse,
                    &OpenInstanceAuthorityResponse::default(),
                );
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
