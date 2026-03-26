//! Container/sandbox name alias extraction for DNS registration.
//!
//! Provides compose-aware alias extraction used by `dns_server.rs` and
//! `docker_events.rs` to register containers under both their full name
//! and their compose service name.

/// Collects all name aliases for a container.
///
/// For a compose container named `project-service-1`, this returns:
/// `["project-service-1", "service"]`
///
/// For a plain container named `mycontainer`, this returns:
/// `["mycontainer"]`
pub fn collect_aliases(container_name: &str) -> Vec<String> {
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
        assert_eq!(extract_compose_service("a-b-1"), Some("b".to_string()));
        assert_eq!(extract_compose_service("project-web-abc"), None);
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
