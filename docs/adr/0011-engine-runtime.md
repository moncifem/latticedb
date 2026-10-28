# ADR 0011: Engine runtime

Status: accepted

## Context

ADR 0008 kept the kernel synchronous and deferred the runtime choice to measurements. The options:

1. **Synchronous kernel on a dedicated writer thread**, with group commit, using blocking file I/O.
2. **An async runtime (tokio) driving the engine's file I/O.**
3. **io_uring** through a `UringIo` behind the `Io` trait, on Linux.

## Decision criteria

Written down before the decision was drawn from the data (the macOS numbers were collected first, then the Linux
numbers from the weekly workflow, which the revisit triggers are stated against):

- C1. Commit latency and throughput at batch sizes 1 to 64 on a real disk, with the fraction of time
  spent in fsync.
- C2. Whether the I/O mechanism, rather than the disk, limits commit throughput. io_uring or an async
  runtime is only worth its complexity if the non-fsync share of commit time is large.
- C3. The simulator must keep running the real kernel code with no executor behind it (ADR 0008).
- C4. The durability contract documented on `Io` must hold unchanged.

## Data

`io-probe` (crate `lattice-bench`) commits 100-byte change sets through the real kernel on a real
filesystem and measures time inside `sync` and `sync_dir`.

macOS arm64, local SSD, 2,000 commits per row (`sync_all`, which issues a full flush on macOS):

| Batch | Commits per second | Microseconds per commit | Share of time in fsync |
|---|---|---|---|
| 1 | 284 | 3527 | 98% |
| 4 | 1,080 | 926 | 97% |
| 16 | 4,661 | 215 | 96% |
| 64 | 15,611 | 64 | 92% |

The weekly workflow runs the same probe on Linux. First run, GitHub-hosted ubuntu runner, 2,000
commits per row:

| Batch | Commits per second | Microseconds per commit | Share of time in fsync |
|---|---|---|---|
| 1 | 3,478 | 288 | 95% |
| 4 | 13,890 | 72 | 94% |
| 16 | 43,901 | 23 | 90% |
| 64 | 127,721 | 7.8 | 78% |

A shared virtual disk is not NVMe, so these initial numbers read against C2 only loosely: the
non-fsync share at batch 64 was 22%, under the 30% trigger below. The later opt-in prototype was
built and measured on Linux CI rather than on the macOS development machine, as recorded below.

## Opt-in prototype measurement (2026-10-28)

P1-62 now has a real Linux `UringIo` behind `Io`, and a benchmark-only Tokio filesystem adapter.
Neither is selected by the production composition root. The existing synchronous kernel drives all
three adapters; the experiment does not redesign the pipeline around many asynchronous operations.

[Linux run 40789753956](https://github.com/moncifem/latticedb/actions/runs/40789753956), revision
`f17a4e6b39a082d676234e7b876151b60d738369`, passed feature-on Clippy, dependency and structural
checks. On x86_64, AMD EPYC 9V74, Linux 7.0.0-1017-azure and the hosted runner's ext4 disk, it ran
three repetitions of 2,000 100-byte commits per batch and 20 complete kernel scans per row.
`io_uring_disabled` was already 0; no runner security setting was changed.

Each adapter passed the same `Io` conformance suite and verified the committed record count and
payloads after reopening. A separate ring regression verified that a completed directory-write error
is returned normally and that subsequent read/write/fsync requests still work. All 36 result rows
completed. Raw results, including p50/p99 batch acknowledgement and every repetition, are committed
in [the measurement report](../../benches/runtime-spikes/linux-hosted-2026-10-28.json).

Medians across three repetitions (the microseconds column is average elapsed work per commit in
each repetition, not p50 acknowledgement latency):

| Adapter | Batch | Commits/s | Microseconds/commit | Fsync share | Kernel scan records/s |
|---|---:|---:|---:|---:|---:|
| portable | 1 | 667 | 1499.77 | 99.2% | 2,389,383 |
| portable | 4 | 6,911 | 144.70 | 97.1% | 2,405,250 |
| portable | 16 | 12,134 | 82.41 | 96.5% | 2,399,776 |
| portable | 64 | 133,623 | 7.48 | 67.7% | 2,423,234 |
| tokio | 1 | 990 | 1010.22 | 95.8% | 2,363,406 |
| tokio | 4 | 3,984 | 250.99 | 91.3% | 2,367,783 |
| tokio | 16 | 19,598 | 51.02 | 89.2% | 2,370,937 |
| tokio | 64 | 21,330 | 46.88 | 93.8% | 2,361,760 |
| uring | 1 | 1,072 | 932.43 | 96.9% | 2,427,859 |
| uring | 4 | 5,946 | 168.19 | 94.8% | 2,444,483 |
| uring | 16 | 20,542 | 48.68 | 91.3% | 2,446,622 |
| uring | 64 | 139,857 | 7.15 | 63.7% | 2,472,782 |

The hosted disk has large fsync tails: portable batch-1 p50 acknowledgement is about 183 us while
its median p99 is about 36 ms. Backend results must not be read as controlled causal comparisons.
At batch 64, the ring median is only about 5% faster than portable; scan rates are similar and include
WAL decoding, allocation and warm page-cache reads, not a syscall-only profile.

The portable non-fsync share at batch 64 is 32.3%, numerically above 30%, but the revisit trigger
requires NVMe evidence. This shared virtual disk does not meet that condition. The data therefore
closes the missing prototype/comparison work, not the hardware-specific runtime adoption question.
The production synchronous-writer decision remains unchanged. Repeat on relevant storage with
controlled workload ordering and a CPU/system-call profile before deciding to adopt another backend.

The opt-in ring uses one in-flight request, with its pointer-lifetime boundary confined to
`lattice-sys`. Negative completion errors propagate; exceptional submission failure or unwinding while
borrowed memory might still be in flight aborts rather than releasing it. This is reviewed prototype
code, not a claim that Miri executes real ring syscalls or that a real-disk probe proves crash safety.
The core continues to run with `SimIo`, without any executor, in the existing fault simulator (C3).

## Decision

- The engine commit path stays a synchronous kernel owned by one writer thread, with group commit
  (the bounded commit pipeline). Commit throughput is set by how many change sets share one fsync,
  not by how the bytes reach the file: at batch 1 the disk flush is 98% of the time, and batching
  gave a 55-fold increase in throughput while the I/O mechanism did not change (C1, C2).
- The async runtime (tokio) is used for the API and service layers that handle many connections. It
  does not drive the engine's file I/O; the writer thread is reached through a bounded queue.
- `UringIo` is not adopted now. The opt-in prototype remains behind the `Io` trait and is off by
  default. Its hosted-runner comparison does not establish the NVMe-specific adoption trigger.

## Revisit when

- the Linux probe shows the non-fsync share of commit time above 30% at batch 64 on NVMe, or
- read workloads (scans, index builds) show system-call overhead above 20% of their time in a profile.

If either holds, extend and repeat the `UringIo` prototype on the relevant hardware, preserving the
same conformance suite and simulator contract, before it carries production data.

## Consequences

The simulator, the kernel and the durability tests are unchanged. Throughput work for Phase 2 goes
into batching, the number of fsyncs per commit and the engines' own write patterns before any change
of I/O mechanism.
