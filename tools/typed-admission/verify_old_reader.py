#!/usr/bin/env python3
"""Check actual immutable readers and preserve complete raw histories and provenance."""
from pathlib import Path
import hashlib
import json
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
FROZEN_SOURCE = '45926c7fc1f6fa71a7a69ac858c805ab1d9679b0'
HARNESS = 'crates/lattice-engines/examples/typed_old_reader.rs'


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def manifest(root):
    return {str(path.relative_to(root)): sha(path) for path in sorted(root.rglob('*')) if path.is_file()}


def main():
    binary = Path(sys.argv[1]).resolve()
    frozen = ROOT / 'frozen'
    revision = subprocess.check_output(['git', '-C', str(frozen), 'rev-parse', 'HEAD'], text=True).strip()
    if revision != FROZEN_SOURCE:
        raise SystemExit('frozen source mismatch')
    for args in [('diff', '--exit-code'), ('diff', '--cached', '--exit-code', 'HEAD')]:
        subprocess.run(['git', '-C', str(frozen), *args], capture_output=True, check=True)
    untracked = subprocess.check_output(['git', '-C', str(frozen), 'ls-files', '--others', '--exclude-standard'], text=True).splitlines()
    if untracked != [HARNESS]:
        raise SystemExit(f'unexpected frozen untracked paths: {untracked}')
    tracked = {}
    for name in ['Cargo.lock', 'rust-toolchain.toml']:
        original = subprocess.check_output(['git', '-C', str(frozen), 'show', f'HEAD:{name}'])
        if original != (frozen / name).read_bytes():
            raise SystemExit(f'changed frozen {name}')
        tracked[name] = sha(frozen / name)
    if sha(frozen / HARNESS) != sha(ROOT / 'tools/typed-admission/old_reader.rs'):
        raise SystemExit('compiled harness differs from source')
    report_path = ROOT / 'typed-admission-old-reader.json'
    report = {'schema': 1, 'status': 'incomplete', 'format_status': 'proposed',
        'source_revision': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
        'frozen_revision': revision, 'frozen_tracked_clean': True, 'allowed_untracked_harness': HARNESS,
        'tracked_input_sha256': tracked, 'harness_sha256': sha(frozen / HARNESS),
        'executable_sha256': sha(binary), 'rustc_version_verbose': subprocess.check_output(['rustc', '-Vv'], text=True),
        'cargo_version': subprocess.check_output(['cargo', '-V'], text=True), 'cases': []}
    report_path.write_text(json.dumps(report, indent=2) + '\n')
    cases = sorted((ROOT / 'target/typed-admission-cases').iterdir())
    expected = {'actual-legacy', 'actual-typed', 'actual-typed-torn-tail', 'independent-cap4', 'independent-cap5'}
    if {case.name for case in cases} != expected or not all(case.is_dir() for case in cases):
        raise SystemExit('missing or unexpected exported histories')
    for case in cases:
        mode = (case / 'mode.txt').read_text()
        if mode != ('legacy' if case.name == 'actual-legacy' else 'guarded'):
            raise SystemExit('case expectation mismatch')
        before = manifest(case)
        run = subprocess.run([str(binary), str(case), mode], text=True, capture_output=True)
        after = manifest(case)
        report['cases'].append({'case': case.name, 'expectation': mode, 'before': before, 'after': after,
            'exit_code': run.returncode, 'stdout': run.stdout, 'stderr': run.stderr})
        report_path.write_text(json.dumps(report, indent=2) + '\n')
        if run.returncode != 0 or before != after:
            raise SystemExit(f'{case.name}: frozen-reader failure or mutated history; raw outcome retained')
    report['status'] = 'passed finite fixture and successful-engine-history checks; not power-loss soak or format acceptance'
    report_path.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({'cases': len(cases), 'frozen_revision': revision, 'status': report['status']}))


if __name__ == '__main__':
    main()
