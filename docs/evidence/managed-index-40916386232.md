# Managed-index engine and frozen-reader proof

[Run 40916386232, attempt 1](https://github.com/moncifem/latticedb/actions/runs/40916386232)
completed successfully in all three jobs. Its reviewed PR head was
`d0edc12bf59d94a05254023bac59048536603f06`; actual checkout was merge source
`aa134abf47a15cd433375c0025514c13bd83db2e`. Its tree
`1decbed5e2a3c80667fb489602d7fa770388349e` exactly matches that reviewed head's tree.
Later replays preserve the owned integration code but require fresh current-head checks.

Independent review found no new blocking code defect in capability admission, strict/canonical LCIX
framing, nested count preflight, policy/conflict/floor validation before ID/RNG/WAL, single-LSN
publication, immutable index IDs, historical drop/recreate, authoritative scrub or callback fallback.
The review also verified all three downloaded archive SHA256 digests and source/tree provenance.
The [compact record](managed-index-40916386232.json) retains those digests, raw-file hashes,
frozen source/lock/toolchain/harness/executable hashes and all 30 before/after probe manifests.

## Observed scope

The managed selection contains 26 tests. Twenty-five normal semantic tests passed; the hosted-only
export was initially ignored and then explicitly selected and passed. Literal/input-derived oracles
cover guarded rows/definitions/history, valid unguarded LCIX refusal, callback and zero-support
readers, a legacy control, denied first-compound writes leaving RNG/WAL unchanged, the planted guard
omission, synchronous create/update/drop/recreate and registry/entry scrub. A reproduced legacy-LCAT
callback lookup regression failed before the table fallback fix and passes afterward.

The actual RowStore API produced 15 finite first-upgrade histories, covering empty-tail promotion
and nonempty legacy-prefix rotation at every observed mutating-operation cut. Recovery checks ACK
survival, all-or-none row/catalog/index publication, historical prefix reads, literal definitions
and lookups, authoritative index scrub and stable second recovery. Each history exported a raw and
recovered snapshot for the real immutable `743b0fb52a495a3f875490268d604ed543be9e64` reader.

| Frozen-reader observations | Count |
|---|---:|
| Finite engine histories | 15 |
| Raw/recovered probes | 30 |
| Guarded refusals, byte-unchanged | 20 |
| Early legacy states accepted | 10 |
| Raw legacy probes permissibly repaired | 2 |
| Additional static legacy/guarded controls | 2 |

All guarded and already-recovered snapshots were byte-unchanged. The two changed raw legacy states
underwent allowed torn-tail repair; required prefix `(7,a)=x` survived. Frozen tracked source was
clean except the reviewed harness, original lock/toolchain bytes matched immutable Git blobs and
literal fixture hashes matched the probe manifests. The harness calls actual old RowStore code,
not a copied decoder.

The separate 4,648-history core guard matrix is admission evidence, **not 4,648 managed-index
histories**. These 15 engine cases do not establish arbitrary workload/lifecycle fault coverage,
large soak, synchronous index-build performance, retained-state memory bounds or resolution of row
decoder allocation/admission budgets. Artifacts retain logs/manifests and binary hashes, not replayable
raw WAL directories; byte manifests do not prove a syscall trace or broader production readiness.

## Reproduction and decision boundaries

Artifact IDs are `12915802602` (core admission), `12915544087` (managed engine) and `12915079342`
(recovery measurement). Their verified archive hashes are in the JSON. To inspect the original
managed proof while retained:

```sh
gh run download 40916386232 --repo moncifem/latticedb \
  --name evidence-managed-engine-admission --dir /tmp/managed-index-proof
python3 - <<'PY'
import json
from pathlib import Path
p = Path('/tmp/managed-index-proof/managed-index-old-reader.json')
r = json.loads(p.read_text())
print(r['matrix_histories'], len(r['matrix_snapshots']))
for probe in r['matrix_snapshots']:
    assert probe['exit_code'] == 0
    if probe['expectation']['allow_mutation'] != 'true':
        assert probe['before'] == probe['after']
PY
```

Repeat only from an isolated source checkout with original locks and the recorded hosted selection,
export and frozen-build commands. A fresh run must verify its own exact source and current parent.
The [owner decision](managed-index-owner-decision-2026-10-29.md) separately accepts RFC 0005 / ADR 0019
and waives this PR's wait; the evidence itself grants neither acceptance, a release nor a phase gate.
