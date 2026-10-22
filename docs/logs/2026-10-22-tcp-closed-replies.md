# Stateless TCP CLOSED replies

`TcpBridge::closed_reply_ip` implements RFC 9293 CLOSED processing after the caller releases upstream resources. RST input has no reply. ACK input selects a RST-only sequence from SEG.ACK. Other TCP input receives RST|ACK acknowledging the sequence, payload, SYN, and FIN with wrapping arithmetic. The method uses the shared packet parser and returns a checksummed Ethernet frame without retaining flow state.

Four wire-level regressions cover IPv4 and IPv6, flags and sequence wraparound, IP and TCP checksums, non-unicast endpoint rejection, TCP options and trailing padding, IPv6 extensions, fragments, truncation, and non-TCP input.

The dedicated test Mac passed all 171 splicetcp all-feature library tests, with no failures or ignored tests, through devenv with Rust 1.96.0. Formatting, strict all-target/all-feature splicetcp Clippy, and all-target checks for splicetcp, arcbox-proxy, arcbox-net, and arcbox-vmm passed.

Results: `/private/tmp/arctap-closed-results-oct22-v1`. Source manifest SHA-256: `ec1935ff8981b7015d1343a13437b6222bed0453bdab139641ee5d810d79de86`. Source files, native bindings, DNS, and interfaces remained unchanged during validation. No native App/NE, TUN, or VM acceptance ran. Explicit abort and final combined workspace validation remain separate work.
