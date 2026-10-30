//! Exact executable inspection for the helper installation stop barrier.

use std::ffi::{CStr, OsStr};
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub(super) fn helper_pids(executable: &Path) -> io::Result<Vec<i32>> {
    let mut matching = Vec::new();
    let maintenance_pid = i32::try_from(std::process::id()).map_err(io::Error::other)?;
    for pid in root_pids()? {
        if pid == maintenance_pid {
            continue;
        }
        let result = process_matches(pid, executable);
        match result {
            Ok(true) => matching.push(pid),
            Ok(false) => {}
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(matching)
}

fn root_pids() -> io::Result<Vec<i32>> {
    const PROC_UID_ONLY: u32 = 4;
    let mut pids = vec![0_i32; 256];
    loop {
        let capacity = i32::try_from(pids.len() * size_of::<i32>()).map_err(io::Error::other)?;
        // SAFETY: the buffer has capacity writable bytes; type 4 selects effective UID 0.
        let bytes =
            unsafe { libc::proc_listpids(PROC_UID_ONLY, 0, pids.as_mut_ptr().cast(), capacity) };
        if bytes <= 0 {
            return Err(io::Error::last_os_error());
        }
        if bytes < capacity {
            pids.truncate(usize::try_from(bytes).map_err(io::Error::other)? / size_of::<i32>());
            pids.retain(|pid| *pid > 0);
            return Ok(pids);
        }
        pids.resize(pids.len() * 2, 0);
    }
}

fn process_matches(pid: i32, executable: &Path) -> io::Result<bool> {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = i32::try_from(size_of::<libc::proc_bsdinfo>()).map_err(io::Error::other)?;
    // SAFETY: info is correctly aligned and has size writable bytes for this flavor.
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if bytes <= 0 {
        return Err(io::Error::last_os_error());
    }
    if bytes != size {
        return Err(io::Error::other(format!(
            "incomplete process metadata for PID {pid}"
        )));
    }
    // SAFETY: proc_pidinfo initialized the complete proc_bsdinfo above.
    let info = unsafe { info.assume_init() };
    let name = info.pbi_comm.map(|byte| byte.to_ne_bytes()[0]);
    let end = name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name.len());
    candidate_matches(info.pbi_uid, &name[..end], executable, || {
        executable_path(pid)
    })
}

fn candidate_matches(
    uid: u32,
    name: &[u8],
    executable: &Path,
    inspect: impl FnOnce() -> io::Result<PathBuf>,
) -> io::Result<bool> {
    // The helper does not rename itself or fork helper workers. The basename only narrows inspection.
    if uid != 0 || name != b"arcbox-helper" {
        return Ok(false);
    }
    Ok(inspect()? == executable)
}

fn executable_path(pid: i32) -> io::Result<PathBuf> {
    let mut buffer = [0_u8; 4096];
    // SAFETY: buffer is writable and its size is the documented proc_pidpath maximum.
    let bytes = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), 4096) };
    if bytes <= 0 {
        return Err(io::Error::last_os_error());
    }
    let path = CStr::from_bytes_until_nul(&buffer).map_err(io::Error::other)?;
    Ok(PathBuf::from(OsStr::from_bytes(path.to_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_root_helper_candidates_inspect_exact_executable_paths() {
        let installed = Path::new("/usr/local/libexec/arcbox-helper");
        for (uid, name) in [(501, b"arcbox-helper".as_slice()), (0, b"tailscaled")] {
            assert!(
                !candidate_matches(uid, name, installed, || panic!("unrelated process")).unwrap()
            );
        }
        assert!(
            candidate_matches(0, b"arcbox-helper", installed, || Ok(installed.to_owned())).unwrap()
        );
        assert!(
            !candidate_matches(0, b"arcbox-helper", installed, || Ok(PathBuf::from(
                "/tmp/arcbox-helper"
            )))
            .unwrap()
        );
        for errno in [libc::EPERM, libc::ENOENT] {
            assert_eq!(
                candidate_matches(0, b"arcbox-helper", installed, || Err(
                    io::Error::from_raw_os_error(errno)
                ))
                .unwrap_err()
                .raw_os_error(),
                Some(errno)
            );
        }
    }

    #[test]
    fn native_executable_inspection_reports_the_running_test_binary() {
        let pid = i32::try_from(std::process::id()).unwrap();
        assert_eq!(
            executable_path(pid).unwrap(),
            std::env::current_exe().unwrap().canonicalize().unwrap()
        );
    }
}
