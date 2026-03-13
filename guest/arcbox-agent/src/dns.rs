//! Name resolution via /etc/hosts for containers and sandboxes.
//!
//! Containers use 127.0.0.1 (shared guest network namespace).
//! Sandboxes use their actual TAP IP (e.g. `10.88.0.2`).
//!
//! For compose compatibility, both the full container name and the service
//! name (extracted from the compose naming pattern `project-service-N`) are
//! registered.

#![allow(dead_code)]

/// IP address used for container name resolution.
/// All containers share the guest network namespace, so localhost works.
const HOSTS_IP: &str = "127.0.0.1";

/// Marker comment appended to lines managed by arcbox.
const HOSTS_MARKER: &str = "# arcbox";

/// Path to the hosts file.
const HOSTS_PATH: &str = "/etc/hosts";

/// Adds a DNS entry for a container to /etc/hosts (using 127.0.0.1).
///
/// Registers the container name (and an extracted compose service alias, if
/// applicable) so that other containers can resolve it by name.
pub fn add_container_dns(container_name: &str) {
    let aliases = collect_aliases(container_name);
    add_dns_entry(container_name, HOSTS_IP, &aliases);
}

/// Adds a DNS entry for a sandbox to /etc/hosts with the given IP.
pub fn add_sandbox_dns(sandbox_id: &str, ip: &str) {
    add_dns_entry(sandbox_id, ip, &[sandbox_id.to_string()]);
}

/// Removes the DNS entry for a name from /etc/hosts.
pub fn remove_dns(name: &str) {
    if name.is_empty() {
        return;
    }

    let content = match std::fs::read_to_string(HOSTS_PATH) {
        Ok(c) => c,
        Err(_) => return,
    };

    let marker_tag = format!("{HOSTS_MARKER}:{name}");
    let filtered: Vec<&str> = content
        .lines()
        .filter(|line| !line.contains(&marker_tag))
        .collect();

    // Nothing changed.
    if filtered.len() == content.lines().count() {
        return;
    }

    let mut updated = filtered.join("\n");
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }

    if let Err(e) = std::fs::write(HOSTS_PATH, &updated) {
        tracing::warn!("Failed to remove DNS entry for '{}' from {}: {}", name, HOSTS_PATH, e);
    } else {
        tracing::debug!("Removed DNS entry for '{}'", name);
    }
}

/// Removes the DNS entry for a container from /etc/hosts.
pub fn remove_container_dns(container_name: &str) {
    remove_dns(container_name);
}

// -------------------------------------------------------------------------
// Internal
// -------------------------------------------------------------------------

/// Shared implementation: append an entry to /etc/hosts.
fn add_dns_entry(name: &str, ip: &str, aliases: &[String]) {
    if name.is_empty() || aliases.is_empty() {
        return;
    }

    let names = aliases.join(" ");
    let new_line = format!("{ip}\t{names} {HOSTS_MARKER}:{name}");

    let content = std::fs::read_to_string(HOSTS_PATH).unwrap_or_default();

    let marker_tag = format!("{HOSTS_MARKER}:{name}");
    if content.lines().any(|l| l.contains(&marker_tag)) {
        tracing::debug!("DNS entry for '{}' already exists in {}", name, HOSTS_PATH);
        return;
    }

    let updated = if content.ends_with('\n') || content.is_empty() {
        format!("{content}{new_line}\n")
    } else {
        format!("{content}\n{new_line}\n")
    };

    if let Err(e) = std::fs::write(HOSTS_PATH, &updated) {
        tracing::warn!("Failed to add DNS entry for '{}' to {}: {}", name, HOSTS_PATH, e);
    } else {
        tracing::info!("Added DNS entry: {} -> {}", names, ip);
    }
}

/// Collects all name aliases for a container.
///
/// For a compose container named `project-service-1`, this returns:
/// `["project-service-1", "service"]`
///
/// For a plain container named `mycontainer`, this returns:
/// `["mycontainer"]`
fn collect_aliases(container_name: &str) -> Vec<String> {
    if container_name.is_empty() {
        return vec![];
    }

    let mut names = vec![container_name.to_string()];

    // Docker compose names containers as `project-service-N` (or
    // `project_service_N` with older versions). Extract the service name
    // by stripping the project prefix and the replica suffix.
    if let Some(service) = extract_compose_service(container_name) {
        if service != container_name {
            names.push(service);
        }
    }

    names
}

/// Extracts the compose service name from a container name.
///
/// Compose v2 names: `project-service-N` (hyphen separated)
/// Compose v1 names: `project_service_N` (underscore separated)
///
/// Returns the middle segment (service name) if the pattern matches.
fn extract_compose_service(name: &str) -> Option<String> {
    // Try hyphen-separated pattern first (compose v2).
    // Pattern: anything-SERVICE-digit(s)
    if let Some(pos) = name.rfind('-') {
        let suffix = &name[pos + 1..];
        if suffix.chars().all(|c| c.is_ascii_digit()) && pos > 0 {
            let prefix_and_service = &name[..pos];
            if let Some(first_sep) = prefix_and_service.find('-') {
                return Some(prefix_and_service[first_sep + 1..].to_string());
            }
        }
    }

    // Try underscore-separated pattern (compose v1).
    if let Some(pos) = name.rfind('_') {
        let suffix = &name[pos + 1..];
        if suffix.chars().all(|c| c.is_ascii_digit()) && pos > 0 {
            let prefix_and_service = &name[..pos];
            if let Some(first_sep) = prefix_and_service.find('_') {
                return Some(prefix_and_service[first_sep + 1..].to_string());
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_compose_service_v2() {
        assert_eq!(
            extract_compose_service("myproject-web-1"),
            Some("web".to_string())
        );
        assert_eq!(
            extract_compose_service("myproject-api-server-2"),
            Some("api-server".to_string())
        );
    }

    #[test]
    fn test_extract_compose_service_v1() {
        assert_eq!(
            extract_compose_service("myproject_web_1"),
            Some("web".to_string())
        );
        assert_eq!(
            extract_compose_service("myproject_api_server_2"),
            Some("api_server".to_string())
        );
    }

    #[test]
    fn test_extract_compose_service_plain() {
        assert_eq!(extract_compose_service("mycontainer"), None);
        assert_eq!(extract_compose_service("web"), None);
    }

    #[test]
    fn test_extract_compose_service_edge_cases() {
        // Single character project and service.
        assert_eq!(extract_compose_service("a-b-1"), Some("b".to_string()));
        // No digit suffix.
        assert_eq!(extract_compose_service("project-web-abc"), None);
        // Only project and suffix, no service.
        assert_eq!(extract_compose_service("1"), None);
        assert_eq!(extract_compose_service("-1"), None);
    }

    #[test]
    fn test_collect_aliases() {
        let aliases = collect_aliases("myproject-web-1");
        assert_eq!(aliases, vec!["myproject-web-1", "web"]);

        let aliases = collect_aliases("mycontainer");
        assert_eq!(aliases, vec!["mycontainer"]);
    }

    #[test]
    fn test_collect_aliases_empty() {
        let aliases = collect_aliases("");
        assert!(aliases.is_empty());
    }
}
