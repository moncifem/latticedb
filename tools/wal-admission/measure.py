#!/usr/bin/env python3
"""Bounded hosted Linux recovery measurements; no performance acceptance threshold."""
from pathlib import Path
import hashlib
import json
import os
import platform
import statistics
import struct
import subprocess
import tempfile

from make_fixtures import crc, header, name, record

ROOT = Path(__file__).resolve().parents[2]
FROZEN = '743b0fb52a495a3f875490268d604ed543be9e64'
HARNESS = 'crates/lattice-bench/examples/admission_measure.rs'


def sha(data):
    return hashlib.sha256(data).hexdigest()


def manifest(root):
    return {str(p.relative_to(root)): sha(p.read_bytes())
            for p in sorted(root.rglob('*')) if p.is_file()}


def output(args):
    return subprocess.check_output(args, text=True).strip()


def provenance(root, extra):
    for args in [('diff', '--exit-code'), ('diff', '--cached', '--exit-code', 'HEAD')]:
        subprocess.run(['git', '-C', str(root), *args], check=True, capture_output=True)
    untracked = output(['git', '-C', str(root), 'ls-files', '--others', '--exclude-standard']).splitlines()
    if sorted(untracked) != sorted(extra):
        raise SystemExit(f'unexpected source additions: {untracked}')
    return {'revision': output(['git', '-C', str(root), 'rev-parse', 'HEAD']),
            'tracked_source_clean': True, 'allowed_untracked': untracked,
            'files_sha256': {p: sha((root / p).read_bytes())
                             for p in ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', *extra]}}


def dataset(root, segments, value_size, guarded):
    wal = root / 'wal'
    wal.mkdir(parents=True)
    for lsn in range(1, segments + 1):
        # Literal LROW insert, valid ChangeSet with strictly increasing id and HLC.
        rows = b'LROW\x01' + struct.pack('<II', 1, 7) + name(b'key') + b'\x01' + name(b'x' * value_size)
        cs = (b'\x01' + lsn.to_bytes(16, 'big') + struct.pack('<QII', lsn, 0, 1)
              + bytes(16) + b'\x00\x00\x00' + name(rows))
        segment = (header() if guarded else b'LATLOG01' + struct.pack('<II', 1, 0)) + record(lsn, 1, cs)
        (wal / f'{lsn:020}.seg').write_bytes(segment)
    (wal / 'LOCK').write_bytes(b'')
    return manifest(root)


def run(binary, root, temp):
    # Explicit warm-cache sample: no root privileges, drop_caches or cold-cache claim.
    for p in sorted((root / 'wal').glob('*.seg')):
        p.read_bytes()
    metrics = temp / 'time.txt'
    args = ['/usr/bin/time', '-v', '-o', str(metrics), str(binary), str(root)]
    result = subprocess.run(args, text=True, capture_output=True, check=True)
    timing = metrics.read_text()
    peak = next(int(line.split(':', 1)[1]) for line in timing.splitlines()
                if 'Maximum resident set size (kbytes)' in line)
    return {'argv': args, 'stdout': result.stdout, 'stderr': result.stderr,
            'exit_code': result.returncode, 'gnu_time_v': timing,
            'peak_rss_kib': peak, 'reader': json.loads(result.stdout)}


def main():
    old = ROOT / 'target/frozen-old/release/examples/admission_measure'
    new = ROOT / 'target/release/examples/admission_measure'
    old_source = provenance(ROOT / 'frozen', [HARNESS])
    if old_source['revision'] != FROZEN:
        raise SystemExit('wrong frozen revision')
    # Build output/evidence files are intentionally untracked at the main root: only
    # tracked-source cleanliness is asserted there, every reviewed tool is hashed.
    subprocess.run(['git', 'diff', '--exit-code'], cwd=ROOT, check=True)
    subprocess.run(['git', 'diff', '--cached', '--exit-code', 'HEAD'], cwd=ROOT, check=True)
    report = {'schema': 1, 'frozen_source': old_source,
              'new_source': output(['git', 'rev-parse', 'HEAD']),
              'github_context': {key: os.environ.get(key) for key in ['GITHUB_REPOSITORY', 'GITHUB_SHA', 'GITHUB_RUN_ID', 'GITHUB_RUN_ATTEMPT', 'GITHUB_REF']},
              'new_tracked_source_clean': True,
              'tool_sha256': {str(p.relative_to(ROOT)): sha(p.read_bytes())
                              for p in sorted((ROOT / 'tools/wal-admission').glob('*')) if p.is_file()},
              'new_inputs_sha256': {p: sha((ROOT / p).read_bytes()) for p in [HARNESS, 'Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml']},
              'binaries_sha256': {'frozen': sha(old.read_bytes()), 'new': sha(new.read_bytes())},
              'rustc': output(['rustc', '-Vv']), 'cargo': output(['cargo', '-V']),
              'time_version': output(['/usr/bin/time', '--version']),
              'platform': platform.platform(), 'cpu': Path('/proc/cpuinfo').read_text(),
              'memory': Path('/proc/meminfo').read_text(),
              'build': 'cargo build --locked --release -p lattice-bench --example admission_measure; old uses --manifest-path frozen/Cargo.toml --target-dir target/frozen-old',
              'environment': {key: os.environ.get(key) for key in ['RUSTFLAGS', 'CARGO_BUILD_JOBS', 'CARGO_PROFILE_RELEASE_LTO']},
              'cache_context': 'All segment files read to OS page cache immediately before each sample. No cold-cache claim. One recorded warmup per mode (excluded from medians), then five timed samples; alternating frozen/new order per repetition.',
              'scope': 'Streaming public Log open only; one valid ChangeSet per segment. GNU time whole-process peak RSS, not allocator accounting, not a full recovery memory bound. No regression threshold or gate sign-off.',
              'cases': []}
    with tempfile.TemporaryDirectory(prefix='wal-admission-measure-') as tmp:
        temp = Path(tmp)
        for segments in [1, 32, 256]:
            for size in [64, 65536]:
                base = temp / f'{segments}-{size}'
                legacy, guarded = base / 'legacy', base / 'guarded'
                before = dataset(legacy, segments, size, False)
                guarded_before = dataset(guarded, segments, size, True)
                case = {'segments': segments, 'value_bytes_per_record': size,
                        'legacy_manifest': before, 'guarded_manifest': guarded_before,
                        'legacy_total_bytes': sum(p.stat().st_size for p in (legacy / 'wal').glob('*.seg')),
                        'guarded_total_bytes': sum(p.stat().st_size for p in (guarded / 'wal').glob('*.seg')),
                        'warmups': {}, 'samples': {'frozen_legacy': [], 'new_legacy': [], 'new_guarded': []}}
                modes = [('frozen_legacy', old, legacy), ('new_legacy', new, legacy), ('new_guarded', new, guarded)]
                for mode, binary, root in modes:
                    case['warmups'][mode] = run(binary, root, temp)
                for repeat in range(5):
                    for mode, binary, root in modes if repeat % 2 == 0 else reversed(modes):
                        sample = run(binary, root, temp)
                        m = sample['reader']
                        if (m['records'] != segments or m['segments'] != segments or m['full_reads'] != segments
                                or m['payload_bytes'] != segments * (81 + size)
                                or m['full_bytes'] != case['guarded_total_bytes' if mode == 'new_guarded' else 'legacy_total_bytes']):
                            raise SystemExit(f'wrong recovery/control counts: {mode} {m}')
                        expected_prefixes = 0 if mode == 'frozen_legacy' else segments
                        if m['prefix_reads'] != expected_prefixes:
                            raise SystemExit('unexpected prefix read count')
                        if max(m['max_prefix_request'], m['max_prefix_return'], m['max_prefix_capacity']) > 32:
                            raise SystemExit('shipped prefix buffer exceeded 32 bytes')
                        if m['prefix_bytes'] != 32 * expected_prefixes:
                            raise SystemExit('unexpected returned prefix bytes')
                        sample['repeat'] = repeat
                        case['samples'][mode].append(sample)
                if manifest(legacy) != before or manifest(guarded) != guarded_before:
                    raise SystemExit('clean dataset changed during recovery measurement')
                case['median'] = {mode: {'open_ns': statistics.median(s['reader']['open_ns'] for s in samples),
                                         'peak_rss_kib': statistics.median(s['peak_rss_kib'] for s in samples)}
                                  for mode, samples in case['samples'].items()}
                case['legacy_overhead'] = {
                    'median_open_ns_difference': case['median']['new_legacy']['open_ns'] - case['median']['frozen_legacy']['open_ns'],
                    'median_peak_rss_kib_difference': case['median']['new_legacy']['peak_rss_kib'] - case['median']['frozen_legacy']['peak_rss_kib']}
                report['cases'].append(case)
    (ROOT / 'wal-admission-measurement.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({str((c['segments'], c['value_bytes_per_record'])): c['median'] for c in report['cases']}, indent=2))


if __name__ == '__main__':
    main()
