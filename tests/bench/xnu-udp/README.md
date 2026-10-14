# IPv4 UDP benchmark runner

Run from the ArcBox checkout:

```sh
devenv shell -- tests/bench/xnu-udp/run.sh send
devenv shell -- tests/bench/xnu-udp/run.sh recv
devenv shell -- tests/bench/xnu-udp/run.sh smoke
```

The runner invokes the existing `arcbox-xnu-net` Criterion target. The workloads remain in `common/arcbox-xnu-net/benches/throughput.rs` so the crate retains its Cargo benchmark target when packaged. No workload is duplicated here.

`send` selects only the `ipv4` group: 10 samples, a one-second warmup, and a two-second measurement. `recv` selects only the `ipv4_recv` group: 30 samples, a one-second warmup, and a three-second measurement. `smoke` runs all six IPv4 cases in Criterion test mode and produces no throughput estimates. The burst-capacity preflight runs before timing; failure stops the benchmark.

Each invocation prints a fresh result directory under `$TMPDIR` (or `/tmp`). Keep that directory with the measurement record. `environment.txt` records the UTC start time, Git revision and dirty state, OS, architecture, and toolchain; macOS runs also record hardware model, CPU counts, and memory. `argv.txt` records one command argument per line. `output.log` contains Cargo and Criterion output, `exit-code.txt` records the command status, and `criterion/` contains Criterion data from measurement runs. The runner returns the benchmark's exit status.

Use an idle host and record any competing load with the result. These loopback measurements include sender and receiver costs; they do not isolate syscall CPU cost or establish Network Extension throughput. See the [send method and historical results](../../../docs/benchmarks/network.md#2026-10-11--ipv4-udp-batch-sends) and [receive method and historical results](../../../docs/benchmarks/udp-receive.md).
