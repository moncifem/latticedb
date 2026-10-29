# Guarded WAL compatibility and warmed-cache admission measurements

[Run 40908208450, attempt 1](https://github.com/moncifem/latticedb/actions/runs/40908208450)
completed successfully on 2026-10-29. The pull-request head was
`f9f0307b42766a01261670bc51a23b1dd88b5581`; the actual checkout compiled and executed was
**merge source `fb11b8f657643e250adb2da0d78f4f14ded555cd`**. The frozen reader used immutable
`743b0fb52a495a3f875490268d604ed543be9e64`, original Cargo.lock/toolchain files and clean tracked
source; only the reviewed host harness was added. Later rebases are not this measured source.
The [compact evidence](wal-admission-40908208450.json) retains all 90 timed observations, 18 warmups,
read controls, input/binary/toolchain hashes, API artifact digests and raw-file SHA256 values.

## Compatibility proof

The hosted codec/context tests and every observed bounded upgrade-cut case passed. The actual frozen
old reader examined 4,648 histories as 9,296 raw/recovered snapshots: 8,080 guarded refusals and 1,216
legacy accepts. Guarded snapshots and every already-recovered snapshot preserved all file bytes.
267 raw unguarded snapshots changed during permitted old-reader torn-tail repair; they are not
claimed unchanged. Two additional static controls exercised a legacy row and a guarded LCIX record.
The sweep includes clean and torn-write/lost-directory profiles, power cuts, every bounded append
prefix followed by error, ENOSPC, failed file/directory sync, initial promotion, nonempty rotation
and later guarded rollover. It is a finite selection, not arbitrary-workload or soak evidence.

## Measurement method

Both actual Log implementations were built in release mode with Rust 1.99.1 on the hosted x86-64
Linux runner. One independently framed, valid ChangeSet per segment has increasing literal id, HLC
and LSN. Frozen and new legacy readers use exactly the same files. The new guarded case changes
only the segment header; record bodies remain identical. New-reader modes explicitly advertise
capability mask `0x01`; the frozen harness uses its actual legacy API. Six cases vary segment count and row-value
size, with one recorded warmup and five samples per mode. Execution order alternates and each
segment is explicitly read into the OS page cache immediately before each sample.

The internal monotonic interval surrounds instrumented `Log::open_streaming` admission/recovery:
metadata, locking, reading, CRC/LSN validation, callback counters and filesystem synchronization are
included. Harness setup, JSON formatting and dropping the Log are outside that interval. This is
**warmed-cache admission/open latency, including filesystem calls and synchronization**, not pure CPU
decode time or cold-disk latency. No threshold or statistical significance claim is supplied.

Median open time, milliseconds:

| Segments | Value bytes/record | Frozen legacy | New legacy | New guarded | Legacy delta |
|---:|---:|---:|---:|---:|---:|
| 1 | 64 | 0.271786 | 0.280692 | 0.299758 | +3.28% |
| 1 | 65,536 | 0.465637 | 0.552679 | 0.517754 | +18.69% |
| 32 | 64 | 0.654348 | 0.890188 | 0.835976 | +36.04% |
| 32 | 65,536 | 6.139847 | 6.336466 | 6.400495 | +3.20% |
| 256 | 64 | 2.803129 | 4.739753 | 4.833858 | +69.09% |
| 256 | 65,536 | 46.110430 | 48.300457 | 48.449183 | +4.75% |

For 256 tiny legacy segments (45,568 total file bytes), the observed median difference is
**+1.936624 ms / +69.09%**. For 256 larger segments (16,806,400 bytes), it is
**+2.190027 ms / +4.75%**. These differences compare the whole old/new instrumented open paths;
they are not isolated marginal costs of just one extra syscall.

Median GNU time whole-process peak RSS, KiB:

| Segments | Value bytes/record | Frozen legacy | New legacy | New guarded |
|---:|---:|---:|---:|---:|
| 1 | 64 | 2,224 | 2,232 | 2,232 |
| 1 | 65,536 | 2,404 | 2,360 | 2,360 |
| 32 | 64 | 2,224 | 2,232 | 2,232 |
| 32 | 65,536 | 2,352 | 2,232 | 2,360 |
| 256 | 64 | 2,224 | 2,232 | 2,232 |
| 256 | 65,536 | 2,352 | 2,488 | 2,488 |

RSS includes runtime, stack, code and allocator behavior; small positive or negative differences
are not allocator measurements or evidence of memory savings. Every sample made one full-file read
per segment. New-reader samples additionally made exactly one prefix call per segment, with request,
returned length and Vec capacity all **32 bytes**; old-reader prefix counts were zero. At 256
segments, returned prefix bytes totaled 8,192. These counters observe the shipped bounded StdIo path
and its buffer capacity, without unsafe allocation instrumentation. Names, parsed headers, containers,
records and actual segment buffers also consume memory. This is not a complete allocation bound,
a bound for third-party adapters, full Kernel/RowStore rebuild, or checkpoint-recovery measurement.

## Audit and reproduction

The two retained artifact IDs are `12912577795` (measurement) and `12911658503` (compatibility).
Their API archive SHA256 digests and extracted raw-file SHA256 values are in the compact JSON.
The raw measurement artifact contains every GNU time output, exact argv, CPU/memory/cache context,
dataset file manifests, source hashes and generated host-source hashes. The compatibility artifact
contains the full per-cut before/after manifests and executable/source provenance. Large raw logs,
binaries and manifests are not copied into this repository.

Download the original measurements while the artifact remains available:

```sh
gh run download 40908208450 --repo moncifem/latticedb \
  --name evidence-wal-admission-measurement --dir /tmp/wal-admission-measured
python3 - <<'PY'
import hashlib, json, statistics
from pathlib import Path
p = Path('/tmp/wal-admission-measured/wal-admission-measurement.json')
print('raw SHA256:', hashlib.sha256(p.read_bytes()).hexdigest())
for case in json.loads(p.read_text())['cases']:
    print(case['segments'], case['value_bytes_per_record'], {
        mode: statistics.median(s['reader']['open_ns'] for s in samples)
        for mode, samples in case['samples'].items()
    })
PY
```

To repeat the experiment, use an isolated checkout of the recorded merge source and the pinned
frozen source, original lockfiles, and the recorded `prepare_measurement.py` / release build /
`measure.py` commands. Repeat on a hosted runner; different machines/cache/synchronization behavior
will change timings. Evidence at this source does not establish later-head measurements, performance
acceptance, RFC 0008/ADR 0022 acceptance, a review-period waiver, LCIX activation, or any phase gate.
