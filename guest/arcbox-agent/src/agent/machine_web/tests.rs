//! Exact machine ownership and complete configuration validation.

use super::*;
use arcbox_connect::v1::{MachineWebPorts, WebPortPolicy};
use arcbox_dns::registry::Lookup;

fn receiver() -> Receiver {
    let registry = DnsRegistry::default();
    Receiver::new(registry.clone(), WebServices::new(registry))
}

fn bridge() -> Ipv4Addr {
    "192.168.64.8".parse().unwrap()
}

fn open() -> OpenMachineWebRequest {
    OpenMachineWebRequest {
        machine_name: "my_vm".into(),
        hostname: "my-vm.machine.arcbox.local".into(),
        ipv4_address: "192.168.64.8".into(),
        ..Default::default()
    }
}

fn replacement(revision: u64, http: Selection, https: Selection) -> ReplaceMachineWebRequest {
    let policy = |selection| WebPortPolicy {
        selection: Some(selection),
        ..Default::default()
    };
    ReplaceMachineWebRequest {
        source_revision: revision,
        ports: MachineWebPorts {
            http_port: policy(http).into(),
            https_port: policy(https).into(),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    }
}

#[test]
fn exact_raw_identity_and_complete_policy_belong_to_one_session() {
    let receiver = receiver();
    let directory = receiver.web.directory_source();
    directory.ready();
    let session = receiver.open(open(), Some(bridge())).unwrap().unwrap();
    assert!(receiver.open(open(), Some(bridge())).unwrap().is_none());
    {
        let registry = receiver.registry.read().unwrap();
        let Lookup::Matched { claim, address, .. } =
            registry.resolve(&open().hostname.parse().unwrap())
        else {
            panic!("machine exact claim is missing");
        };
        assert_eq!(claim.owner(), "machine:my_vm");
        assert_eq!(address.to_string(), open().ipv4_address);
        assert!(matches!(
            registry.resolve(&"child.my-vm.machine.arcbox.local".parse().unwrap()),
            Lookup::Unmatched
        ));
        assert_eq!(registry.instance_authorities().1, []);
    }
    let request = replacement(
        1,
        Selection::Auto(Box::default()),
        Selection::Off(Box::default()),
    );
    assert_eq!(session.replace(request.clone()).unwrap(), 1);
    {
        let state = receiver.state.lock().unwrap();
        let configuration = state.configuration.as_ref().unwrap();
        assert_eq!(configuration.open, open());
        assert_eq!(configuration.policy.as_ref(), Some(&request));
    }
    assert_eq!(
        receiver.web.snapshot(None),
        [] as [arcbox_connect::v1::GuestWebService; 0]
    );
    drop(session);
    assert!(receiver.state.lock().unwrap().configuration.is_none());
    assert!(
        receiver
            .registry
            .read()
            .unwrap()
            .get("machine:my_vm")
            .is_none()
    );
    assert!(receiver.open(open(), Some(bridge())).unwrap().is_some());
}
