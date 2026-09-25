//! The guest `dockerd` configuration.
//!
//! `/etc/docker/daemon.json` is the union of two sources. ArcBox owns the
//! keys the runtime depends on — container DNS through the gateway, the
//! bridge address pool the host routes to, direct routing, the `nofile`
//! ulimit, and the containerd image store — and the operator supplies the
//! rest (`registry-mirrors`, `insecure-registries`, anything else dockerd
//! accepts) through `[docker]` in the host's `config.toml`, which the host
//! renders into the shared data directory before boot. The operator's
//! keys are merged last, but never over an owned key: a wrong `bip` would
//! break every container's connectivity, so those are dropped with a
//! warning rather than honoured.

use std::collections::BTreeMap;
use std::path::Path;

use arcbox_constants::container_network::ContainerNetwork;
use arcbox_constants::paths::guest;
use serde_json::{Map, Value, json};

/// Target NOFILE limit for the guest VM, matching Docker Desktop / OrbStack.
/// Used by both the init-time rlimit raise and the `daemon.json` ulimit.
pub const NOFILE_LIMIT: u64 = 1_048_576;

/// Keys ArcBox manages; an operator value for one is dropped.
///
/// `features` is owned as a whole: dockerd rejects a `features` map that is
/// also set on the command line, and `containerd-snapshotter` must stay
/// pinned (see [`owned_keys`]).
pub const OWNED_KEYS: &[&str] = &[
    "dns",
    "allow-direct-routing",
    "bip",
    "default-address-pools",
    "default-ulimits",
    "features",
];

/// Guest path of the host-rendered override file.
#[must_use]
pub fn overrides_path() -> String {
    format!(
        "{}/{}/{}",
        guest::MOUNT,
        guest::CONFIG,
        guest::DOCKER_ENGINE_CONFIG
    )
}

/// Reads the host's override file, if the host wrote one.
///
/// A missing file means no overrides. Unreadable or unparsable content is
/// an error the caller reports; it is not silently treated as empty, since
/// the operator wrote something and expects it applied.
pub fn read_overrides(path: &Path) -> Result<Map<String, Value>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(format!("{} is not a JSON object", path.display())),
        Err(e) => Err(format!("parse {}: {e}", path.display())),
    }
}

/// The keys ArcBox manages, for `network`.
///
/// `containerd-snapshotter` stays explicitly `true` even though dockerd ≥ 29
/// defaults to the containerd image store: without the explicit flag, dockerd
/// falls back to the graphdriver whenever it finds prior graphdriver state on
/// the data volume (`graphdriver-prior`), which would silently flip a machine
/// with stale overlay2 remnants back to the legacy store. dockerd 29 logs a
/// benign "no longer needed" warning for it.
fn owned_keys(network: ContainerNetwork) -> Map<String, Value> {
    let bridge = format!(
        "{}/{}",
        network.docker_bridge_gateway(),
        network.docker_network_prefix()
    );
    let Value::Object(map) = json!({
        "dns": ["10.0.2.1"],
        "allow-direct-routing": true,
        "bip": bridge,
        "default-address-pools": [{
            "base": network.to_string(),
            "size": network.docker_network_prefix()
        }],
        "default-ulimits": {
            "nofile": { "Name": "nofile", "Soft": NOFILE_LIMIT, "Hard": NOFILE_LIMIT }
        },
        "features": {
            "containerd-snapshotter": true
        }
    }) else {
        unreachable!("json! object literal")
    };
    map
}

/// The rendered `daemon.json`, plus the operator keys that were refused.
#[derive(Debug)]
pub struct DaemonJson {
    /// The document to write.
    pub content: String,
    /// Operator keys dropped because ArcBox owns them.
    pub refused: Vec<String>,
}

/// Builds `daemon.json` from the managed keys and the operator's overrides.
///
/// Keys are emitted in sorted order so the file is stable across boots and
/// diffs cleanly.
#[must_use]
pub fn render(network: ContainerNetwork, overrides: Map<String, Value>) -> DaemonJson {
    let mut merged: BTreeMap<String, Value> = owned_keys(network).into_iter().collect();
    let mut refused = Vec::new();
    for (key, value) in overrides {
        if OWNED_KEYS.contains(&key.as_str()) {
            refused.push(key);
        } else {
            merged.insert(key, value);
        }
    }
    let content = serde_json::to_string_pretty(&merged).expect("BTreeMap<String, Value> is JSON");
    DaemonJson { content, refused }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_daemon_json() -> Value {
        let rendered = render(ContainerNetwork::default(), Map::new());
        assert!(rendered.refused.is_empty());
        serde_json::from_str(&rendered.content).expect("valid JSON")
    }

    #[test]
    fn daemon_json_contains_nofile_ulimit() {
        let nofile = &default_daemon_json()["default-ulimits"]["nofile"];
        assert_eq!(nofile["Soft"], 1048576);
        assert_eq!(nofile["Hard"], 1048576);
        assert_eq!(nofile["Name"], "nofile");
    }

    #[test]
    fn daemon_json_contains_dns() {
        assert_eq!(default_daemon_json()["dns"][0], "10.0.2.1");
    }

    #[test]
    fn daemon_json_allows_direct_container_routing() {
        assert_eq!(default_daemon_json()["allow-direct-routing"], true);
    }

    #[test]
    fn daemon_json_preserves_the_production_docker_bridge() {
        let value = default_daemon_json();
        assert_eq!(value["bip"], "172.17.0.1/16");
        assert_eq!(value["default-address-pools"][0]["base"], "172.16.0.0/12");
        assert_eq!(value["default-address-pools"][0]["size"], 16);
    }

    #[test]
    fn daemon_json_enables_containerd_snapshotter() {
        assert_eq!(
            default_daemon_json()["features"]["containerd-snapshotter"],
            true
        );
    }

    #[test]
    fn daemon_json_uses_the_selected_container_pool() {
        let network = ContainerNetwork::from_kernel_cmdline(
            "root=/dev/vda arcbox.container_network=10.80.0.0/20",
        )
        .unwrap();
        let value: Value = serde_json::from_str(&render(network, Map::new()).content).unwrap();

        assert_eq!(value["bip"], "10.80.1.1/24");
        assert_eq!(value["default-address-pools"][0]["base"], "10.80.0.0/20");
        assert_eq!(value["default-address-pools"][0]["size"], 24);
    }

    #[test]
    fn operator_keys_are_merged_and_owned_keys_refused() {
        let Value::Object(overrides) = json!({
            "registry-mirrors": ["https://mirror.example.com"],
            "insecure-registries": ["registry.corp:5000"],
            "max-concurrent-downloads": 6,
            "bip": "10.99.0.1/16",
            "features": { "containerd-snapshotter": false }
        }) else {
            unreachable!()
        };

        let rendered = render(ContainerNetwork::default(), overrides);
        let value: Value = serde_json::from_str(&rendered.content).unwrap();

        assert_eq!(
            value["registry-mirrors"],
            json!(["https://mirror.example.com"])
        );
        assert_eq!(value["insecure-registries"], json!(["registry.corp:5000"]));
        assert_eq!(value["max-concurrent-downloads"], 6);
        assert_eq!(value["bip"], "172.17.0.1/16", "owned keys win");
        assert_eq!(value["features"]["containerd-snapshotter"], true);
        assert_eq!(rendered.refused, vec!["bip", "features"]);
    }

    #[test]
    fn read_overrides_distinguishes_missing_from_broken() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docker-engine.json");

        assert!(read_overrides(&path).unwrap().is_empty(), "missing → none");

        std::fs::write(&path, b"{ not json").unwrap();
        assert!(read_overrides(&path).is_err(), "broken → error, not empty");

        std::fs::write(&path, b"[1, 2]").unwrap();
        assert!(read_overrides(&path).is_err(), "non-object → error");

        std::fs::write(&path, br#"{"registry-mirrors": ["https://m"]}"#).unwrap();
        assert_eq!(read_overrides(&path).unwrap().len(), 1);
    }
}
