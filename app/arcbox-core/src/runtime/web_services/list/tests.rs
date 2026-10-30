//! Typed machine projection and canonical guest selection without native VM fixtures.

use super::*;
use arcbox_connect::v1::{
    GuestWebService, MachineWebService, OpenMachineWebRequest, guest_web_service::Route,
    machine_web_service::HttpsBackend,
};

fn captured() -> CapturedMachineWeb {
    CapturedMachineWeb {
        guest: CapturedWebGuest {
            machine: "first_vm".into(),
            incarnation: arcbox_engine::vm::VmIncarnation::for_test(),
        },
        open: OpenMachineWebRequest {
            machine_name: "first_vm".into(),
            hostname: "first-vm.machine.arcbox.local".into(),
            ipv4_address: "192.168.64.8".into(),
            ..Default::default()
        },
        claim_revision: 1,
        ports: Default::default(),
    }
}

fn route(machine: &CapturedMachineWeb, https_backend: Option<HttpsBackend>) -> GuestWebService {
    GuestWebService {
        route: Some(Route::Machine(Box::new(MachineWebService {
            machine_name: machine.open.machine_name.clone(),
            hostname: machine.open.hostname.clone(),
            ipv4_address: machine.open.ipv4_address.clone(),
            http_port: 8080,
            https_backend,
            ..Default::default()
        }))),
        ..Default::default()
    }
}

#[test]
fn projection_binds_raw_identity_and_preserves_native_or_managed_tls() {
    let machine = captured();
    for (backend, mode, port) in [
        (None, super::super::HttpsMode::None, 0),
        (
            Some(HttpsBackend::NativeHttpsPort(8443)),
            super::super::HttpsMode::Passthrough,
            8443,
        ),
        (
            Some(HttpsBackend::TerminatedHttpPort(8080)),
            super::super::HttpsMode::Managed,
            8080,
        ),
    ] {
        let services = machine_services(vec![route(&machine, backend)], &machine).unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].owner, "machine:first_vm");
        assert_eq!(services[0].http_url, "http://first-vm.machine.arcbox.local");
        assert_eq!(services[0].https_mode, mode);
        assert_eq!(services[0].https_port, port);
        assert_eq!(
            services[0].https_url,
            if port == 0 {
                ""
            } else {
                "https://first-vm.machine.arcbox.local"
            }
        );
    }
}

#[test]
fn projection_rejects_masquerading_variants_identity_and_ports() {
    let machine = captured();
    for bad in [
        GuestWebService::default(),
        GuestWebService {
            route: Some(Route::Container(Box::default())),
            ..Default::default()
        },
        GuestWebService {
            route: Some(Route::Index(Box::default())),
            ..Default::default()
        },
    ] {
        assert!(machine_services(vec![bad], &machine).is_err());
    }
    for field in ["name", "hostname", "address", "http", "https", "empty"] {
        let mut bad = route(&machine, None);
        let Some(Route::Machine(value)) = &mut bad.route else {
            unreachable!()
        };
        match field {
            "name" => value.machine_name = "first-vm".into(),
            "hostname" => value.hostname = "other.machine.arcbox.local".into(),
            "address" => value.ipv4_address = "192.168.64.9".into(),
            "http" => value.http_port = 65536,
            "https" => value.https_backend = Some(HttpsBackend::NativeHttpsPort(0)),
            "empty" => value.http_port = 0,
            _ => unreachable!(),
        }
        assert!(machine_services(vec![bad], &machine).is_err(), "{field}");
    }
    let valid = route(&machine, None);
    assert!(machine_services(vec![valid.clone(), valid], &machine).is_err());
    assert_eq!(
        machine_services(vec![], &machine).unwrap(),
        Vec::<WebService>::new()
    );
}

#[tokio::test]
async fn independent_machine_selection_requires_no_system_vm_and_keeps_raw_names() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(
        Runtime::new(crate::Config {
            data_dir: directory.path().into(),
            ..Default::default()
        })
        .unwrap(),
    );
    assert_eq!(
        runtime.list_web_services(None).await.unwrap(),
        Vec::<WebService>::new()
    );
    runtime
        .machine_manager
        .register_mock_machine("first_vm", 7)
        .unwrap();
    runtime
        .machine_manager
        .register_mock_machine("second", 8)
        .unwrap();
    let mut machines = runtime.machine_manager.list();
    assert_eq!(
        runtime.web_guest_names(machines.clone(), None),
        Vec::<String>::new()
    );
    for machine in &mut machines {
        machine.distro = Some("alpine".into());
    }
    assert_eq!(
        runtime.web_guest_names(machines.clone(), None),
        ["first_vm", "second"]
    );
    let first = "first-vm.machine.arcbox.local".parse().unwrap();
    assert_eq!(
        runtime.web_guest_names(machines.clone(), Some(&first)),
        ["first_vm"]
    );
    let wrong = "firstvm.machine.arcbox.local".parse().unwrap();
    assert_eq!(
        runtime.web_guest_names(machines.clone(), Some(&wrong)),
        Vec::<String>::new()
    );
    let alias = "child.first-vm.machine.arcbox.local".parse().unwrap();
    assert_eq!(
        runtime.web_guest_names(machines.clone(), Some(&alias)),
        Vec::<String>::new()
    );
    for machine in &mut machines {
        machine.state = MachineState::Stopped;
    }
    assert_eq!(
        runtime.web_guest_names(machines, None),
        Vec::<String>::new()
    );
}

#[test]
fn machine_projection_applies_shared_local_ca_scope_to_foreign_suffixes() {
    let mut machine = captured();
    machine.open.hostname = "first-vm.machine.example.test".into();
    let mut managed = route(&machine, Some(HttpsBackend::TerminatedHttpPort(8080)));
    let services = machine_services(vec![managed.clone()], &machine).unwrap();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].http_url, "http://first-vm.machine.example.test");
    assert_eq!(services[0].https_url, "");
    assert_eq!(services[0].https_port, 0);
    let Some(Route::Machine(value)) = &mut managed.route else {
        unreachable!()
    };
    value.http_port = 0;
    assert_eq!(
        machine_services(vec![managed], &machine).unwrap(),
        Vec::<WebService>::new()
    );
    let native = machine_services(
        vec![route(&machine, Some(HttpsBackend::NativeHttpsPort(443)))],
        &machine,
    )
    .unwrap();
    assert_eq!(native[0].https_url, "https://first-vm.machine.example.test");
    assert_eq!(native[0].https_port, 443);
}

#[tokio::test]
async fn system_fixture_and_registered_claim_cannot_publish_a_machine_without_its_physical_vm() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(crate::Config {
        data_dir: directory.path().into(),
        ..Default::default()
    })
    .unwrap()
    .with_network_fixture(crate::runtime::NetworkFixture::default());
    runtime
        .machine_manager
        .register_mock_machine("first_vm", 7)
        .unwrap();
    runtime
        .register_machine_dns("first_vm", "192.168.64.8".parse().unwrap())
        .await
        .unwrap();
    let machine = captured();
    let response = ListGuestWebServicesResponse {
        services: vec![route(
            &machine,
            Some(HttpsBackend::TerminatedHttpPort(8080)),
        )],
        ..Default::default()
    };
    assert!(
        WebGuest::Machine(Box::new(machine))
            .project(&runtime, response, None)
            .is_err()
    );
}
