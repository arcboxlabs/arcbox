//! Image config + create request → OCI `config.json`.

use std::collections::HashMap;
use std::path::Path;

use arcbox_oci::{Mount, OCI_VERSION, Process, Root, Spec, User};
use oci2rootfs::ContainerConfig;

use super::WorkloadError;
use crate::ComputerMountSpec;

/// `PATH` for an image whose config sets none, as `docker run` uses.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The workload half of a create request.
#[derive(Debug, Clone, Default)]
pub struct WorkloadRequest {
    /// Replaces the image's `Cmd`; the image's `Entrypoint` stays in front,
    /// as with `docker run <image> <cmd>`. Empty keeps the image's `Cmd`.
    pub cmd: Vec<String>,
    /// Added to the image's `Env`, replacing variables of the same name.
    pub env: HashMap<String, String>,
    /// Empty keeps the image's `WorkingDir`, or `/` when it has none.
    pub working_dir: String,
    /// Docker-style `user[:group]`. Empty keeps the image's `User`, or root.
    pub user: String,
    /// Columns and rows of a terminal to allocate; `None` runs without one.
    pub tty: Option<(u16, u16)>,
    /// Host directories shared into the computer.
    pub mounts: Vec<ComputerMountSpec>,
}

/// The image's `/etc/passwd` and `/etc/group`, which named users resolve
/// against.
#[derive(Debug, Clone, Default)]
pub struct ImageAccounts {
    pub passwd: String,
    pub group: String,
}

impl ImageAccounts {
    /// Read both files from an ext4 rootfs image. A file the image lacks
    /// reads as empty, so only numeric users resolve.
    pub fn read(rootfs: &Path) -> crate::Result<Self> {
        let mut reader = arcbox_ext4::Reader::new(rootfs).map_err(|error| {
            crate::ComputerError::Rootfs(format!("read {}: {error}", rootfs.display()))
        })?;
        let mut read = |path: &str| {
            reader
                .read_file(path, 0, None)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default()
        };
        Ok(Self {
            passwd: read("/etc/passwd"),
            group: read("/etc/group"),
        })
    }
}

/// Build the OCI spec for `request` over an image with `image` config.
///
/// The result passes [`super::Workload::from_spec`]: it sets no field the
/// guest cannot carry out. Named users resolve against `accounts` here, on
/// the host, because OCI names a user by uid and gid only.
pub fn bundle_spec(
    image: Option<&ContainerConfig>,
    request: &WorkloadRequest,
    accounts: &ImageAccounts,
) -> Result<Spec, WorkloadError> {
    let image = image.cloned().unwrap_or_default();
    let mut args = image.entrypoint.unwrap_or_default();
    if request.cmd.is_empty() {
        args.extend(image.cmd.unwrap_or_default());
    } else {
        args.extend(request.cmd.iter().cloned());
    }

    let mut env = image.env;
    let mut added: Vec<_> = request.env.iter().collect();
    added.sort();
    for (key, value) in added {
        let entry = format!("{key}={value}");
        match env
            .iter_mut()
            .find(|existing| existing.split_once('=').map(|(k, _)| k) == Some(key.as_str()))
        {
            Some(existing) => *existing = entry,
            None => env.push(entry),
        }
    }
    if !env.iter().any(|entry| entry.starts_with("PATH=")) {
        env.insert(0, format!("PATH={DEFAULT_PATH}"));
    }

    let cwd = [
        request.working_dir.as_str(),
        image.working_dir.as_deref().unwrap_or_default(),
    ]
    .into_iter()
    .find(|dir| !dir.is_empty())
    .unwrap_or("/")
    .to_owned();
    let user = if request.user.is_empty() {
        image.user.unwrap_or_default()
    } else {
        request.user.clone()
    };
    let (uid, gid) = resolve_user(&user, accounts)?;

    let spec = Spec {
        oci_version: OCI_VERSION.to_owned(),
        root: Some(Root {
            path: "rootfs".to_owned(),
            readonly: false,
        }),
        process: Some(Process {
            terminal: request.tty.is_some(),
            console_size: request.tty.map(|(width, height)| arcbox_oci::ConsoleSize {
                height: height.into(),
                width: width.into(),
            }),
            cwd,
            env,
            args,
            rlimits: Vec::new(),
            apparmor_profile: None,
            capabilities: None,
            no_new_privileges: false,
            oom_score_adj: None,
            selinux_label: None,
            user: Some(User {
                uid,
                gid,
                umask: None,
                additional_gids: Vec::new(),
            }),
            io_priority: None,
            scheduler: None,
            exec_cpu_affinity: None,
        }),
        hostname: None,
        domainname: None,
        mounts: request
            .mounts
            .iter()
            .map(|mount| Mount {
                destination: mount.target.clone(),
                source: Some(mount.source.clone()),
                mount_type: Some("bind".to_owned()),
                options: Some(vec![
                    "rbind".to_owned(),
                    if mount.readonly { "ro" } else { "rw" }.to_owned(),
                ]),
                ..Mount::default()
            })
            .collect(),
        hooks: None,
        annotations: HashMap::new(),
        linux: None,
    };
    spec.validate()?;
    Ok(spec)
}

/// Resolve `user[:group]` the way `docker run --user` does: a numeric uid
/// needs no passwd entry and then defaults to gid 0, a name must exist, and
/// an explicit group wins over the passwd entry's.
///
/// The guest agent resolves `StartCommand.user` with the same rules
/// (`arcbox_vm_agent::user_spec`); the two crates do not depend on each
/// other, so the rules are restated here.
fn resolve_user(spec: &str, accounts: &ImageAccounts) -> Result<(u32, u32), WorkloadError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok((0, 0));
    }
    let (user, group) = match spec.split_once(':') {
        Some((user, group)) => (user, Some(group)),
        None => (spec, None),
    };
    // passwd: name:password:uid:gid:...
    let passwd = || {
        accounts.passwd.lines().filter_map(|line| {
            let fields: Vec<_> = line.split(':').collect();
            Some((
                *fields.first()?,
                fields.get(2)?.parse::<u32>().ok()?,
                fields.get(3)?.parse::<u32>().ok()?,
            ))
        })
    };
    let (uid, passwd_gid) = match user.parse::<u32>() {
        Ok(uid) => (
            uid,
            passwd().find(|entry| entry.1 == uid).map(|entry| entry.2),
        ),
        Err(_) => passwd()
            .find(|entry| entry.0 == user)
            .map(|(_, uid, gid)| (uid, Some(gid)))
            .ok_or_else(|| {
                WorkloadError::User(format!(
                    "user '{user}' not found in the image's /etc/passwd"
                ))
            })?,
    };
    let gid = match group {
        None => passwd_gid.unwrap_or(0),
        Some(group) => match group.parse::<u32>() {
            Ok(gid) => gid,
            // group: name:password:gid:members
            Err(_) => accounts
                .group
                .lines()
                .find_map(|line| {
                    let fields: Vec<_> = line.split(':').collect();
                    (*fields.first()? == group).then(|| fields.get(2)?.parse().ok())?
                })
                .ok_or_else(|| {
                    WorkloadError::User(format!(
                        "group '{group}' not found in the image's /etc/group"
                    ))
                })?,
        },
    };
    Ok((uid, gid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::Workload;

    fn accounts() -> ImageAccounts {
        ImageAccounts {
            passwd: "root:x:0:0:root:/root:/bin/sh\nweb:x:1000:1001::/home/web:/bin/sh\n".into(),
            group: "root:x:0:\nstaff:x:2000:web\n".into(),
        }
    }

    fn image() -> ContainerConfig {
        ContainerConfig {
            entrypoint: Some(vec!["/docker-entrypoint.sh".into()]),
            cmd: Some(vec!["nginx".into(), "-g".into(), "daemon off;".into()]),
            env: vec!["PATH=/usr/bin".into(), "NGINX_VERSION=1.27".into()],
            working_dir: Some("/srv".into()),
            user: Some("web".into()),
            ..ContainerConfig::default()
        }
    }

    #[test]
    fn the_image_config_runs_when_the_request_names_nothing() {
        let spec = bundle_spec(Some(&image()), &WorkloadRequest::default(), &accounts()).unwrap();
        let start = Workload::from_spec(&spec).unwrap().start.unwrap();
        assert_eq!(
            start.cmd,
            ["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"]
        );
        assert_eq!(start.working_dir, "/srv");
        assert_eq!(start.user, "1000:1001");
        assert_eq!(start.env["PATH"], "/usr/bin");
        assert!(!start.tty);
    }

    #[test]
    fn the_request_overrides_cmd_env_cwd_and_user() {
        let request = WorkloadRequest {
            cmd: vec!["sh".into()],
            env: HashMap::from([
                ("NGINX_VERSION".into(), "1.28".into()),
                ("EXTRA".into(), "x=y".into()),
            ]),
            working_dir: "/tmp".into(),
            user: "1000:staff".into(),
            tty: Some((100, 30)),
            mounts: vec![ComputerMountSpec {
                source: "/Users/me/src".into(),
                target: "/src".into(),
                readonly: true,
            }],
        };
        let spec = bundle_spec(Some(&image()), &request, &accounts()).unwrap();
        // The bundle is valid OCI JSON and round-trips.
        let spec = Spec::from_json(&spec.to_json().unwrap()).unwrap();
        let workload = Workload::from_spec(&spec).unwrap();
        let start = workload.start.unwrap();
        // The entrypoint stays, as with `docker run <image> sh`.
        assert_eq!(start.cmd, ["/docker-entrypoint.sh", "sh"]);
        assert_eq!(start.env["NGINX_VERSION"], "1.28");
        assert_eq!(start.env["EXTRA"], "x=y");
        assert_eq!(start.working_dir, "/tmp");
        assert_eq!(start.user, "1000:2000");
        assert!(start.tty);
        assert_eq!((start.tty_width, start.tty_height), (100, 30));
        assert_eq!(workload.mounts, request.mounts);
    }

    #[test]
    fn an_image_without_config_runs_the_request_as_root_with_a_default_path() {
        let request = WorkloadRequest {
            cmd: vec!["/bin/true".into()],
            ..WorkloadRequest::default()
        };
        let spec = bundle_spec(None, &request, &ImageAccounts::default()).unwrap();
        let start = Workload::from_spec(&spec).unwrap().start.unwrap();
        assert_eq!(start.working_dir, "/");
        assert_eq!(start.user, "0:0");
        assert_eq!(start.env["PATH"], DEFAULT_PATH);

        let spec = bundle_spec(None, &WorkloadRequest::default(), &accounts()).unwrap();
        assert!(Workload::from_spec(&spec).unwrap().start.is_none());
    }

    #[test]
    fn users_resolve_like_docker_run() {
        let accounts = accounts();
        for (spec, expected) in [
            ("", (0, 0)),
            ("web", (1000, 1001)),
            ("1000", (1000, 1001)),
            // A uid without a passwd entry runs with gid 0.
            ("4242", (4242, 0)),
            ("web:staff", (1000, 2000)),
            ("web:7", (1000, 7)),
        ] {
            assert_eq!(resolve_user(spec, &accounts).unwrap(), expected, "{spec}");
        }
        for spec in ["nobody", "web:wheel"] {
            assert!(matches!(
                resolve_user(spec, &accounts),
                Err(WorkloadError::User(_))
            ));
        }
    }

    #[test]
    fn accounts_are_read_from_the_rootfs_image() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("rootfs.ext4");
        let mut fmt = arcbox_ext4::Formatter::new(&image, 4096, 8 * 1024 * 1024).unwrap();
        let passwd = accounts().passwd.into_bytes();
        fmt.create(
            "/etc/passwd",
            arcbox_ext4::constants::file_mode::S_IFREG | 0o644,
            None,
            None,
            Some(&mut passwd.as_slice()),
            None,
            None,
            None,
        )
        .unwrap();
        fmt.close().unwrap();

        let read = ImageAccounts::read(&image).unwrap();
        assert_eq!(read.passwd, accounts().passwd);
        assert_eq!(read.group, "");
        assert_eq!(resolve_user("web", &read).unwrap(), (1000, 1001));
    }
}
