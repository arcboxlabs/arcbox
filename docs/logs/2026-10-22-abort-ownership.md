# Explicit abort ownership coverage

Two regressions cover pending, queued, and resolved passive dials with and without waker forwarding, plus retained promoted readers. Abort closes abandoned resolver channels and releases queued sockets. Before each reset callback, the corresponding reader is cancelled and marked dead. Retained readers cannot publish buffered upstream bytes after abort. Generation tokens remain live, and the observer receives each established flow's byte totals once. Repeated abort emits nothing.

The dedicated test Mac passed all 177 splicetcp all-feature library tests, formatting, and strict all-target/all-feature Clippy through devenv with Rust 1.96.0. No tests failed or were ignored. This batch changes only regression coverage; the explicit abort implementation is unchanged.

Results: `/private/tmp/arctap-abort-ownership-results-oct22-v1`. Source manifest SHA-256: `a1966516e72514187abd57a33079d56a68c346446fbab976f8cfb5b59756ff32`. Source files, native bindings, DNS, and interfaces remained unchanged. Native App/NE, TUN, VM, and final combined workspace acceptance remain outstanding.
