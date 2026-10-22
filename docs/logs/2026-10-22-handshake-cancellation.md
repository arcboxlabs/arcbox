# TCP handshake cancellation

Handshake polling rejects cancelled queued dial results and connected native sockets. A final ACK cannot promote a cancelled socket or upload its payload. Peer resets are consumed without a reset reply. The forwarding task drops its dial-result receiver when the bridge abandons the handshake.

Reset frames use the next local sequence after a SYN or SYN-ACK has been sent. Passive resets acknowledge the peer SYN. Active resets omit ACK and are sent only when the initial SYN was published. Ordinary sequence values and wraparound are covered.

The dedicated test Mac passed all 167 splicetcp all-feature library tests, with no failures or ignored tests, through devenv with Rust 1.96.0. Formatting and strict all-target/all-feature splicetcp Clippy passed. The cases cover queued dial results, waiting for a final ACK, payload-bearing final ACKs, peer resets, active opens before and after the SYN, and destruction of a pending forwarding task. This batch changes private handshake logic; downstream all-target checks passed in the preceding primary-cancellation batch. Final combined workspace validation remains required.

Results are in `/private/tmp/arctap-handshake-cancel-results-oct22-v2`. Source manifest SHA-256: `3cc9ce8bfd5faf8d32493ad5074fb0b4c598698ef0da9f41451bdf77d20c7ba3`. Source files, native bindings, DNS, and interfaces remained unchanged during validation. No VM, TUN, or native App/NE acceptance ran. Explicit bridge abort and remaining UDP integration are separate batches.
