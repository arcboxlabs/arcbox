use super::*;
use arcbox_connect::v1::MachineService as _;
use arcbox_core::machine::{MachineConfig, MachineWebPorts, WebPortPolicy};
use buffa::Message as _;
use connectrpc::HasMessageView as _;

async fn set(
    service: &MachineServiceImpl,
    request: pb::SetMachineWebPortsRequest,
) -> ServiceResult<pb::MachineWebPorts> {
    let bytes = bytes::Bytes::from(request.encode_to_vec());
    let view = pb::SetMachineWebPortsRequest::decode_view(&bytes).unwrap();
    service
        .set_web_ports(
            RequestContext::new(Default::default()),
            ServiceRequest::from_parts(&view, &bytes),
        )
        .await
}

async fn inspect(service: &MachineServiceImpl) -> MachineWebPorts {
    let bytes = bytes::Bytes::from(
        pb::InspectMachineRequest {
            id: "web".into(),
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let view = pb::InspectMachineRequest::decode_view(&bytes).unwrap();
    let response = service
        .inspect(
            RequestContext::new(Default::default()),
            ServiceRequest::from_parts(&view, &bytes),
        )
        .await
        .unwrap();
    MachineWebPorts::try_from(&*response.body.web_ports).unwrap()
}

#[tokio::test]
async fn web_settings_preserve_omission_and_reject_invalid_updates_atomically() {
    let (directory, shared) = crate::connect::tests::storage_runtime();
    let runtime = shared.ready().unwrap();
    let initial = MachineWebPorts {
        http_port: "8080".parse().unwrap(),
        https_port: WebPortPolicy::Off,
    };
    runtime
        .machine_manager()
        .create(MachineConfig {
            name: "web".into(),
            web_ports: initial,
            ..Default::default()
        })
        .await
        .unwrap();
    let service = MachineServiceImpl::new(shared.clone());
    let response = set(
        &service,
        pb::SetMachineWebPortsRequest {
            id: "web".into(),
            https_port: pb::WebPortPolicy::from(WebPortPolicy::Auto).into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let saved = MachineWebPorts::try_from(&response.body).unwrap();
    assert_eq!(
        saved,
        MachineWebPorts {
            http_port: initial.http_port,
            https_port: WebPortPolicy::Auto
        }
    );
    assert_eq!(inspect(&service).await, saved);
    let error = set(
        &service,
        pb::SetMachineWebPortsRequest {
            id: "web".into(),
            http_port: pb::WebPortPolicy::from(WebPortPolicy::Off).into(),
            https_port: pb::WebPortPolicy {
                selection: Some(pb::web_port_policy::Selection::Port(0)),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        },
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.code, connectrpc::ErrorCode::InvalidArgument);
    assert_eq!(inspect(&service).await, saved);
    let config = directory.join("machines/web/config.toml");
    std::fs::remove_file(&config).unwrap();
    std::fs::create_dir(&config).unwrap();
    assert!(
        set(
            &service,
            pb::SetMachineWebPortsRequest {
                id: "web".into(),
                http_port: pb::WebPortPolicy::from(WebPortPolicy::Off).into(),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    assert_eq!(inspect(&service).await, saved);
    let error = set(
        &service,
        pb::SetMachineWebPortsRequest {
            id: "default".into(),
            ..Default::default()
        },
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.code, connectrpc::ErrorCode::InvalidArgument);
    std::fs::remove_dir_all(directory).unwrap();
}
