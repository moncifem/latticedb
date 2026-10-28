# Linux runtime spike

This opt-in experiment implements P1-62. It does not change the production engine runtime or enable
an async runtime in the deterministic kernel.

On Linux, from a fresh output directory:

```sh
cargo run --release -p lattice-bench --features runtime-spike --bin runtime_probe -- \
  --dir /tmp/lattice-runtime-example --commits 2000 --read-passes 20 --output runtime-probe.json
```

The `io-probe` workflow's manual dispatch runs the experiment in a bounded job. It checks feature-on
Clippy, layer and safety rules; a completed-error regression test; and the same `Io` conformance suite
for every adapter. The report records three repetitions of batches 1, 4, 16 and 64 through the real
kernel, fsync time, throughput, average work per commit, p50/p99 batch acknowledgement latency, kernel
scan throughput, and a reopen/payload check. Job logs record the runner CPU, kernel and filesystem.

- Portable: the existing `StdIo`.
- Tokio: a current-thread runtime driving `tokio::fs`; filesystem work uses Tokio's blocking pool.
  Appends explicitly flush before returning, preserving the immediate visibility contract.
- io_uring: safe `UringIo` over the pointer-lifetime boundary in `lattice-sys`. Reads, writes and full
  fsync use actual ring requests; metadata and locks retain the portable implementation.

Only one ring operation is in flight, preserving the synchronous `Io` contract. The borrowed file and
buffer stay alive through completion. Negative completion results return ordinary I/O errors, and
interrupted submissions retry. Unexpected submission failure or unwinding while borrowed memory may
still be in flight aborts the experiment rather than releasing that memory. The default feature set
contains neither the wrapper nor Tokio.

A denied or unsupported ring is an error, not an automatic portable fallback. The report says
`incomplete`, preserves any portable/Tokio rows, and the job fails. A workflow file or a successful
compile is not a successful runtime measurement. The hosted runner is shared virtual storage; the
results cannot establish the NVMe-specific revisit threshold in ADR 0011. Reads include allocation
and WAL decoding, not only system calls. This tests a synchronous bridge, not every possible Tokio
or io_uring engine design. Runtime adoption still requires evidence against ADR 0011's criteria.
