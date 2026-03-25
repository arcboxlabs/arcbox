//! Guest-side NFS export management.
//!
//! This module configures a tiny in-kernel NFSv4 server that exposes the
//! dockerd data mount under `/export/docker` as a read-only host mount.

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;
    use std::io;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use arcbox_constants::paths::DOCKER_DATA_MOUNT_POINT;
    use nix::mount::{MsFlags, mount};

    const EXPORT_ROOT: &str = "/export";
    const EXPORT_DOCKER: &str = "/export/docker";
    const NFSD_MOUNTPOINT: &str = "/proc/fs/nfsd";
    const NFS_STATE_DIR: &str = "/var/lib/nfs";
    const EXPORTS_PATH: &str = "/etc/exports";
    const NFS_CONF_PATH: &str = "/etc/nfs.conf";
    const NFSD_PORT: u16 = 2049;
    const NFSD_THREADS: &str = "4";
    const SERVICE_EXPORT: &str = "nfs-export";
    const SERVICE_MOUNTD: &str = "rpc.mountd";
    const SERVICE_NFSD: &str = "rpc.nfsd";

    /// Immutable guest-side NFS export configuration.
    pub struct ExportConfig<'a> {
        pub export_root: &'a str,
        pub export_docker: &'a str,
        pub exports_path: &'a str,
        pub nfs_conf_path: &'a str,
        pub port: u16,
        pub threads: &'a str,
    }

    impl Default for ExportConfig<'_> {
        fn default() -> Self {
            Self {
                export_root: EXPORT_ROOT,
                export_docker: EXPORT_DOCKER,
                exports_path: EXPORTS_PATH,
                nfs_conf_path: NFS_CONF_PATH,
                port: NFSD_PORT,
                threads: NFSD_THREADS,
            }
        }
    }

    pub fn ensure_docker_export() -> Result<Vec<String>, String> {
        let cfg = ExportConfig::default();
        let mut notes = Vec::new();

        tracing::info!(
            var_fstype = mounted_fstype("/var").as_deref().unwrap_or("unmounted"),
            etc_fstype = mounted_fstype("/etc").as_deref().unwrap_or("unmounted"),
            export_fstype = mounted_fstype(cfg.export_root)
                .as_deref()
                .unwrap_or("unmounted"),
            var_lib_nfs_fstype = mounted_fstype(NFS_STATE_DIR)
                .as_deref()
                .unwrap_or("unmounted"),
            "nfs export: mount state before setup"
        );

        tracing::info!("nfs export: ensuring writable export root");
        ensure_export_root_tmpfs(cfg.export_root)?;
        fs::create_dir_all(cfg.export_docker)
            .map_err(|e| format!("mkdir {} failed({})", cfg.export_docker, e))?;
        fs::create_dir_all(NFS_STATE_DIR)
            .map_err(|e| format!("mkdir {} failed({})", NFS_STATE_DIR, e))?;
        // nfsdcltrack stores its SQLite DB here. Must exist before nfsd
        // starts so the UMH upcall can determine there are no prior clients.
        fs::create_dir_all(format!("{NFS_STATE_DIR}/nfsdcltrack"))
            .map_err(|e| format!("mkdir nfsdcltrack failed({e})"))?;

        tracing::info!("nfs export: ensuring nfsd pseudo-fs");
        ensure_nfsd_mount()?;

        if !is_mounted(cfg.export_docker) {
            tracing::info!(
                source = DOCKER_DATA_MOUNT_POINT,
                target = cfg.export_docker,
                "nfs export: binding docker data mount"
            );
            bind_readonly(DOCKER_DATA_MOUNT_POINT, cfg.export_docker)?;
            notes.push(format!(
                "bound {} -> {} (ro)",
                DOCKER_DATA_MOUNT_POINT, cfg.export_docker
            ));
        } else {
            tracing::info!(
                target = cfg.export_docker,
                "nfs export: docker bind mount already present"
            );
        }

        tracing::info!("nfs export: writing exports and nfs.conf");
        write_exports(&cfg).map_err(|e| format!("write {} failed({})", cfg.exports_path, e))?;
        write_nfs_conf(&cfg).map_err(|e| format!("write {} failed({})", cfg.nfs_conf_path, e))?;

        if !mountd_running() {
            tracing::info!("nfs export: starting rpc.mountd");
            spawn_mountd()?;
            notes.push("spawned rpc.mountd".to_string());
        } else {
            tracing::info!("nfs export: rpc.mountd already running");
        }

        tracing::info!("nfs export: refreshing export table");
        refresh_exports()?;
        notes.push("refreshed exportfs".to_string());

        tracing::info!(
            threads = cfg.threads,
            "nfs export: ensuring rpc.nfsd threads"
        );
        ensure_nfsd_threads(&cfg)?;
        notes.push(format!("ensured rpc.nfsd threads={}", cfg.threads));

        tracing::info!(
            export_ready = export_ready(),
            mountd_ready = mountd_ready(),
            nfsd_ready = nfsd_ready(),
            "nfs export: ensure complete"
        );

        Ok(notes)
    }

    pub fn export_root() -> &'static str {
        EXPORT_ROOT
    }

    pub fn port() -> u16 {
        NFSD_PORT
    }

    pub fn service_names() -> (&'static str, &'static str, &'static str) {
        (SERVICE_EXPORT, SERVICE_MOUNTD, SERVICE_NFSD)
    }

    pub fn export_ready() -> bool {
        is_mounted(EXPORT_DOCKER)
            && Path::new(EXPORTS_PATH).exists()
            && Path::new(NFS_CONF_PATH).exists()
    }

    pub fn mountd_ready() -> bool {
        mountd_running()
    }

    pub fn nfsd_ready() -> bool {
        nfsd_thread_count().is_some_and(|count| count > 0) && tcp_port_ready(NFSD_PORT)
    }

    fn ensure_nfsd_mount() -> Result<(), String> {
        if is_mounted(NFSD_MOUNTPOINT) {
            tracing::info!(
                target = NFSD_MOUNTPOINT,
                "nfs export: nfsd pseudo-fs already mounted"
            );
            return Ok(());
        }

        fs::create_dir_all(NFSD_MOUNTPOINT)
            .map_err(|e| format!("mkdir {} failed({})", NFSD_MOUNTPOINT, e))?;
        mount(
            Some("nfsd"),
            NFSD_MOUNTPOINT,
            Some("nfsd"),
            MsFlags::empty(),
            None::<&str>,
        )
        .map_err(|e| format!("mount -t nfsd {} failed({})", NFSD_MOUNTPOINT, e))?;
        tracing::info!(
            target = NFSD_MOUNTPOINT,
            "nfs export: mounted nfsd pseudo-fs"
        );
        Ok(())
    }

    fn ensure_export_root_tmpfs(target: &str) -> Result<(), String> {
        match mounted_fstype(target).as_deref() {
            Some("tmpfs") => {
                tracing::info!(target, "nfs export: export root already mounted as tmpfs");
                return Ok(());
            }
            Some(fstype) => {
                return Err(format!(
                    "unexpected filesystem mounted at {}: {}",
                    target, fstype
                ));
            }
            None => {}
        }

        fs::create_dir_all(target).map_err(|e| format!("mkdir {} failed({})", target, e))?;
        mount(
            Some("tmpfs"),
            target,
            Some("tmpfs"),
            MsFlags::MS_NODEV | MsFlags::MS_NOSUID,
            Some("mode=0755"),
        )
        .map_err(|e| format!("mount -t tmpfs {} failed({})", target, e))?;
        tracing::info!(target, "nfs export: mounted tmpfs export root");
        Ok(())
    }

    fn bind_readonly(source: &str, target: &str) -> Result<(), String> {
        mount(
            Some(source),
            target,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .map_err(|e| format!("bind mount {} -> {} failed({})", source, target, e))?;

        mount(
            None::<&str>,
            target,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
            None::<&str>,
        )
        .map_err(|e| format!("remount readonly {} failed({})", target, e))
    }

    fn write_exports(cfg: &ExportConfig<'_>) -> io::Result<()> {
        fs::write(cfg.exports_path, render_exports(cfg))
    }

    fn write_nfs_conf(cfg: &ExportConfig<'_>) -> io::Result<()> {
        fs::write(cfg.nfs_conf_path, render_nfs_conf(cfg))
    }

    fn refresh_exports() -> Result<(), String> {
        let output = Command::new("/sbin/exportfs")
            .args(["-ra"])
            .output()
            .map_err(|e| format!("failed to execute exportfs: {e}"))?;

        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "exportfs -ra exited with {} stderr='{}' stdout='{}'",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim(),
                String::from_utf8_lossy(&output.stdout).trim()
            ))
        }
    }

    fn spawn_mountd() -> Result<(), String> {
        let mut cmd = Command::new("/sbin/rpc.mountd");
        cmd.arg("-F")
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .stdin(Stdio::null())
            .stdout(daemon_log_file("rpc.mountd"))
            .stderr(daemon_log_file("rpc.mountd"));

        let child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn rpc.mountd: {e}"))?;
        tracing::info!(pid = child.id(), "nfs export: rpc.mountd spawned");
        Ok(())
    }

    fn ensure_nfsd_threads(cfg: &ExportConfig<'_>) -> Result<(), String> {
        let output = Command::new("/sbin/rpc.nfsd")
            .args([cfg.threads])
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .output()
            .map_err(|e| format!("failed to execute rpc.nfsd: {e}"))?;

        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "rpc.nfsd {} exited with {} stderr='{}' stdout='{}'",
                cfg.threads,
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim(),
                String::from_utf8_lossy(&output.stdout).trim()
            ))
        }
    }

    fn mountd_running() -> bool {
        process_named("rpc.mountd")
    }

    fn process_named(name: &str) -> bool {
        let Ok(entries) = fs::read_dir("/proc") else {
            return false;
        };

        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(pid) = file_name.to_str() else {
                continue;
            };
            if !pid.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }

            let Ok(comm) = fs::read_to_string(entry.path().join("comm")) else {
                continue;
            };
            if comm.trim() == name {
                return true;
            }
        }

        false
    }

    fn nfsd_thread_count() -> Option<u32> {
        fs::read_to_string("/proc/fs/nfsd/threads")
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn tcp_port_ready(port: u16) -> bool {
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    }

    fn is_mounted(path: &str) -> bool {
        mounted_fstype(path).is_some()
    }

    fn mounted_fstype(path: &str) -> Option<String> {
        fs::read_to_string("/proc/mounts").ok().and_then(|content| {
            content.lines().find_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                match (parts.get(1), parts.get(2)) {
                    (Some(&mountpoint), Some(&fstype)) if mountpoint == path => {
                        Some(fstype.to_string())
                    }
                    _ => None,
                }
            })
        })
    }

    fn render_exports(cfg: &ExportConfig<'_>) -> String {
        format!(
            "{} *(ro,fsid=0,crossmnt,no_subtree_check,insecure,all_squash,anonuid=0,anongid=0)\n",
            cfg.export_root
        )
    }

    fn render_nfs_conf(cfg: &ExportConfig<'_>) -> String {
        format!(
            "[nfsd]\nvers2 = n\nvers3 = n\nvers4 = y\nudp = n\nport = {}\nthreads = {}\n\n[mountd]\nmanage-gids = n\n",
            cfg.port, cfg.threads
        )
    }

    fn daemon_log_file(name: &str) -> Stdio {
        let log_dir = format!("/arcbox/{}", arcbox_constants::paths::guest::LOG);
        let arcbox_path = format!("{}/{}.log", log_dir, name);
        let tmp_log_path = format!("/tmp/{}.log", name);

        if Path::new("/arcbox").exists() {
            let _ = fs::create_dir_all(&log_dir);
        }

        let log_path = if Path::new("/arcbox").exists() {
            &arcbox_path
        } else {
            &tmp_log_path
        };

        match fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
        {
            Ok(file) => file.into(),
            Err(_) => match fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&tmp_log_path)
            {
                Ok(file) => file.into(),
                Err(_) => Stdio::null(),
            },
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{ExportConfig, render_exports, render_nfs_conf};

        #[test]
        fn render_exports_uses_export_root_and_permissions_policy() {
            let cfg = ExportConfig::default();
            let rendered = render_exports(&cfg);
            assert!(rendered.starts_with("/export *("));
            assert!(rendered.contains("fsid=0"));
            assert!(rendered.contains("insecure"));
            assert!(rendered.contains("all_squash"));
        }

        #[test]
        fn default_export_paths_match_docker_mount() {
            let cfg = ExportConfig::default();
            assert_eq!(cfg.export_root, "/export");
            assert_eq!(cfg.export_docker, "/export/docker");
        }

        #[test]
        fn render_nfs_conf_keeps_v4_only_fixed_port() {
            let cfg = ExportConfig::default();
            let rendered = render_nfs_conf(&cfg);
            assert!(rendered.contains("vers2 = n"));
            assert!(rendered.contains("vers3 = n"));
            assert!(rendered.contains("vers4 = y"));
            assert!(rendered.contains("port = 2049"));
        }
    }
}

#[cfg(target_os = "linux")]
pub use platform::{
    ensure_docker_export, export_ready, export_root, mountd_ready, nfsd_ready, port, service_names,
};

/// Vsock-to-TCP relay for NFS.
///
/// Accepts vsock connections on [`arcbox_constants::ports::NFS_RELAY_PORT`] and
/// relays each bidirectionally to the local nfsd at `127.0.0.1:2049`. This
/// allows the host daemon to reach guest nfsd via vsock, bypassing the bridge
/// NIC entirely.
#[cfg(target_os = "linux")]
pub async fn run_nfs_relay(cancel: tokio_util::sync::CancellationToken) {
    use tokio::io::copy_bidirectional;
    use tokio::net::TcpStream;
    use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};

    let port = arcbox_constants::ports::NFS_RELAY_PORT;
    let addr = VsockAddr::new(VMADDR_CID_ANY, port);
    let mut listener = match VsockListener::bind(addr) {
        Ok(l) => {
            tracing::info!(port, "NFS vsock relay listening");
            l
        }
        Err(e) => {
            tracing::error!(port, error = %e, "failed to bind NFS vsock relay");
            return;
        }
    };

    loop {
        let stream = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tracing::info!("NFS vsock relay shutting down");
                return;
            }
            result = listener.accept() => match result {
                Ok((stream, _)) => stream,
                Err(e) => {
                    tracing::warn!(error = %e, "NFS vsock relay accept failed");
                    continue;
                }
            }
        };

        tokio::spawn(async move {
            match TcpStream::connect("127.0.0.1:2049").await {
                Ok(mut tcp) => {
                    let mut vsock = stream;
                    let _ = copy_bidirectional(&mut vsock, &mut tcp).await;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "NFS relay: failed to connect to local nfsd");
                }
            }
        });
    }
}
