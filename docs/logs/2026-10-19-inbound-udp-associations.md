# Published UDP ports preserve client associations

Date: 2026-10-19

Snell v5 QUIC acceptance reached the first server response but could not finish the handshake through a published Docker UDP port. An independent UDP echo control on `xuan-work-mbp` found that six datagrams from one connected host socket reached the container with source ports `61107` through `61112`.

`InboundRelay::inject_udp` allocated a new source port for every datagram. The listener retained the client endpoint, but `HostEgress::handle_inbound_command` discarded that endpoint before injection. QUIC and other protocols that associate peers with a source endpoint therefore lost their association after the first packet.

The relay now indexes each association by the host listener endpoint and client endpoint. The reply channel identifies the listener generation. Packets in one association retain their guest-visible source port. The allocator skips live reverse mappings. Traffic in either direction refreshes the association's idle time; maintenance removes both indexes after 60 seconds without traffic or after the listener closes.

Each listener owns one reply channel and its UDP socket. Removing a listener closes the reply channel and releases the socket. The relay rejects queued commands for a closed listener. A replacement listener at the same endpoint starts a new association.

The regression module `common/arcbox-proxy/src/inbound_relay/udp_tests.rs` checks source-port stability, reverse routing for two clients, empty datagrams, expiry, listener replacement, port exhaustion, and socket release. `tests/bench/inbound-udp/probe.py` checks the complete Docker path with two interleaved clients and 12 exact echoes.

Run the checks on the acceptance machine. Select the isolated Docker daemon with `DOCKER_HOST` before running the probe:

```sh
cargo test -p arcbox-proxy --lib
python3 tests/bench/inbound-udp/probe.py
```

When a checkout provides devenv, run these commands through `devenv shell --`. The probe requires the Docker CLI and the `node:24-bookworm-slim` image. Set `DOCKER` to select a Docker CLI outside `PATH`.

The Snell acceptance checks retain Quinn's default fixed-bit greasing. A passing echo control alone does not prove QUIC interoperability; the real handshake and stream check must also pass after the daemon replacement.

## Acceptance on the test Mac

All executions below ran on `xuan-work-mbp` (SSH alias `xuan-work-mbp-local`), arm64, macOS 26.5 build 25F71. These runs selected the patched guest dockerd used by the protocol fixtures. A VM restart starts the upstream dockerd again; select the patched binary after each restart. The daemon and protocol binaries were signed and identified by SHA-256 in the acceptance manifests.

| Check | Before the relay fix | After the relay fix |
| --- | --- | --- |
| Two clients, six interleaved datagrams each, including empty payloads | All 12 contents echoed, but source ports changed from `61113` through `61124`; association assertion failed | All 12 contents echoed; each client kept one distinct source port, `61000` and `61001` |
| `arcbox-proxy` library tests | Not used as the before control | 35 passed, including four association lifecycle regressions |
| Snell v5 native UDP | First response arrived; subsequent exchange lost its association | 1200-byte Initial, 64-byte fixed-bit packet, and 64-byte greased packet echoed exactly |
| Snell v5 QUIC | Handshake did not finish | Real Quinn handshake passed; bidirectional streams of 1, 16000, and 256000 bytes passed with default greasing |

The native UDP and QUIC reruns used the unchanged `snell-cid-return-oct19` artifact. Its signed SHA-256 is `8f04cf025346dbb0dc49c5346ab18dad2612a21ad2db550e51dcbffd1ba79e49`. The relay-fixed daemon's signed SHA-256 is `d2c8bc0fb944f7e8d571189d4f60724562c2b279461a9fa71a0ce8763a420f48`. These first passing runs used Tokio 1.50.0.

Evidence in `/private/tmp/arctap-remote-acceptance-oct19` on the test Mac:

- `evidence/arcbox-udp-associations-oct19/`: checked-in probe, failing before log, passing after log, and signed daemon manifest.
- `logs/20261019T112702.696968Z-arcbox-proxy-udp-oct19-all.log`: 35 passing library tests.
- `logs/20261019T113507.213948Z-snell-cid-return-oct19-native-datagram.log`: native UDP shapes.
- `logs/20261019T113526.514979Z-snell-cid-return-oct19-v5-quic.log`: real QUIC handshake and streams.

The official Snell servers still fail the separate empty-UDP acceptance assertions. A direct v6 unsafe-raw control, bypassing the ArcTap decoder, also loses the empty reply while returning the next nonempty reply. The ArcTap path also receives an incomplete 16000-byte v5 datagram. These observations did not establish the v5 cause; the later independent control below identifies the frame boundaries. The formal assertions remain unchanged.


## Final Tokio 1.53.3 validation

The final daemon replaces the deprecated `AsyncFd` constructors with `AsyncFd::register` and `register_with_interest`. Each registration documents the descriptor owner and registration lifetime. The workspace requires Tokio 1.53.3. The release daemon was built through the ArcBox devenv environment with the macOS 26 SDK and the default, `gic`, and `vmnet` features.

The raw daemon SHA-256 is `6b3a80a5634289c01079620727ad264980f093244ece3649e2014371c2df501b`. The signed daemon SHA-256 is `8ee81fe5631831b3268ee7c82f96e88f8c4475a8f66b632c34c5b04855d86e3e`. The test Mac verified the installed binary before the probe and Snell reruns. The isolated guest selected Docker `29.8.2-arcbox-mount-lifetime`, Git commit `8af9fe3-lifecycle`.

The checked-in UDP probe passed again with this daemon and patched guest dockerd: 12 exact echoes, including empty payloads, retained distinct ports `61000` and `61001`. The probe SHA-256 is `282869e724d3ce482b59307c2d5cabd9b0af3a2898b41bf8eb774b16a33d40e5`. The output SHA-256 is `c5345ad9b82a99de2e1c31ede3784982dbcb150ae478a3b8fbdcabb384a5f648`.

Tokio 1.53.3 passed all 35 proxy library tests on the test Mac. The final affected binaries passed another 60 tests:

| Crate | Group | Passed |
| --- | --- | ---: |
| `arcbox-transport` | Library | 26 |
| `arcbox-net` | Datapath loop | 9 |
| `arcbox-xnu-net` | Async batch | 2 |
| `arcbox-engine` | Serial | 7 |
| `arcbox-daemon` | mDNS | 7 |
| `arcbox-daemon` | Services | 9 |

The ignored vmnet relay test requires root and entitlements. That test has compile coverage only and is not counted as passed. Docker create, copy, start, logs, and remove exercised the migrated vsock stream path during the probe and protocol fixtures.

The final Snell binary also passed the native UDP control and the real v5 QUIC handshake and stream test. Its signed SHA-256 is `2770f3c5f37f8b4e4a0f7622607fdbce222167daa87fc27fd448dcc6666afce7`. Both results use acceptance manifest SHA-256 `7e28a1325426c6451cc365efb6c798424e4d5f2e806075292fc23ea6938f6770`.

Final test-Mac logs in the acceptance bundle:

- `20261019T114611.579137Z-arcbox-proxy-tokio-oct19-all.log`: 35 proxy tests.
- `20261019T121225.554098Z-final-arcbox-transport-tokio-oct19-all.log`: transport.
- `20261019T121230.254805Z-final-arcbox-net-tokio-oct19-datapath-loop.log`: datapath loop.
- `20261019T121234.764578Z-final-arcbox-xnu-net-tokio-oct19-async-batch.log`: async batch.
- `20261019T121239.298320Z-final-arcbox-engine-tokio-oct19-serial.log`: serial.
- `20261019T121243.853656Z-final-arcbox-daemon-tokio-oct19-mdns.log`: mDNS.
- `20261019T121248.986065Z-final-arcbox-daemon-tokio-oct19-services.log`: services.
- `20261019T121722.153863Z-final-arctap-outbound-test-snell-interop-oct19-native-datagram.log`: native UDP.
- `20261019T121726.838324Z-final-arctap-outbound-test-snell-interop-oct19-v5-quic.log`: QUIC.

The development Mac holds the remote probe receipt at `/private/tmp/arctap-inbound-udp-associations-tokio-patched.log` and the final affected test copies at `/private/tmp/arctap-final-arcbox-tokio-evidence/`. No acceptance binary ran on the development Mac.

Static checks used pinned Rust 1.96 through `devenv shell -- cargo-clippy clippy` and `devenv shell -- cargo-fmt`. Clippy passed with `-D warnings` for proxy, xnu-net, net, vmnet, engine, daemon, vz, and transport. The direct `cargo-clippy` command avoids the host rustup proxy selected by `cargo clippy` in this environment. Formatting and `git diff --check` passed.

## Independent Snell UDP boundary control

The test Mac ran `/private/tmp/arctap-snell-independent-frames.py` using Python AESGCM and libsodium, without importing ArcTap. The local receipt is the same path with `.log`. The control reproduced a v4 server crash after an empty datagram in a warmed session. The v5 and v6 unshaped/unsafe-raw servers omitted empty replies in both cold and warmed sessions. The echo target logged receiving and sending the empty datagrams. These independent results do not cover v6 default shaping.

For v5.0.1, the independent client received all 16000 bytes across three authenticated frames of 5382, 5382, and 5255 bytes. Only the first frame contained the 19-byte address. The frame headers were `04000000001506`, `04000000001506`, and `04000000001487`. Concatenating the payloads after the first address produced the exact original content. This proves a frame-boundary mismatch rather than lost bytes in the relay.

The supplied Surge 6.9.1 static client analysis confirms one UDP delivery per decoded frame. `SGUDPConnectorSnellV4::socket:didReadData:` at `0x1000e16e4` strips an address from each frame and does not assemble continuations. The V4 decoder at `0x1000a4db4` records each AEAD body length separately. No total datagram length or continuation marker was found in this receive path. The Surge client was not run for this control. ArcTap retains the failing assertions and does not infer datagram boundaries from TCP reads or timeouts.
