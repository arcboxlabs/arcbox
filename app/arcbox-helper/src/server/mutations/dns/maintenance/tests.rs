use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

use super::*;

fn owner() -> u32 {
    // SAFETY: geteuid has no arguments or failure case.
    unsafe { libc::geteuid() }
}

fn roots(names: &[&str]) -> Vec<Domain> {
    parse_roots(
        &names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
    )
    .unwrap()
}

#[test]
fn check_selects_exact_legacy_files_and_never_changes_them() {
    let dir = tempfile::tempdir().unwrap();
    let helper = contents(DnsPort::try_from(5553).unwrap());
    let cli = "# managed by arcbox\nnameserver 127.0.0.1\nport 5554\nsearch_order 1\n";
    fs::write(dir.path().join("helper.local"), &helper).unwrap();
    fs::write(dir.path().join("cli.local"), cli).unwrap();
    fs::write(dir.path().join("unselected.local"), &helper).unwrap();
    let selected = roots(&["helper.local", "cli.local", "helper.local", "missing.local"]);
    let actual = inspect_legacy(dir.path(), owner(), &selected).unwrap();
    assert_eq!(
        actual
            .iter()
            .map(|(root, port)| (root.as_str(), port.value()))
            .collect::<Vec<_>>(),
        vec![("cli.local", 5554), ("helper.local", 5553)]
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("helper.local")).unwrap(),
        helper
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("cli.local")).unwrap(),
        cli
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
}

#[test]
fn copied_markers_leases_and_noncanonical_ports_are_not_legacy() {
    let generated = contents(DnsPort::try_from(5553).unwrap());
    for content in [
        format!("{generated}# lease 501 production 00000000000000000000000000000001 1 1\n"),
        format!("{generated}nameserver 1.1.1.1\n"),
        generated.replace("port 5553", "port 05553"),
        generated.replace("port 5553", "port 53"),
        generated.trim_end().to_owned(),
        "# managed by arcbox-helper\n".to_owned(),
    ] {
        assert!(legacy_port(content.as_bytes()).is_none(), "{content}");
    }
    assert!(parse_roots(&["local".to_owned()]).is_err());
    assert!(parse_roots(&["../arcbox.local".to_owned()]).is_err());
}

#[test]
fn checks_use_explicit_owner_and_reject_unsafe_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let selected = roots(&["arcbox.local"]);
    let file = dir.path().join("arcbox.local");
    let content = contents(DnsPort::try_from(5553).unwrap());
    fs::write(&file, &content).unwrap();
    assert!(inspect_legacy(dir.path(), owner().wrapping_add(1), &selected).is_err());
    assert_eq!(inspect_legacy(&file, owner(), &[]).unwrap(), []);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(inspect_legacy(dir.path(), owner(), &selected).unwrap(), []);
    fs::remove_file(&file).unwrap();
    symlink("missing", &file).unwrap();
    assert_eq!(inspect_legacy(dir.path(), owner(), &selected).unwrap(), []);
    fs::remove_file(&file).unwrap();
    fs::write(&file, &content).unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
    assert!(inspect_legacy(dir.path(), owner(), &selected).is_err());
    assert_eq!(fs::read_to_string(&file).unwrap(), content);
}
