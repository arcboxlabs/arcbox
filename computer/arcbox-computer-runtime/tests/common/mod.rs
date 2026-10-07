//! Helpers shared by the `integration` test binary.

// Serialize loop users so cleanup assertions cannot target another test's reused device.
#[cfg(target_os = "linux")]
pub static LOOP_DEVICE_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
