# Recovery scan mutation follow-up

Requirement: P1-69. Part of #42.

## Scope and recorded evidence

[Focused run 40892226094](https://github.com/moncifem/latticedb/actions/runs/40892226094),
attempt 1, tested source `bf1ec110658401ab3e9bc4b4d6959e139121cbd2` with
cargo-mutants 27.1.0, package `lattice-core`, default features, a 120-second timeout
and the recorded ten-mutant filter. Its explicit package baseline passed. Raw
outcomes report eight caught, one unviable, one missed and no timeouts. These are
selection results, not a project mutation score.

The missed mutant at `log.rs:96:15` changes the scan condition from
`off + RECORD_HEADER_LEN <= data.len()` to subtraction. The record header is
17 bytes. Both production callers, recovery and scrub, validate the 16-byte
segment header, start at offset 16, advance to complete record ends and call the
helper only at a damaged offset inside the buffer. Thus the scan starts at 17 or
later and subtraction cannot underflow. The mutant checks every original candidate
plus at most 34 positions. Those extra positions cannot fit a complete header:
checked field reads or the independent record-end check reject them. Return values
are equivalent for these current callers, with bounded additional work.

That is not equivalence for every documented helper input. The helper is internal,
but its byte-scan contract does not require a segment header. For `from < 16`, the
subtraction can underflow in a checked build or exit before a valid candidate in
an unchecked build. The follow-up explicitly documents that existing header-free
contract and tests valid records immediately after offsets 0 through 15, with an
independent LSN threshold assertion. No mutation exclusion is added. Passing unit
tests alone do not establish that the mutant is caught; fresh hosted mutation
verification remains necessary.

## Additional weekly evidence

[Weekly run 40864349680](https://github.com/moncifem/latticedb/actions/runs/40864349680)
uses older source `a88d1ed1670ba0828c2c3689a83bfba7e7043a72`, attempt 1.
The following completed failed-shard ZIP digests matched GitHub artifact metadata;
their manifests bind that source and attempt. These are partial observations.

| Shard | Artifact SHA256 | Relevant result |
| --- | --- | --- |
| dependent/20 | `33c9be68105e566c4a52396c5e3de782eb6e4214cbe490be9928d77a5c9cb85f` | Missed `log.rs:102:28`, record-end addition changed to subtraction |
| dependent/23 | `37d79cfc4242fcabef8feb8aeb3d129213dc9b3ea3ac7b50f4c2dd09445b4f51` | Missed `log.rs:228:55`, recovery prefix subtraction changed to addition |
| dependent/24 | `194d86e2771e0d5c60e26e303db1f8751ec70891e90d271c2ec5a19374132d5b` | Missed recovery prefix subtraction changed to division; scan increment multiplication timed out |
| dependent/25 | `ad1e0b3005e1d9b4a024e248a4649d86af39be400f817791ce71a9795533b38e` | No missed mutants; lexer comparison mutation timed out |

The recovery threshold survivors expose a distinct test gap. A valid LSN 2 after
a damaged byte following LSN 1 must prevent tail truncation. Existing tests with
later LSNs did not pin this immediate successor. The new regression asserts
corruption refusal and exact preservation of the segment bytes. No production
logic changes are required.

The shard 20 survivor is a separate redundancy candidate: decoding independently
checks complete framing. It is not excluded here. The shard 24 timeout log shows
the CLI doctor damage test still running; shard 25 shows `parses_every_construct`
still running. Neither is a baseline or build failure. Interrupted shard 27 has
no complete shard artifact and contributes no completed aggregate result.

The older weekly baseline tested the core/LSL packages while mutant execution
tested the workspace. Its baseline therefore does not establish a matching clean
workspace baseline. The observed missed mutants still represent successful
mutated workspace executions. Current-main fixes and fresh focused verification
must be assessed separately; no complete campaign score or Phase 1 gate pass is
claimed here.
