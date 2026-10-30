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
            .map(|candidate| (candidate.root.as_str(), candidate.port.value()))
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

use std::net::{Ipv4Addr, UdpSocket};

fn migration_socket() -> UdpSocket {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    assert!(socket.local_addr().unwrap().port() >= 1024);
    socket
}

fn legacy_cli_contents(port: u16) -> String {
    format!("# managed by arcbox\nnameserver 127.0.0.1\nport {port}\nsearch_order 1\n")
}

#[test]
fn migration_reserves_all_ports_and_removes_only_selected_legacy_files() {
    let dir = tempfile::tempdir().unwrap();
    let sockets = [migration_socket(), migration_socket()];
    let addresses = sockets
        .each_ref()
        .map(|socket| socket.local_addr().unwrap());
    let helper = contents(DnsPort::try_from(addresses[0].port()).unwrap());
    let cli = legacy_cli_contents(addresses[1].port());
    let foreign = "nameserver 1.1.1.1\n";
    for (name, content) in [
        ("helper.local", helper.as_str()),
        ("duplicate.local", helper.as_str()),
        ("cli.local", cli.as_str()),
        ("unselected.local", helper.as_str()),
        ("foreign.local", foreign),
    ] {
        fs::write(dir.path().join(name), content).unwrap();
    }
    let selected = roots(&[
        "helper.local",
        "duplicate.local",
        "cli.local",
        "foreign.local",
        "missing.local",
    ]);
    drop(sockets);
    migrate(dir.path(), owner(), &selected, || {
        for address in addresses {
            assert_eq!(
                UdpSocket::bind(address).unwrap_err().kind(),
                std::io::ErrorKind::AddrInUse
            );
        }
        Ok(())
    })
    .unwrap();
    for name in [
        "helper.local",
        "duplicate.local",
        "cli.local",
        "missing.local",
    ] {
        assert!(!dir.path().join(name).try_exists().unwrap());
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("unselected.local")).unwrap(),
        helper
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("foreign.local")).unwrap(),
        foreign
    );
}

#[test]
fn any_busy_legacy_port_prevents_stop_and_preserves_the_entire_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mut sockets = [migration_socket(), migration_socket()];
    sockets.sort_by_key(|socket| socket.local_addr().unwrap().port());
    let [free, busy] = sockets;
    let first = contents(DnsPort::try_from(free.local_addr().unwrap().port()).unwrap());
    let second = contents(DnsPort::try_from(busy.local_addr().unwrap().port()).unwrap());
    fs::write(dir.path().join("a.local"), &first).unwrap();
    fs::write(dir.path().join("b.local"), &second).unwrap();
    drop(free);
    let error = migrate(dir.path(), owner(), &roots(&["a.local", "b.local"]), || {
        panic!("a busy port must prevent helper shutdown")
    })
    .unwrap_err();
    assert!(matches!(error, HelperError::Io { op, .. } if op == "reserve legacy DNS port"));
    assert_eq!(
        fs::read_to_string(dir.path().join("a.local")).unwrap(),
        first
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("b.local")).unwrap(),
        second
    );
    drop(busy);
}

#[test]
fn failed_helper_stop_preserves_every_selected_resolver() {
    let dir = tempfile::tempdir().unwrap();
    let socket = migration_socket();
    let content = contents(DnsPort::try_from(socket.local_addr().unwrap().port()).unwrap());
    for name in ["a.local", "b.local"] {
        fs::write(dir.path().join(name), &content).unwrap();
    }
    drop(socket);
    let failure = HelperError::other("fixture stop failed");
    assert_eq!(
        migrate(
            dir.path(),
            owner(),
            &roots(&["a.local", "b.local"]),
            || Err(failure.clone())
        ),
        Err(failure)
    );
    for name in ["a.local", "b.local"] {
        assert_eq!(fs::read_to_string(dir.path().join(name)).unwrap(), content);
    }
}

#[test]
fn post_stop_content_or_metadata_changes_preserve_the_entire_batch() {
    for change in [
        "canonical",
        "replacement",
        "foreign",
        "writable",
        "symlink",
        "hardlink",
        "directory",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let socket = migration_socket();
        let port = socket.local_addr().unwrap().port();
        let original = contents(DnsPort::try_from(port).unwrap());
        let first = dir.path().join("a.local");
        let second = dir.path().join("b.local");
        fs::write(&first, &original).unwrap();
        fs::write(&second, &original).unwrap();
        let mut replacement = original.clone();
        drop(socket);
        let result = migrate(dir.path(), owner(), &roots(&["a.local", "b.local"]), || {
            match change {
                "canonical" => {
                    replacement = legacy_cli_contents(port);
                    fs::write(&second, &replacement).unwrap();
                }
                "replacement" => {
                    let path = dir.path().join("replacement");
                    fs::write(&path, &original).unwrap();
                    fs::rename(path, &second).unwrap();
                }
                "foreign" => {
                    replacement = "nameserver 1.1.1.1\n".to_owned();
                    fs::write(&second, &replacement).unwrap();
                }
                "writable" => {
                    fs::set_permissions(&second, fs::Permissions::from_mode(0o666)).unwrap();
                }
                "symlink" => {
                    fs::remove_file(&second).unwrap();
                    symlink(&first, &second).unwrap();
                }
                "hardlink" => fs::hard_link(&second, dir.path().join("alias.local")).unwrap(),
                "directory" => {
                    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
                }
                _ => unreachable!(),
            }
            Ok(())
        });
        assert!(result.is_err(), "{change}");
        assert_eq!(fs::read_to_string(&first).unwrap(), original, "{change}");
        assert_eq!(
            fs::read_to_string(&second).unwrap(),
            replacement,
            "{change}"
        );
        if change == "symlink" {
            assert!(fs::symlink_metadata(&second).unwrap().is_symlink());
        }
    }
}
