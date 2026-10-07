//! Helpers shared by the `integration` test binary.

/// Returns true if the process is running with effective UID 0 (root).
#[cfg(target_os = "linux")]
pub fn is_root() -> bool {
    // /proc/self/status Uid line: real  effective  saved  filesystem
    std::fs::read_to_string("/proc/self/status").is_ok_and(|s| {
        s.lines()
            .find(|l| l.starts_with("Uid:"))
            .and_then(|l| l.split_whitespace().nth(2))
            == Some("0")
    })
}
