use std::net::Ipv4Addr;

use arcbox_dns::container::ContainerMetadata;
use serde_json::{Value, json};

const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn inspect() -> Value {
    json!({ "Id": ID, "Name": "/MyProject_Web_1" })
}

#[test]
fn incomplete_identity_is_rejected() {
    for inspect in [
        json!({}),
        json!({ "Name": "/web" }),
        json!({ "Id": ID }),
        json!({ "Id": "", "Name": "/web" }),
        json!({ "Id": ID, "Name": "" }),
        json!({ "Id": ID, "Name": "/" }),
        json!({ "Id": 42, "Name": "/web" }),
        json!({ "Id": ID, "Name": null }),
    ] {
        assert!(
            ContainerMetadata::from_inspect(&inspect).is_none(),
            "{inspect}"
        );
    }
}

#[test]
fn plain_names_preserve_case_and_underscores_without_truncating_identity() {
    let container = ContainerMetadata::from_inspect(&inspect()).unwrap();
    assert_eq!(container.id, ID);
    assert_eq!(container.name, "MyProject_Web_1");
    assert_eq!(container.default_aliases(), ["MyProject_Web_1"]);
}

#[test]
fn compose_aliases_preserve_label_spelling_and_service_first_order() {
    let mut inspect = inspect();
    inspect["Config"] = json!({ "Labels": {
        "com.docker.compose.project": "MyProject",
        "com.docker.compose.service": "Web_Api"
    }});
    let container = ContainerMetadata::from_inspect(&inspect).unwrap();
    assert_eq!(
        container.default_aliases(),
        ["Web_Api.MyProject", "MyProject_Web_1"]
    );
}

#[test]
fn incomplete_compose_labels_leave_only_the_container_name() {
    for labels in [
        Value::Null,
        json!({ "com.docker.compose.project": "project" }),
        json!({ "com.docker.compose.service": "web" }),
        json!({
            "com.docker.compose.project": "",
            "com.docker.compose.service": "web"
        }),
        json!({
            "com.docker.compose.project": "project",
            "com.docker.compose.service": ""
        }),
    ] {
        let mut inspect = inspect();
        inspect["Config"] = json!({ "Labels": labels });
        let container = ContainerMetadata::from_inspect(&inspect).unwrap();
        assert_eq!(container.default_aliases(), ["MyProject_Web_1"]);
    }
}

#[test]
fn network_order_selects_a_stable_primary_and_deduplicates_valid_ipv4_addresses() {
    let mut networks = [
        ("z-bridge", json!({ "IPAddress": "172.17.0.2" })),
        ("m-app", json!({ "IPAddress": "172.20.0.3" })),
        ("n-shared", json!({ "IPAddress": "172.20.0.3" })),
        ("a-empty", json!({ "IPAddress": "" })),
        ("b-invalid", json!({ "IPAddress": "not-an-ip" })),
        ("c-ipv6", json!({ "IPAddress": "2001:db8::1" })),
        ("d-missing", json!({})),
    ];
    let primary = Ipv4Addr::new(172, 20, 0, 3);
    for _ in 0..2 {
        let mut inspect = inspect();
        let network_map: serde_json::Map<_, _> = networks
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect();
        inspect["NetworkSettings"] = json!({
            "IPAddress": "192.0.2.99",
            "Networks": network_map
        });
        let container = ContainerMetadata::from_inspect(&inspect).unwrap();
        assert_eq!(container.primary_ipv4(), Some(primary));
        assert_eq!(
            container.ipv4_addresses(),
            [primary, Ipv4Addr::new(172, 17, 0, 2)]
        );
        networks.reverse();
    }
}

#[test]
fn top_level_address_does_not_replace_missing_network_addresses() {
    for settings in [
        Value::Null,
        json!({ "IPAddress": "192.0.2.99" }),
        json!({ "IPAddress": "192.0.2.99", "Networks": {} }),
        json!({ "IPAddress": "192.0.2.99", "Networks": {
            "host": { "IPAddress": "" }
        }}),
    ] {
        let mut inspect = inspect();
        inspect["NetworkSettings"] = settings;
        let container = ContainerMetadata::from_inspect(&inspect).unwrap();
        assert_eq!(container.primary_ipv4(), None);
        assert_eq!(container.ipv4_addresses(), &[] as &[Ipv4Addr]);
    }
}
