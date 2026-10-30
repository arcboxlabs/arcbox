use super::*;
use arcbox_connect::v1::MachineService as _;
use arcbox_core::event::Event;
use arcbox_core::machine::{MachineConfig, MachineWebPorts};
use buffa::Message as _;
use connectrpc::HasMessageView as _;
use std::sync::Arc;

#[tokio::test]
async fn early_directory_loads_saved_records_without_admitting_operations() {
    let (directory, stored) = crate::connect::tests::storage_runtime();
    let mut config = stored.ready().unwrap().config().clone();
    config.machine.default_machine = Some("dev".into());
    let web_ports = MachineWebPorts {
        http_port: "8080".parse().unwrap(),
        ..Default::default()
    };
    stored
        .ready()
        .unwrap()
        .machine_manager()
        .create(MachineConfig {
            name: "dev".into(),
            web_ports,
            ..Default::default()
        })
        .await
        .unwrap();
    drop(stored);
    let runtime = Arc::new(arcbox_core::Runtime::new(config).unwrap());
    let hold = runtime.machine_manager().storage_hold_path();
    std::fs::create_dir_all(hold.parent().unwrap()).unwrap();
    std::fs::write(hold, "offline-check").unwrap();
    let operational: SharedRuntime = Arc::default();
    let early: SharedRuntime = Arc::default();
    let service = MachineServiceImpl::new(operational.clone(), early.clone());
    let list_bytes = bytes::Bytes::from(pb::ListMachinesRequest::default().encode_to_vec());
    let list_view = pb::ListMachinesRequest::decode_view(&list_bytes).unwrap();
    let inspect_bytes = bytes::Bytes::from(
        pb::InspectMachineRequest {
            id: "dev".into(),
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let inspect_view = pb::InspectMachineRequest::decode_view(&inspect_bytes).unwrap();
    let event_bytes = bytes::Bytes::from(pb::MachineEventsRequest::default().encode_to_vec());
    let event_view = pb::MachineEventsRequest::decode_view(&event_bytes).unwrap();
    let start_bytes = bytes::Bytes::from(
        pb::StartMachineRequest {
            id: "default".into(),
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let start_view = pb::StartMachineRequest::decode_view(&start_bytes).unwrap();
    for phase in 0..3 {
        if phase == 1 {
            assert!(early.set(runtime.clone()).is_ok());
        } else if phase == 2 {
            assert!(operational.set(runtime.clone()).is_ok());
        }
        let listed = service
            .list(
                RequestContext::new(Default::default()),
                ServiceRequest::from_parts(&list_view, &list_bytes),
            )
            .await;
        let inspected = service
            .inspect(
                RequestContext::new(Default::default()),
                ServiceRequest::from_parts(&inspect_view, &inspect_bytes),
            )
            .await;
        let events = service
            .events(
                RequestContext::new(Default::default()),
                ServiceRequest::from_parts(&event_view, &event_bytes),
            )
            .await;
        if phase == 0 {
            assert_eq!(
                listed.err().unwrap().code,
                connectrpc::ErrorCode::Unavailable
            );
            assert_eq!(
                inspected.err().unwrap().code,
                connectrpc::ErrorCode::Unavailable
            );
            assert_eq!(
                events.err().unwrap().code,
                connectrpc::ErrorCode::Unavailable
            );
        } else {
            let listed = listed.unwrap().body;
            assert_eq!(listed.default_machine, "dev");
            assert_eq!(listed.machines.len(), 1);
            assert_eq!(listed.machines[0].name, "dev");
            let inspected = inspected.unwrap().body;
            assert_eq!(inspected.name, "dev");
            assert_eq!(
                MachineWebPorts::try_from(&*inspected.web_ports).unwrap(),
                web_ports
            );
            assert!(events.is_ok());
        }
        let error = service
            .start(
                RequestContext::new(Default::default()),
                ServiceRequest::from_parts(&start_view, &start_bytes),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.code,
            if phase == 2 {
                connectrpc::ErrorCode::FailedPrecondition
            } else {
                connectrpc::ErrorCode::Unavailable
            }
        );
        assert_eq!(operational.get().is_some(), phase == 2);
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn early_events_subscribe_before_polling_and_preserve_filter_independent_resync() {
    let (directory, early) = crate::connect::tests::storage_runtime();
    let runtime = early.ready().unwrap();
    let service = MachineServiceImpl::new(Arc::default(), early.clone());
    let bytes = bytes::Bytes::from(
        pb::MachineEventsRequest {
            id: "dev".into(),
            action: "started".into(),
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let view = pb::MachineEventsRequest::decode_view(&bytes).unwrap();
    let mut events = service
        .events(
            RequestContext::new(Default::default()),
            ServiceRequest::from_parts(&view, &bytes),
        )
        .await
        .unwrap()
        .body;
    for event in [
        Event::MachineStarted {
            name: "other".into(),
        },
        Event::MachineStopped { name: "dev".into() },
        Event::MachineStarted { name: "dev".into() },
    ] {
        runtime.event_bus().publish(event);
    }
    let started = next_event(&mut events).await;
    assert_eq!(
        (started.name.as_str(), started.action.as_str()),
        ("dev", "started")
    );
    for _ in 0..512 {
        runtime.event_bus().publish(Event::MachineStopped {
            name: "other".into(),
        });
    }
    runtime
        .event_bus()
        .publish(Event::MachineStarted { name: "dev".into() });
    let resync = next_event(&mut events).await;
    assert_eq!(
        (resync.name.as_str(), resync.action.as_str()),
        ("", "resync")
    );
    let started = next_event(&mut events).await;
    assert_eq!(
        (started.name.as_str(), started.action.as_str()),
        ("dev", "started")
    );
    std::fs::remove_dir_all(directory).unwrap();
}

async fn next_event(events: &mut ServiceStream<pb::MachineEvent>) -> pb::MachineEvent {
    tokio::time::timeout(std::time::Duration::from_secs(2), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}
