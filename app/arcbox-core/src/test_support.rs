//! Coordination between unit tests that share this crate's test process.

/// Keeps tests that spawn child processes apart from tests that watch a
/// socket being released.
///
/// macOS has no `SOCK_CLOEXEC`: std and mio mark a new socket close-on-exec
/// with a separate `fcntl`, and a child that another thread spawns in between
/// inherits the descriptor and holds it until the child exits. A listener the
/// test then closes keeps accepting, and a port it frees stays bound.
///
/// Tests that spawn hold the write half. Tests that assert a listener closed
/// or a port came free hold the read half, so they still run alongside each
/// other.
pub static CHILD_PROCESSES: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());
