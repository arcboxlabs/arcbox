use super::*;
use crate::agent_client::wire;
use crate::machine::{MachineWebPorts, WebPortPolicy};
use std::time::Duration;

async fn request(outgoing: &mut mpsc::Receiver<(Bytes, Instant)>, kind: MessageType) -> Vec<u8> {
    let (frame, _) = outgoing.recv().await.unwrap();
    let (actual, trace, payload) = wire::parse_response(&frame).unwrap();
    assert_eq!(actual, kind as u32);
    assert_eq!(trace, "");
    payload
}

async fn respond(incoming: &mpsc::Sender<Bytes>, kind: MessageType, body: &impl Message) {
    incoming
        .send(wire::build_message(kind, "", &body.encode_to_vec()))
        .await
        .unwrap();
}

#[tokio::test]
async fn fixed_identity_and_full_policies_require_matching_increasing_revisions() {
    for ending in ["zero", "repeat", "wrong ack", "unsolicited", "eof"] {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (commands, mut outgoing) = mpsc::channel(1);
            let (incoming, mut frames) = mpsc::channel(1);
            let (replacements, mut updates) = mpsc::channel(1);
            let (ready, _opened) = watch::channel(false);
            let open = OpenMachineWebRequest {
                machine_name: "my_vm".into(),
                hostname: "my-vm.machine.arcbox.local".into(),
                ipv4_address: "192.168.64.8".into(),
                ..Default::default()
            };
            let expected_open = open.encode_to_vec();
            let coordinator = tokio::spawn(async move {
                run(&commands, &mut frames, &mut updates, ready, open).await
            });
            request(&mut outgoing, MessageType::PingRequest).await;
            respond(
                &incoming,
                MessageType::PingResponse,
                &AgentPingResponse {
                    protocol_version: arcbox_constants::wire::AGENT_PROTOCOL_VERSION,
                    ..Default::default()
                },
            )
            .await;
            assert_eq!(
                request(&mut outgoing, MessageType::OpenMachineWebRequest).await,
                expected_open
            );
            respond(
                &incoming,
                MessageType::OpenMachineWebResponse,
                &OpenMachineWebResponse::default(),
            )
            .await;
            for (revision, http_port) in [(1, "8080"), (2, "off")] {
                let source_revision = match (ending, revision) {
                    ("zero", _) => 0,
                    ("repeat", 2) => 1,
                    _ => revision,
                };
                let policy = MachineWebPorts {
                    http_port: http_port.parse().unwrap(),
                    https_port: WebPortPolicy::Auto,
                };
                let (applied, committed) = oneshot::channel();
                replacements
                    .send(Replacement {
                        snapshot: ReplaceMachineWebRequest {
                            source_revision,
                            ports: arcbox_connect::v1::MachineWebPorts::from(policy).into(),
                            ..Default::default()
                        },
                        applied,
                    })
                    .await
                    .unwrap();
                if ending == "zero" || ending == "repeat" && revision == 2 {
                    assert!(committed.await.is_err());
                    break;
                }
                let bytes = request(&mut outgoing, MessageType::ReplaceMachineWebRequest).await;
                let sent = ReplaceMachineWebRequest::decode_from_slice(&bytes).unwrap();
                assert_eq!(sent.source_revision, source_revision);
                assert_eq!(MachineWebPorts::try_from(&*sent.ports).unwrap(), policy);
                respond(
                    &incoming,
                    MessageType::MachineWebAck,
                    &MachineWebAck {
                        source_revision: if ending == "wrong ack" {
                            source_revision + 1
                        } else {
                            source_revision
                        },
                        ..Default::default()
                    },
                )
                .await;
                if ending == "wrong ack" {
                    assert!(committed.await.is_err());
                    break;
                }
                committed.await.unwrap();
            }
            if ending == "unsolicited" {
                respond(
                    &incoming,
                    MessageType::MachineWebAck,
                    &MachineWebAck::default(),
                )
                .await;
            }
            drop(incoming);
            let error = coordinator.await.unwrap().unwrap_err().to_string();
            let expected = match ending {
                "zero" | "repeat" => "source revision must increase",
                "wrong ack" => "different source revision",
                "unsolicited" => "unsolicited response",
                _ => "connection closed",
            };
            assert!(error.contains(expected), "{ending}: {error}");
            assert!(
                outgoing.recv().await.is_none(),
                "{ending} sent an invalid request"
            );
        })
        .await
        .unwrap();
    }
}
