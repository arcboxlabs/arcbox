# Validated SYN rejection responses

The bridge previously constructed rejection responses by reading IPv4 byte offsets and writing a separate IPv4 RST encoder. The rejection module now parses shared IP/TCP views, checks the Ethernet/IP family match, and uses the common IPv4/IPv6 RST builder. A SYN carrying data receives an acknowledgment for the SYN and its declared payload, including sequence-number wraparound. Ethernet padding contributes no sequence space.

The dedicated xuan-work-mbp test Mac passed 144 splicetcp library tests with all features and no ignored tests, strict all-target/all-feature Clippy, workspace formatting, and default-feature splicetcp/arcbox-proxy compilation. Validation used Rust 1.96.0 and a fresh build directory. New independent byte fixtures verify IPv4/IPv6 endpoint reversal, reset flags, acknowledgment length, scalar checksum residuals, IPv6 Hop-by-Hop parsing, fragment rejection, every truncated prefix, mismatched families, and malformed TCP headers.

The change affects rejected connection attempts. It does not change the packet data loop or activate IPv6 classifier/handshake dispatch. The IPv6 response helper is verified in isolation; native IPv6 acceptance remains pending. Original App, NE, VPN, DNS, and interface state remained unchanged.
