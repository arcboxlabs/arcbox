# IPv4 UDP receive batching

## Method

Run the Criterion `ipv4_recv` group in `common/arcbox-xnu-net/benches/throughput.rs`. Each iteration sends 64 datagrams on loopback, then receives all 64. Compare scalar `recv_from` with `BatchDgram::recv_batch_from_v4`. Receive slots contain 16,385 bytes. The receiver requests a 1 MiB socket buffer. Both paths wait for readiness after `WouldBlock`.

The timed iteration includes sends, receive setup, readiness waits, and receives. These results do not isolate receive CPU cost or measure a Network Extension. The benchmark checks datagram counts and scalar lengths. Unit tests separately check batch payloads, sources, header reuse, empty datagrams, and truncation boundaries.

Before timing each payload size, the IPv4 benchmarks read back the effective `SO_RCVBUF` and check that the receiver accepts the complete 64-datagram burst. The check sends the whole burst before draining it, verifies payloads and sources, and stops after a two-second deadline. A failed check reports the requested and effective buffer sizes, platform, and sent/received counts. The benchmark does not shrink the burst or add a concurrent receiver. Buffer readback alone cannot prove capacity because the kernel also accounts for packet metadata. This setup check does not change the timed workload or the historical measurements below.

From the ArcBox checkout, run the checked-in [benchmark driver](../../tests/bench/xnu-udp/README.md):

```sh
devenv shell -- tests/bench/xnu-udp/run.sh recv
```

The driver preserves the workload and sampling settings, selects only the receive group, and records the command, environment, output, status, and Criterion data. Use `smoke` instead of `recv` to check all six IPv4 cases without measuring throughput.

## 2026-10-11

Developer Mac, Darwin 25.6.0, Rust 1.96.0. Each cell has 30 Criterion samples after a one-second warmup. Local compilation finished before measurement. The remote Mac ran an independent loopback baseline during measurement.

Historical command, run from the ArcTap devenv environment:

```sh
cargo bench --locked --manifest-path .source/arcbox/Cargo.toml -p arcbox-xnu-net --bench throughput -- ipv4_recv --warm-up-time 1 --measurement-time 3 --sample-size 30
```

| Payload bytes | Scalar, µs per 64 packets | Batch, µs per 64 packets | Point-estimate reduction |
| --- | --- | --- | --- |
| 1472 | 258.58 [249.42, 272.39] | 250.81 [239.75, 262.00] | 3.0% |
| 3972 | 317.10 [294.63, 343.66] | 296.72 [268.32, 334.80] | 6.4% |

Brackets contain the Criterion confidence bounds. The bounds overlap for both sizes. This run does not establish a throughput improvement. Evaluate the ArcTap candidate with adjacent NE measurements before retaining the change.

The [raw output](udp-receive-2026-10-11.txt) includes all cells and outlier reports. Linux runtime behavior and concurrent callers on multiple runtimes were not measured.
