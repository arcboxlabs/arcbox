# Isolated legacy NAT validation

Run these checks only in a disposable Linux container. The checks require an empty NAT table and a loopback-only network namespace. Do not use host networking, host mounts, a Docker socket mount, or `--privileged`.

The Dockerfile extends the existing local `arcbox-validation-musl:1.96.0` image. Verify its ID before building: `sha256:766c0dfb539f26ef7c15251a25a6874a658103301b7ec2aea53c9b4adf78a0e7`. The base image is a local prerequisite, not a published image. The derived image pins Alpine package versions and selects the production legacy backend at `/sbin/iptables`.

From the repository root, select an independent Docker engine with `DOCKER_HOST`, then build and check the tools:

```sh
docker build --pull=false -t arcbox-validation-musl-net:1.96.0-20261009 tests/bench/local-https
docker run --rm --pull never --network none --cap-drop ALL \
  --cap-add NET_ADMIN --cap-add NET_RAW --security-opt no-new-privileges \
  -e ARCBOX_TEST_ISOLATED_NAT=1 -i arcbox-validation-musl-net:1.96.0-20261009 \
  sh -s < tests/bench/local-https/verify-nat.sh
```

`NET_ADMIN` permits table changes. The legacy backend also needs `NET_RAW` for its control socket. The script checks insertion, lookup, deletion, save, and restore, then verifies the original rules. The container's network namespace disappears even if the script fails.

Build the guest bin tests with the repository's configured musl toolchain:

```sh
cargo test -p arcbox-agent --target aarch64-unknown-linux-musl --bin arcbox-agent --no-run
```

Set `ARCBOX_AGENT_TEST_BINARY` to the executable path printed by Cargo. Run the production publisher regression in a new container:

```sh
docker run --rm --pull never --network none --cap-drop ALL \
  --cap-add NET_ADMIN --cap-add NET_RAW --security-opt no-new-privileges \
  -e ARCBOX_TEST_ISOLATED_NAT=1 -i arcbox-validation-musl-net:1.96.0-20261009 \
  sh -c 'cat > /tmp/agent-tests && chmod 700 /tmp/agent-tests && exec /tmp/agent-tests --ignored --exact domains::index::publisher::kernel_tests::kernel_index_repairs_order_and_preserves_unrelated_rules --nocapture --test-threads=1' \
  < "$ARCBOX_AGENT_TEST_BINARY"
```

The regression uses real `Publisher` and `TaggedRules` operations. It checks stale-port replacement, repeated publication, Docker jump reordering, duplicate removal during repair, DNS withdrawal, and preservation of unrelated rules. It does not establish packet forwarding, production-kernel behavior, or browser HTTPS acceptance.

Run the same command with the exact test name `domains::routes::kernel_tests::kernel_machine_discovery_replaces_routes_and_cleans_up` to check machine discovery and route cleanup. This test uses real `/proc` listener discovery, HTTP probes, and `Routes` operations. It checks disappearance, rediscovery, replacement while the previous backend still listens, native port selection, disabled policies, explicit removal, and shutdown. The test restores the original NAT table and loopback addresses, preserves an unrelated rule, and verifies frontend release. Its 30-second deadline makes stalled discovery or cleanup fail.

The machine test supplies a bound frontend socket without a TLS server and selects an explicit native port without probing it. The result does not establish packet forwarding, native TLS discovery, the signing channel, machine session activation, or distro boot-tool availability.

The machine test selects a temporary executable that records each call before invoking the real legacy tool. It verifies the selected path separately during insertion, removal, and shutdown. Removing that executable must make shutdown fail without deleting rules through another tool; restoring the executable permits cleanup. Kernel snapshots use `/sbin/iptables` independently of the selected path.
