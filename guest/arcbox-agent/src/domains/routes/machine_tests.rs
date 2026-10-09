//! Machine readiness must fail closed while preserving asynchronous cleanup errors.

use super::*;
use std::os::unix::fs::PermissionsExt as _;

#[tokio::test]
async fn failed_initial_sweep_never_confirms_readiness_and_still_attempts_cleanup() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let directory = tempfile::tempdir().unwrap();
        let tool = directory.path().join("iptables");
        std::fs::write(&tool, b"#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\nprintf 'selected sweep failure\\n' >&2\nexit 23\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
        let routes = Routes::new(WebServices::default(), &tool);
        let (_commands, inbox) = mpsc::unbounded_channel();
        let (ready, readiness) = oneshot::channel();
        let (_authority, root) = tokio::sync::watch::channel(None);
        let cancel = CancellationToken::new();
        let error = routes.run_machine(inbox, RootSource::Session(root), cancel.clone(), ready).await.unwrap_err();
        assert!(readiness.await.is_err());
        assert!(cancel.is_cancelled());
        let message = format!("{error:#}");
        assert!(message.contains("sweep machine web redirects"), "{message}");
        assert!(message.contains("domain shutdown failed"), "{message}");
        assert_eq!(message.matches("selected sweep failure").count(), 2, "{message}");
        assert_eq!(std::fs::read_to_string(tool.with_extension("calls")).unwrap(), "-t nat -w 2 -S PREROUTING\n-t nat -w 2 -S PREROUTING\n");
    }).await.unwrap();
}
