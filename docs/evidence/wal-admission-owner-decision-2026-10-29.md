# Owner decision: WAL admission acceptance and specific review-period waiver

On 2026-10-29 the project owner explicitly accepted RFC 0008 / ADR 0022 and waived this guard's
seven-full-day review wait in response to the scoped acceptance/waiver question. The response was:

> Accept WAL guard and merge #222 after green CI

The question disclosed the bounded compatibility proof and warmed-cache measured costs: approximately
+1.94 ms / +69.1% for 256 tiny legacy segments and +2.19 ms / +4.75% for the 256 larger-segment case.
[The exact observations and limits](wal-admission-40908208450.md) distinguish PR head `f9f0307` from
actual tested merge source `fb11b8f657643e250adb2da0d78f4f14ded555cd`. Timings include filesystem
calls and synchronization; process RSS/prefix counters are not a whole-recovery allocation bound.

This accepts the reviewed guarded WAL header and capability admission contract, including the
`LATCAP02` version-2 header, 32-byte length, bit-0 capability assignment, strict dispatch and cumulative
requirements, durability protocol, actual recovery context and required bounded `Io::read_prefix`
API for unpublished seam implementations. WAL v1 bytes remain unchanged. The frozen corpus and
independent review support this decision; no storage behavior was changed to record it.

Authorization to merge [PR 222](https://github.com/moncifem/latticedb/pull/222) is conditional on its
exact final head passing CI. This record does not claim that merge has happened.

The decision does **not** accept RFC 0005 / ADR 0019, waive PR 209's separate review hold, activate LCIX
on main, select a checkpoint format, assign a release, or sign any phase gate. PR 209 still requires
its own acceptance/hold decision and current checks after the accepted guard is integrated.
