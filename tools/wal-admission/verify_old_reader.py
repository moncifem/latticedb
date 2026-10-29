#!/usr/bin/env python3
"""Run a real frozen pre-LCIX RowStore binary; preserve source/hash and byte manifests."""
from pathlib import Path
import hashlib
import json
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
SOURCE = '743b0fb52a495a3f875490268d604ed543be9e64'


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def manifest(root):
    return {str(p.relative_to(root)): sha(p) for p in sorted(root.rglob('*')) if p.is_file()}


if __name__ == '__main__':
    binary = Path(sys.argv[1]).resolve()
    source = subprocess.check_output(['git', '-C', str(ROOT / 'frozen'), 'rev-parse', 'HEAD'], text=True).strip()
    if source != SOURCE:
        raise SystemExit('frozen source revision mismatch')
    report = {'source_revision': source, 'harness_sha256': sha(ROOT / 'tools/wal-admission/old_reader.rs'),
              'executable_sha256': sha(binary), 'cases': []}
    for mode, filename in [('legacy', 'wal-admission-v1-rows.golden'), ('guarded', 'wal-admission-v2-compound.golden')]:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            wal = root / 'wal'
            wal.mkdir()
            (wal / '00000000000000000001.seg').write_bytes((ROOT / 'formats/corpus' / filename).read_bytes())
            # The shipped reader takes an exclusive LOCK. Pre-create it so every file can be compared.
            (wal / 'LOCK').write_bytes(b'')
            before = manifest(root)
            run = subprocess.run([str(binary), str(root), mode], text=True, capture_output=True, check=True)
            after = manifest(root)
            if before != after:
                raise SystemExit(f'{mode} old-reader operation modified fixture files')
            report['cases'].append({'mode': mode, 'fixture': filename, 'before': before, 'after': after,
                                    'stdout': run.stdout, 'stderr': run.stderr, 'exit_code': run.returncode})
    (ROOT / 'wal-admission-old-reader.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))

    exported = ROOT / 'target/wal-admission-cases'
    cases = sorted(p for p in exported.iterdir() if p.is_dir())
    if not cases or len(cases) % 2:
        raise SystemExit('missing raw/recovered matrix snapshots')
    stages = {}
    for case in cases:
        number, stage = case.name.split('-', 1)
        stages.setdefault(number, set()).add(stage)
    if any(value != {'raw', 'recovered'} for value in stages.values()):
        raise SystemExit('matrix snapshot pairing is incomplete')
    matrix = []
    for root in cases:
        expect = dict(line.split('=', 1) for line in (root / 'expect.txt').read_text().splitlines())
        mode = expect['mode']
        args = [str(binary), str(root), mode]
        if expect['prefix_row'] == 'true':
            args.append('row-required')
        before = manifest(root)
        run = subprocess.run(args, text=True, capture_output=True, check=True)
        after = manifest(root)
        if expect['allow_mutation'] != 'true' and before != after:
            raise SystemExit(f'{root.name} frozen reader mutated a refused/recovered case')
        matrix.append({'case': root.name, 'expectation': expect, 'before': before, 'after': after,
                       'stdout': run.stdout, 'stderr': run.stderr, 'exit_code': run.returncode})
    report['matrix_snapshots'] = matrix
    report['matrix_histories'] = len(cases) // 2
    (ROOT / 'wal-admission-old-reader.json').write_text(json.dumps(report, indent=2) + '\n')
    print(f'Frozen old-reader verified {len(cases)} raw/recovered snapshots from {len(cases)//2} fault histories')
