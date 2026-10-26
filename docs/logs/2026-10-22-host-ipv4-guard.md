# Host egress IPv4 header guard

`HostEgress` checks both Ethernet type and the IP version before inbound reply matching or transport dispatch. An IPv6 packet mislabeled with an IPv4 EtherType can no longer reach the IPv4 UDP handler. The existing no-runtime regression now covers both EtherTypes; invalid dispatch would attempt to spawn a socket task.

The dedicated test Mac passed all 38 arcbox-proxy all-feature library tests, formatting, and strict all-target/all-feature Clippy through devenv with Rust 1.96.0. No tests failed or were ignored. The preceding UDP batch passed all-target consumer checks and 177 splicetcp tests. Results: `/private/tmp/arctap-host-ipv4-guard-results-oct22-v1`. Source manifest SHA-256: `4ec051cd172008893932df50f94eff7a434a3e6b516ce26de4dcd6af9581f4df`. Source, native bindings, DNS, and interfaces remained unchanged. Final combined workspace and native acceptance remain outstanding.
