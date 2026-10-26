# Primary TCP cancellation

The bridge checks managed cancellation before forwarding payload in packet interception and fast-path polling. Cancellation emits one guest reset, removes the flow, and releases the primary socket. A peer reset closes the flow without a reply. The shared reset builder preserves the connection sequence and acknowledgment cursors.

The dedicated test Mac passed 164 splicetcp all-feature library tests through devenv with Rust 1.96.0. New real-socket cases cover IPv4 and IPv6, Ethernet and bare IP input, cancellation during polling, no post-cancellation upload, and no reply to a peer reset. Formatting, strict splicetcp Clippy, and all-target checks for splicetcp, arcbox-proxy, arcbox-net, and arcbox-vmm passed.

Source and native bindings remained unchanged during validation. DNS and interface snapshots matched. Results are in `/private/tmp/arctap-primary-cancel-results-oct22-v1`; the source manifest SHA-256 is `a0fabe2a61249ee3995edbfc69c2973730e59bd0bc7567c5ffaf18df568c0e46`.

The [network benchmark record](../benchmarks/network.md#2026-10-22--primary-cancellation) includes the initial paired measurements and their concurrent-compilation limitation. A compiler-idle repeat remains pending. No VM, TUN, native App/NE acceptance, or isolated download-performance verdict is claimed. Handshake cancellation remains a separate integration step.
