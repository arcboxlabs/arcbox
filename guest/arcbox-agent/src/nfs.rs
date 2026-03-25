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

        fs::create_dir_all(cfg.export_root)
            .map_err(|e| format!("mkdir {} failed({})", cfg.export_root, e))?;
        fs::create_dir_all(cfg.export_docker)
            .map_err(|e| format!("mkdir {} failed({})", cfg.export_docker, e))?;
        fs::create_dir_all(NFS_STATE_DIR)
            .map_err(|e| format!("mkdir {} failed({})", NFS_STATE_DIR, e))?;

        ensure_nfsd_mount()?;

        if !is_mounted(cfg.export_docker) {
            bind_readonly(DOCKER_DATA_MOUNT_POINT, cfg.export_docker)?;
            notes.push(format!(
                "bound {} -> {} (ro)",
                DOCKER_DATA_MOUNT_POINT, cfg.export_docker
            ));
        }

        write_exports(&cfg).map_err(|e| format!("write {} failed({})", cfg.exports_path, e))?;
        write_nfs_conf(&cfg).map_err(|e| format!("write {} failed({})", cfg.nfs_conf_path, e))?;

        if !mountd_running() {
            spawn_mountd()?;
            notes.push("spawned rpc.mountd".to_string());
        }

        ensure_nfsd_threads(&cfg)?;
        notes.push(format!("ensured rpc.nfsd threads={}", cfg.threads));

        refresh_exports()?;
        notes.push("refreshed exportfs".to_string());

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
        .map_err(|e| format!("mount -t nfsd {} failed({})", NFSD_MOUNTPOINT, e))
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
        let status = Command::new("/sbin/exportfs")
            .args(["-ra"])
            .status()
            .map_err(|e| format!("failed to execute exportfs: {e}"))?;

        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "exportfs -ra exited with {}",
                status.code().unwrap_or(-1)
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

        cmd.spawn()
            .map(|_| ())
            .map_err(|e| format!("failed to spawn rpc.mountd: {e}"))
    }

    fn ensure_nfsd_threads(cfg: &ExportConfig<'_>) -> Result<(), String> {
        let status = Command::new("/sbin/rpc.nfsd")
            .args([cfg.threads])
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .status()
            .map_err(|e| format!("failed to execute rpc.nfsd: {e}"))?;

        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "rpc.nfsd {} exited with {}",
                cfg.threads,
                status.code().unwrap_or(-1)
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
        fs::read_to_string("/proc/mounts").is_ok_and(|content| {
            content.lines().any(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                parts.get(1).is_some_and(|&mountpoint| mountpoint == path)
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
