use super::*;

fn uri(s: &str) -> Uri {
    s.parse().unwrap()
}

#[test]
fn extract_id_from_start_path() {
    let u = uri("/containers/abc123/start");
    assert_eq!(extract_container_id(&u).as_deref(), Some("abc123"));
}

#[test]
fn extract_id_from_versioned_path() {
    let u = uri("/v1.43/containers/def456/stop");
    assert_eq!(extract_container_id(&u).as_deref(), Some("def456"));
}

#[test]
fn extract_id_from_delete_path() {
    let u = uri("/containers/xyz789");
    assert_eq!(extract_container_id(&u).as_deref(), Some("xyz789"));
}

#[test]
fn extract_id_skips_collection_endpoints() {
    assert_eq!(extract_container_id(&uri("/containers/json")), None);
    assert_eq!(extract_container_id(&uri("/containers/create")), None);
    assert_eq!(extract_container_id(&uri("/containers/prune")), None);
}

#[test]
fn extract_id_no_containers_segment() {
    assert_eq!(extract_container_id(&uri("/images/abc/json")), None);
}

#[test]
fn kill_teardown_only_on_terminating_signal() {
    // Default (no signal) and explicit SIGKILL terminate → tear down.
    assert!(kill_terminates_container(&uri("/containers/abc/kill")));
    assert!(kill_terminates_container(&uri(
        "/containers/abc/kill?signal=SIGKILL"
    )));
    assert!(kill_terminates_container(&uri(
        "/containers/abc/kill?signal=KILL"
    )));
    assert!(kill_terminates_container(&uri(
        "/containers/abc/kill?signal=9"
    )));

    // Non-fatal / catchable signals leave the container running → no teardown.
    assert!(!kill_terminates_container(&uri(
        "/containers/abc/kill?signal=SIGHUP"
    )));
    assert!(!kill_terminates_container(&uri(
        "/containers/abc/kill?signal=HUP"
    )));
    assert!(!kill_terminates_container(&uri(
        "/containers/abc/kill?signal=SIGUSR1"
    )));
    assert!(!kill_terminates_container(&uri(
        "/containers/abc/kill?signal=SIGTERM"
    )));
    assert!(!kill_terminates_container(&uri(
        "/containers/abc/kill?signal=15"
    )));
}

#[test]
fn extract_name_from_inspect_json() {
    assert_eq!(
        extract_container_name(br#"{"Id":"abc","Name":"/my-nginx"}"#).as_deref(),
        Some("my-nginx")
    );
    assert_eq!(extract_container_name(br#"{"Name":""}"#), None);
    assert_eq!(extract_container_name(br#"{"Id":"abc"}"#), None);
    assert_eq!(extract_container_name(b"not json"), None);
}

#[test]
fn test_extract_container_dns_info_plain() {
    let json = serde_json::json!({
        "Id": "abc123",
        "Name": "/my-nginx",
        "NetworkSettings": {
            "Networks": { "bridge": { "IPAddress": "172.17.0.2" } }
        }
    });
    let bytes = serde_json::to_vec(&json).unwrap();
    let (aliases, ip) = extract_container_dns_info(&bytes).unwrap();
    assert_eq!(aliases, vec!["my-nginx"]);
    assert_eq!(ip, "172.17.0.2".parse::<IpAddr>().unwrap());
}

#[test]
fn test_extract_container_dns_info_compose() {
    let json = serde_json::json!({
        "Id": "abc123",
        "Name": "/myproject-web-1",
        "Config": {
            "Labels": {
                "com.docker.compose.project": "myproject",
                "com.docker.compose.service": "web"
            }
        },
        "NetworkSettings": {
            "Networks": { "bridge": { "IPAddress": "172.17.0.2" } }
        }
    });
    let bytes = serde_json::to_vec(&json).unwrap();
    let (aliases, ip) = extract_container_dns_info(&bytes).unwrap();
    assert_eq!(aliases, vec!["web.myproject", "myproject-web-1"]);
    assert_eq!(ip, "172.17.0.2".parse::<IpAddr>().unwrap());
}

#[test]
fn test_extract_container_dns_info_ignores_stale_top_level_address() {
    let json = serde_json::json!({
        "Id": "abc123",
        "Name": "/web-app",
        "NetworkSettings": {
            "IPAddress": "192.0.2.99",
            "Networks": {
                "bridge": {
                    "IPAddress": "172.18.0.3"
                }
            }
        }
    });
    let bytes = serde_json::to_vec(&json).unwrap();
    let (aliases, ip) = extract_container_dns_info(&bytes).unwrap();
    assert_eq!(aliases, vec!["web-app"]);
    assert_eq!(ip, "172.18.0.3".parse::<IpAddr>().unwrap());
}

#[test]
fn test_extract_container_dns_info_rejects_top_level_only_address() {
    let json = serde_json::json!({
        "Id": "abc123",
        "Name": "/web-app",
        "NetworkSettings": { "IPAddress": "172.17.0.2" }
    });
    let bytes = serde_json::to_vec(&json).unwrap();
    assert!(extract_container_dns_info(&bytes).is_none());
}

#[test]
fn test_extract_container_dns_info_selects_networks_in_name_order() {
    let mut networks = [
        ("z-bridge", "172.17.0.2"),
        ("a-invalid", "invalid"),
        ("b-ipv6", "2001:db8::1"),
        ("m-app", "172.18.0.3"),
    ];
    for _ in 0..2 {
        let network_map: serde_json::Map<_, _> = networks
            .iter()
            .map(|(name, address)| {
                (
                    (*name).to_owned(),
                    serde_json::json!({ "IPAddress": address }),
                )
            })
            .collect();
        let json = serde_json::json!({
            "Id": "abc123",
            "Name": "/web-app",
            "NetworkSettings": { "Networks": network_map }
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let (aliases, ip) = extract_container_dns_info(&bytes).unwrap();
        assert_eq!(aliases, ["web-app"]);
        assert_eq!(ip, "172.18.0.3".parse::<IpAddr>().unwrap());
        networks.reverse();
    }
}

#[test]
fn test_extract_container_dns_info_no_ip() {
    let json = serde_json::json!({
        "Id": "abc123",
        "Name": "/isolated",
        "NetworkSettings": {
            "IPAddress": "",
            "Networks": {}
        }
    });
    let bytes = serde_json::to_vec(&json).unwrap();
    assert!(extract_container_dns_info(&bytes).is_none());
}

#[test]
fn test_extract_container_dns_info_invalid_json() {
    assert!(extract_container_dns_info(b"not json").is_none());
}

#[test]
fn test_extract_container_dns_info_no_name() {
    let json = serde_json::json!({
        "Id": "abc123",
        "NetworkSettings": {
            "Networks": { "bridge": { "IPAddress": "172.17.0.2" } }
        }
    });
    let bytes = serde_json::to_vec(&json).unwrap();
    assert!(extract_container_dns_info(&bytes).is_none());
}
