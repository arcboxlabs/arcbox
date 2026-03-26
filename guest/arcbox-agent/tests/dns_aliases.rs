//! Integration tests for container DNS alias extraction.

use arcbox_agent::dns::collect_aliases;

#[test]
fn compose_v2_extracts_service_name() {
    let aliases = collect_aliases("myproject-web-1");
    assert_eq!(aliases, vec!["myproject-web-1", "web"]);
}

#[test]
fn compose_v2_multi_segment_service() {
    let aliases = collect_aliases("myproject-api-server-2");
    assert_eq!(aliases, vec!["myproject-api-server-2", "api-server"]);
}

#[test]
fn compose_v1_underscore() {
    let aliases = collect_aliases("myproject_web_1");
    assert_eq!(aliases, vec!["myproject_web_1", "web"]);
}

#[test]
fn plain_container_no_alias() {
    let aliases = collect_aliases("mycontainer");
    assert_eq!(aliases, vec!["mycontainer"]);
}

#[test]
fn empty_name_no_aliases() {
    let aliases = collect_aliases("");
    assert!(aliases.is_empty());
}
