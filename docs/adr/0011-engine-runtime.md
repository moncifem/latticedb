# ADR 0011: Engine runtime

Status: accepted

## Context

ADR 0008 kept the kernel synchronous and deferred the runtime choice to measurements. The options:

1. **Synchronous kernel on a dedicated writer thread**, with group commit, using blocking file I/O.
2. **An async runtime (tokio) driving the engine's file I/O.**
3. **io_uring** through a `UringIo` behind the `Io` trait, on Linux.

## Decision criteria

Written down before the decision was drawn from the data (the macOS numbers were collected first; the
Linux numbers are not in yet and the revisit triggers are stated against them):

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

A shared virtual disk is not NVMe, so these numbers read against C2 only loosely: the non-fsync share
at batch 64 is 22%, under the 30% trigger below. A `UringIo` prototype was not built: it cannot be built or
measured on the development machine, and the data above does not yet justify it.

## Decision

- The engine commit path stays a synchronous kernel owned by one writer thread, with group commit
  (the bounded commit pipeline). Commit throughput is set by how many change sets share one fsync,
  not by how the bytes reach the file: at batch 1 the disk flush is 98% of the time, and batching
  gave a 55-fold increase in throughput while the I/O mechanism did not change (C1, C2).
- The async runtime (tokio) is used for the API and service layers that handle many connections. It
  does not drive the engine's file I/O; the writer thread is reached through a bounded queue.
- `UringIo` is not adopted now. It remains an option behind the `Io` trait, which preserves C3 and C4.

## Revisit when

- the Linux probe shows the non-fsync share of commit time above 30% at batch 64 on NVMe, or
- read workloads (scans, index builds) show system-call overhead above 20% of their time in a profile.

If either holds, build `UringIo` behind `Io` and run it through the same conformance suite and the
simulator before it carries data.

## Consequences

The simulator, the kernel and the durability tests are unchanged. Throughput work for Phase 2 goes
into batching, the number of fsyncs per commit and the engines' own write patterns before any change
of I/O mechanism.
