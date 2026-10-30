use super::*;
use crate::machine::MachineConfig;
use std::sync::Arc;

fn manager(path: &std::path::Path) -> MachineManager {
    MachineManager::new(
        Arc::new(crate::vm::VmManager::new(path.join("snapshots"))),
        path.to_path_buf(),
        crate::vm::HostNetwork::default(),
        crate::event::EventBus::new(),
    )
}

#[test]
fn policy_parser_and_serialization_reject_invalid_ports() {
    for text in ["auto", "off", "1", "8080", "65535"] {
        let policy: WebPortPolicy = text.parse().unwrap();
        assert_eq!(policy.to_string(), text);
        let json = serde_json::to_string(&policy).unwrap();
        assert_eq!(json, format!("\"{text}\""));
        assert_eq!(
            serde_json::from_str::<WebPortPolicy>(&json).unwrap(),
            policy
        );
    }
    for text in ["", "0", "65536", "-1", "https"] {
        assert!(text.parse::<WebPortPolicy>().is_err(), "{text}");
        assert!(serde_json::from_value::<WebPortPolicy>(serde_json::json!(text)).is_err());
    }
}

#[tokio::test]
async fn partial_update_survives_reload_and_failed_save_preserves_memory() {
    let dir = tempfile::tempdir().unwrap();
    let first = manager(dir.path());
    let initial = MachineWebPorts {
        http_port: "8080".parse().unwrap(),
        https_port: WebPortPolicy::Off,
    };
    first
        .create(MachineConfig {
            name: "web".into(),
            web_ports: initial,
            ..Default::default()
        })
        .await
        .unwrap();
    let saved = first
        .set_web_ports("web", None, Some("8443".parse().unwrap()))
        .unwrap();
    assert_eq!(saved.http_port, initial.http_port);
    assert_eq!(first.persistence.load("web").unwrap().web_ports, saved);
    drop(first);
    let reloaded = manager(dir.path());
    assert_eq!(reloaded.get("web").unwrap().web_ports, saved);

    let config = dir.path().join("machines/web/config.toml");
    std::fs::remove_file(&config).unwrap();
    std::fs::create_dir(&config).unwrap();
    assert!(
        reloaded
            .set_web_ports("web", Some(WebPortPolicy::Off), None)
            .is_err()
    );
    assert_eq!(reloaded.get("web").unwrap().web_ports, saved);
}

#[tokio::test]
async fn web_update_respects_transitions_and_storage_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let manager = Arc::new(manager(dir.path()));
    manager
        .create(MachineConfig {
            name: "web".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    for state in [MachineState::Starting, MachineState::Stopping] {
        manager
            .machines
            .write()
            .unwrap()
            .get_mut("web")
            .unwrap()
            .state = state;
        assert!(
            manager
                .set_web_ports("web", Some(WebPortPolicy::Off), None)
                .is_err()
        );
        assert_eq!(
            manager.persistence.load("web").unwrap().web_ports,
            MachineWebPorts::default()
        );
    }
    manager
        .machines
        .write()
        .unwrap()
        .get_mut("web")
        .unwrap()
        .state = MachineState::Running;
    manager
        .set_web_ports("web", Some(WebPortPolicy::Off), None)
        .unwrap();
    manager.create(MachineConfig::default()).await.unwrap();
    let _reservation = manager.reserve_storage().unwrap();
    manager
        .set_web_ports("web", None, Some(WebPortPolicy::Off))
        .unwrap();
    assert!(
        manager
            .set_web_ports("default", Some(WebPortPolicy::Off), None)
            .is_err()
    );
}
