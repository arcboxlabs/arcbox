//! Container/sandbox name alias extraction for DNS registration.
//!
//! Provides compose-aware alias extraction used by `dns_server.rs` and
//! `docker_events.rs` to register containers under hierarchical DNS names.
//!
//! Compose containers are registered as `<service>.<project>.arcbox.local`
//! and `<container>.<project>.arcbox.local`. Plain containers keep the flat
//! `<name>.arcbox.local` format.

/// Compose metadata extracted from Docker container labels.
pub struct ComposeInfo {
    pub project: String,
    pub service: String,
}

/// Extracts compose metadata from a Docker labels JSON object.
///
/// Looks for `com.docker.compose.project` and `com.docker.compose.service`.
/// Returns `None` for non-compose containers.
pub fn extract_compose_info(
    labels: &serde_json::Map<String, serde_json::Value>,
) -> Option<ComposeInfo> {
    let project = labels
        .get("com.docker.compose.project")?
        .as_str()
        .filter(|s| !s.is_empty())?;
    let service = labels
        .get("com.docker.compose.service")?
        .as_str()
        .filter(|s| !s.is_empty())?;
    Some(ComposeInfo {
        project: project.to_string(),
        service: service.to_string(),
    })
}

/// Collects all DNS name aliases for a container.
///
/// For a compose container (project=`myproject`, service=`web`, name=`myproject-web-1`):
/// `["web.myproject", "myproject-web-1.myproject"]`
///
/// For a plain container named `mycontainer`:
/// `["mycontainer"]`
pub fn collect_aliases(container_name: &str, compose: Option<&ComposeInfo>) -> Vec<String> {
    if container_name.is_empty() {
        return vec![];
    }

    match compose {
        Some(info) => {
            let service_alias = format!("{}.{}", info.service, info.project);
            // Strip the trailing replica number (e.g. `-1`) from the
            // container name so that `myproject-web-1` becomes
            // `myproject-web.myproject.arcbox.local`.
            let base = strip_replica_suffix(container_name);
            let container_alias = format!("{base}.{}", info.project);
            if service_alias == container_alias {
                vec![service_alias]
            } else {
                vec![service_alias, container_alias]
            }
        }
        None => vec![container_name.to_string()],
    }
}

/// Strips the trailing replica number from a compose container name.
///
/// `myproject-web-1` → `myproject-web`, `myproject_web_1` → `myproject_web`.
/// Returns the name unchanged if it doesn't end with a separator + digits.
fn strip_replica_suffix(name: &str) -> &str {
    // Try hyphen (compose v2) then underscore (compose v1).
    for sep in ['-', '_'] {
        if let Some(pos) = name.rfind(sep) {
            if name[pos + 1..].chars().all(|c| c.is_ascii_digit()) && !name[pos + 1..].is_empty() {
                return &name[..pos];
            }
        }
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compose(project: &str, service: &str) -> ComposeInfo {
        ComposeInfo {
            project: project.to_string(),
            service: service.to_string(),
        }
    }

    #[test]
    fn compose_container_aliases() {
        let info = compose("myproject", "web");
        let aliases = collect_aliases("myproject-web-1", Some(&info));
        assert_eq!(aliases, vec!["web.myproject", "myproject-web.myproject"]);
    }

    #[test]
    fn compose_multi_segment_service() {
        let info = compose("myproject", "api-server");
        let aliases = collect_aliases("myproject-api-server-2", Some(&info));
        assert_eq!(
            aliases,
            vec!["api-server.myproject", "myproject-api-server.myproject"]
        );
    }

    #[test]
    fn compose_v1_underscore() {
        let info = compose("myproject", "web");
        let aliases = collect_aliases("myproject_web_1", Some(&info));
        assert_eq!(aliases, vec!["web.myproject", "myproject_web.myproject"]);
    }

    #[test]
    fn plain_container_no_compose() {
        let aliases = collect_aliases("mycontainer", None);
        assert_eq!(aliases, vec!["mycontainer"]);
    }

    #[test]
    fn empty_name() {
        let aliases = collect_aliases("", None);
        assert!(aliases.is_empty());
    }

    #[test]
    fn dedup_when_service_equals_container() {
        // Edge case: container name happens to match service.project pattern
        let info = compose("myproject", "mycontainer");
        let aliases = collect_aliases("mycontainer", Some(&info));
        assert_eq!(aliases, vec!["mycontainer.myproject"]);
    }

    #[test]
    fn extract_compose_info_from_labels() {
        let mut labels = serde_json::Map::new();
        labels.insert(
            "com.docker.compose.project".to_string(),
            serde_json::Value::String("myproject".to_string()),
        );
        labels.insert(
            "com.docker.compose.service".to_string(),
            serde_json::Value::String("web".to_string()),
        );
        let info = extract_compose_info(&labels).unwrap();
        assert_eq!(info.project, "myproject");
        assert_eq!(info.service, "web");
    }

    #[test]
    fn extract_compose_info_missing_labels() {
        let labels = serde_json::Map::new();
        assert!(extract_compose_info(&labels).is_none());
    }

    #[test]
    fn extract_compose_info_empty_values() {
        let mut labels = serde_json::Map::new();
        labels.insert(
            "com.docker.compose.project".to_string(),
            serde_json::Value::String(String::new()),
        );
        labels.insert(
            "com.docker.compose.service".to_string(),
            serde_json::Value::String("web".to_string()),
        );
        assert!(extract_compose_info(&labels).is_none());
    }
}
