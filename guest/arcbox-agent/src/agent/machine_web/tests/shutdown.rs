//! The exclusive session outlives real stream shutdown and withdraws before waiting.

use super::*;
use crate::domains::{EndpointFacts, SelectedRoutes};
use crate::rpc::{MessageType, write_message};
use buffa::Message as _;
use std::{
    future::Future as _,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{io::ReadBuf, net::UnixStream, sync::oneshot};

#[test]
fn invalid_open_identity_or_address_cannot_claim_the_session() {
    let receiver = receiver();
    for request in [
        OpenMachineWebRequest {
            machine_name: String::new(),
            ..open()
        },
        OpenMachineWebRequest {
            hostname: "UPPER.machine.arcbox.local".into(),
            ..open()
        },
        OpenMachineWebRequest {
            hostname: "*.machine.arcbox.local".into(),
            ..open()
        },
        OpenMachineWebRequest {
            ipv4_address: "::1".into(),
            ..open()
        },
        OpenMachineWebRequest {
            ipv4_address: "192.168.64.999".into(),
            ..open()
        },
    ] {
        assert!(receiver.open(request, Some(bridge())).is_err());
        assert!(!receiver.state.lock().unwrap().active);
        assert!(receiver.state.lock().unwrap().configuration.is_none());
        assert_eq!(receiver.registry.read().unwrap().epoch(), 0);
    }
    assert!(receiver.open(open(), Some(bridge())).unwrap().is_some());
}

struct ShutdownGate {
    stream: UnixStream,
    entered: Option<oneshot::Sender<()>>,
    resume: Option<oneshot::Receiver<()>>,
}

impl AsyncRead for ShutdownGate {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for ShutdownGate {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(entered) = self.entered.take() {
            entered.send(()).unwrap();
        }
        if let Some(resume) = self.resume.as_mut() {
            std::task::ready!(Pin::new(resume).poll(cx)).unwrap();
            self.resume = None;
        }
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

fn publish_cleanup_witness(receiver: &Receiver) {
    let name = "witness.arcbox.local".parse().unwrap();
    receiver
        .registry
        .write()
        .unwrap()
        .replace(
            "witness".into(),
            vec![Claim::exclusive(
                NamePattern::Exact(name),
                "witness".into(),
                "container".into(),
            )],
            Some("192.168.64.9".parse().unwrap()),
        )
        .unwrap();
    let facts = EndpointFacts::from_inspect(
        &serde_json::json!({
            "Id": "witness", "Name": "/witness", "State": { "Pid": 1 },
            "NetworkSettings": { "SandboxKey": "/proc/self/ns/net", "Networks": {
                "bridge": { "IPAddress": "192.168.64.9" }
            }}
        }),
        &"arcbox.local".parse().unwrap(),
    )
    .unwrap();
    receiver.web.publish(
        &facts,
        SelectedRoutes {
            http: Some(8080),
            https: None,
        },
    );
    assert_eq!(receiver.web.snapshot(None).len(), 1);
}

#[test]
fn dropping_a_session_withdraws_routes_and_preserves_unrelated_dns() {
    let receiver = receiver();
    let directory = receiver.web.directory_source();
    directory.ready();
    let session = receiver.open(open(), Some(bridge())).unwrap().unwrap();
    publish_cleanup_witness(&receiver);
    drop(session);
    assert_eq!(
        receiver.web.snapshot(None),
        [] as [arcbox_connect::v1::GuestWebService; 0]
    );
    assert!(receiver.registry.read().unwrap().get("witness").is_some());
    assert!(receiver.open(open(), Some(bridge())).unwrap().is_some());
}

#[test]
fn a_new_revision_with_identical_policies_invalidates_installed_routes() {
    let receiver = receiver();
    let directory = receiver.web.directory_source();
    directory.ready();
    let session = receiver.open(open(), Some(bridge())).unwrap().unwrap();
    let mut request = replacement(1, Selection::Port(8080), Selection::Port(8443));
    session.replace(request.clone()).unwrap();
    publish_cleanup_witness(&receiver);
    request.source_revision = 2;
    assert_eq!(session.replace(request.clone()).unwrap(), 2);
    assert_eq!(
        receiver.web.snapshot(None),
        [] as [arcbox_connect::v1::GuestWebService; 0]
    );
    assert_eq!(
        receiver
            .state
            .lock()
            .unwrap()
            .configuration
            .as_ref()
            .unwrap()
            .source_revision,
        request.source_revision
    );
}

#[tokio::test]
async fn eof_withdraws_before_shutdown_and_preserves_exclusivity_until_shutdown_finishes() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let receiver = receiver();
        let directory = receiver.web.directory_source();
        directory.ready();
        let (mut client, stream) = UnixStream::pair().unwrap();
        let (entered, waiting) = oneshot::channel();
        let (resume, released) = oneshot::channel();
        let serving = receiver.clone();
        let task = tokio::spawn(async move {
            let mut stream = ShutdownGate {
                stream,
                entered: Some(entered),
                resume: Some(released),
            };
            serving
                .serve(&mut stream, "open", open(), Some(bridge()))
                .await
        });
        let (kind, trace, _) = read_message(&mut client).await.unwrap();
        assert_eq!(kind, MessageType::OpenMachineWebResponse);
        assert_eq!(trace, "open");
        write_message(
            &mut client,
            MessageType::ReplaceMachineWebRequest,
            "replace",
            &replacement(1, Selection::Port(8080), Selection::Off(Box::default())).encode_to_vec(),
        )
        .await
        .unwrap();
        let (kind, trace, payload) = read_message(&mut client).await.unwrap();
        assert_eq!(kind, MessageType::MachineWebAck);
        assert_eq!(trace, "replace");
        assert_eq!(
            MachineWebAck::decode_from_slice(&payload)
                .unwrap()
                .source_revision,
            1
        );
        publish_cleanup_witness(&receiver);
        client.shutdown().await.unwrap();
        waiting.await.unwrap();
        assert!(receiver.state.lock().unwrap().active);
        assert!(receiver.state.lock().unwrap().configuration.is_none());
        {
            let registry = receiver.registry.read().unwrap();
            assert!(registry.get("machine:my_vm").is_none());
            assert!(registry.get("witness").is_some());
        }
        assert_eq!(
            receiver.web.snapshot(None),
            [] as [arcbox_connect::v1::GuestWebService; 0]
        );
        assert!(receiver.open(open(), Some(bridge())).unwrap().is_none());
        resume.send(()).unwrap();
        assert!(task.await.unwrap().is_err());
        assert!(receiver.open(open(), Some(bridge())).unwrap().is_some());
    })
    .await
    .unwrap();
}

#[test]
fn invalid_or_stale_replacements_preserve_the_last_complete_policy() {
    let receiver = receiver();
    let session = receiver.open(open(), Some(bridge())).unwrap().unwrap();
    assert!(
        session
            .replace(replacement(0, Selection::Port(80), Selection::Port(443)))
            .is_err()
    );
    let accepted = replacement(2, Selection::Port(8080), Selection::Port(8443));
    session.replace(accepted.clone()).unwrap();
    let mut invalid = vec![
        replacement(0, Selection::Port(80), Selection::Port(443)),
        replacement(1, Selection::Port(80), Selection::Port(443)),
        accepted,
        replacement(3, Selection::Port(0), Selection::Port(443)),
        replacement(3, Selection::Port(80), Selection::Port(65536)),
        ReplaceMachineWebRequest {
            source_revision: 3,
            ..Default::default()
        },
    ];
    for http in [true, false] {
        let mut missing = replacement(3, Selection::Port(80), Selection::Port(443));
        let ports = missing.ports.as_option_mut().unwrap();
        if http {
            ports.http_port = WebPortPolicy::default().into();
        } else {
            ports.https_port = WebPortPolicy::default().into();
        }
        invalid.push(missing);
    }
    let accepted_configuration = receiver.state.lock().unwrap().configuration.clone();
    for request in invalid {
        assert!(session.replace(request).is_err());
        assert_eq!(
            receiver.state.lock().unwrap().configuration,
            accepted_configuration
        );
    }
}

#[test]
fn bridge_admission_requires_the_observed_machine_interface() {
    let receiver = receiver();
    for observed in [None, Some("192.168.64.9".parse().unwrap())] {
        assert!(receiver.open(open(), observed).is_err());
        assert!(!receiver.state.lock().unwrap().active);
        assert_eq!(receiver.registry.read().unwrap().epoch(), 0);
    }
    assert!(receiver.open(open(), Some(bridge())).unwrap().is_some());
}
