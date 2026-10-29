#!/usr/bin/env python3
"""Manual same-host foundation diagnostic; never evaluates ADR 0013 C4."""
import argparse
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import statistics
import struct
import subprocess
import sys

BASELINE = '96d409f7ecfc8b36418edcf1ceb2478030f68236'
CANDIDATE = '45926c7fc1f6fa71a7a69ac858c805ab1d9679b0'
WORKLOADS = {'commit_batch_of_16_cpu_per_item', 'commit_single_cpu', 'hlc_tick',
             'lsl_parse_and_check', 'orset_merge_500', 'record_encode_decode_100b',
             'recovery_open_per_record'}
SCALE = 5
PAIRS = 9
MAX_LEDGER = 1024 * 1024


def require(condition, why):
    if not condition:
        raise ValueError(why)


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def save(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True, allow_nan=False) + '\n')


def command(args, cwd):
    return subprocess.check_output(args, cwd=cwd, text=True).strip()


def source_identity(path, expected):
    path = Path(path).resolve()
    require(command(['git', 'rev-parse', 'HEAD'], path) == expected, 'source revision differs')
    require(not command(['git', 'status', '--porcelain', '--untracked-files=all'], path), 'source is dirty')
    names = subprocess.check_output(['git', 'ls-files', '-z'], cwd=path).split(b'\0')
    files = {name.decode(): digest(path / name.decode()) for name in names if name}
    require('Cargo.lock' in files and 'rust-toolchain.toml' in files, 'missing pinned inputs')
    return {'revision': expected, 'tracked_sha256': hashlib.sha256(
        json.dumps(files, sort_keys=True, separators=(',', ':')).encode()).hexdigest(),
        'tracked_files': files, 'lock_sha256': files['Cargo.lock'],
        'toolchain_sha256': files['rust-toolchain.toml']}


def crc32c(data):
    crc = 0xffffffff
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0x82f63b78 if crc & 1 else 0)
    return crc ^ 0xffffffff


def read_measurement(root):
    """Independently read the single fresh legacy WAL/ChangeSet ledger, checking CRC and framing."""
    entries = sorted((Path(root) / 'ledger').glob('*.seg'))
    require(len(entries) == 1, 'expected exactly one ledger segment')
    require(entries[0].stat().st_size <= MAX_LEDGER, 'ledger too large')
    raw = entries[0].read_bytes()
    require(raw[:16] == b'LATLOG01' + struct.pack('<II', 1, 0), 'unexpected ledger header')
    require(len(raw) >= 33, 'truncated WAL record')
    length, crc, lsn, kind = struct.unpack_from('<IIQB', raw, 16)
    require(lsn == 1 and kind == 1 and len(raw) == 33 + length, 'record framing/count differs')
    body = raw[33:]
    require(crc32c(raw[16:20] + raw[24:33] + body) == crc, 'ledger CRC differs')
    require(len(body) >= 56 and body[0] == 1 and body[49] == 0, 'unexpected ChangeSet envelope')
    require(struct.unpack_from('<H', body, 50)[0] == 0, 'unexpected parents')
    size = struct.unpack_from('<I', body, 52)[0]
    require(len(body) == 56 + size and body[56:62] == b'bench\n', 'ledger payload framing differs')
    value = json.loads(body[62:].decode())
    require(value.get('profile') == 'ci-ubuntu' and type(value.get('scale')) is int
            and value['scale'] == SCALE, 'sample profile/scale differs')
    timings = value.get('ns_per_op')
    require(isinstance(timings, dict) and set(timings) == WORKLOADS, 'sample workload coverage differs')
    require(all(type(x) in (int, float) and math.isfinite(x) and x > 0 for x in timings.values()),
            'sample has invalid timings')
    return value


def order(pair):
    return ['baseline', 'candidate'] if pair % 2 == 0 else ['candidate', 'baseline']


def distribution(values):
    median = statistics.median(values)
    return {'min': min(values), 'median': median, 'max': max(values),
            'mad': statistics.median(abs(x - median) for x in values),
            'sample_stdev': statistics.stdev(values) if len(values) > 1 else 0}


def summarize(receipts):
    expected = {(pair, arm) for pair in range(PAIRS) for arm in ('baseline', 'candidate')}
    observed = [(row.get('pair'), row.get('arm')) for row in receipts]
    errors = [row for row in receipts if row.get('exit_code') != 0 or row.get('error')]
    complete = len(observed) == len(set(observed)) and set(observed) == expected and not errors
    result = {'schema': 1, 'diagnostic_only': True, 'adr_c4_status': 'not_evaluated',
              'reason': 'Foundation microbenchmarks cannot establish a historical SQL baseline.',
              'complete': complete, 'expected_samples': 2 * PAIRS,
              'observed_samples': len(receipts), 'failed_samples': errors, 'workloads': None}
    if complete:
        by_key = {(r['pair'], r['arm']): r['measurement']['ns_per_op'] for r in receipts}
        require(all(set(v) == WORKLOADS and all(type(x) in (int, float) and math.isfinite(x) and x > 0
                                              for x in v.values()) for v in by_key.values()),
                'summary coverage/timing differs')
        result['workloads'] = {name: {
            'baseline_ns': distribution([by_key[p, 'baseline'][name] for p in range(PAIRS)]),
            'candidate_ns': distribution([by_key[p, 'candidate'][name] for p in range(PAIRS)]),
            'paired_candidate_over_baseline': distribution([
                by_key[p, 'candidate'][name] / by_key[p, 'baseline'][name] for p in range(PAIRS)])
        } for name in sorted(WORKLOADS)}
    return result


def sample(binary, source, directory, pair, arm, timeout=900):
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=False)
    args = [str(binary), '--profile', 'ci-ubuntu', '--scale', str(SCALE),
            '--tolerance', '2.0', '--ledger', str(directory / 'raw')]
    receipt = {'pair': pair, 'arm': arm, 'arguments': args, 'cwd': str(source),
               'binary_sha256': digest(binary), 'started_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
               'exit_code': None}
    save(directory / 'receipt.json', receipt)
    try:
        with (directory / 'stdout.log').open('wb') as stdout, (directory / 'stderr.log').open('wb') as stderr:
            process = subprocess.run(args, cwd=source, stdout=stdout, stderr=stderr, timeout=timeout, check=False)
        receipt['exit_code'] = process.returncode
        receipt['measurement'] = read_measurement(directory / 'raw')
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        receipt['error'] = str(error)
    receipt['finished_utc'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    receipt['files_sha256'] = {str(p.relative_to(directory)): digest(p)
                             for p in sorted(directory.rglob('*')) if p.is_file() and p.name != 'receipt.json'}
    save(directory / 'receipt.json', receipt)
    return receipt


def run(args):
    root = Path(args.output).resolve()
    root.mkdir(parents=True, exist_ok=False)
    sources = {'baseline': Path(args.baseline).resolve(), 'candidate': Path(args.candidate).resolve()}
    identities = {arm: source_identity(path, BASELINE if arm == 'baseline' else CANDIDATE)
                  for arm, path in sources.items()}
    require(identities['baseline']['toolchain_sha256'] == identities['candidate']['toolchain_sha256'],
            'toolchain source bytes differ')
    metadata = {'schema': 1, 'baseline': BASELINE, 'candidate': CANDIDATE, 'scale': SCALE, 'pairs': PAIRS,
                'warmups_per_binary': 1, 'sources': identities, 'platform': platform.platform(),
                'cpu': Path('/proc/cpuinfo').read_text() if Path('/proc/cpuinfo').exists() else platform.processor(),
                'run_id': os.environ.get('GITHUB_RUN_ID'), 'attempt': os.environ.get('GITHUB_RUN_ATTEMPT'),
                'controller_revision': os.environ.get('GITHUB_SHA'),
                'rustc': command(['rustc', '-Vv'], sources['candidate']),
                'cargo': command(['cargo', '-V'], sources['candidate']),
                'build': 'cargo build --locked --release -p lattice-bench --bin bench_runner; default features; original release profile',
                'sample_order': [order(p) for p in range(PAIRS)],
                'scope': 'seven foundation CPU/SimIo workloads; no SQL or raw RowStore comparison'}
    save(root / 'metadata.json', metadata)
    binaries = {}
    for arm, path in sources.items():
        target = root / 'build' / arm
        env = dict(os.environ, CARGO_TARGET_DIR=str(target), CARGO_BUILD_JOBS='2', CARGO_INCREMENTAL='0',
                   RUSTFLAGS='', CARGO_ENCODED_RUSTFLAGS='')
        # Each checkout is built unmodified, with its own original lockfile and pinned toolchain.
        argv = ['cargo', 'build', '--locked', '--release', '-p', 'lattice-bench', '--bin', 'bench_runner']
        with (root / f'build-{arm}.log').open('wb') as log:
            build = subprocess.run(argv, cwd=path, env=env, stdout=log, stderr=subprocess.STDOUT,
                                   timeout=1800, check=False)
        save(root / f'build-{arm}.json', {'arguments': argv, 'exit_code': build.returncode,
             'env': {key: env[key] for key in ('CARGO_TARGET_DIR', 'CARGO_BUILD_JOBS', 'CARGO_INCREMENTAL',
                                              'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS')}})
        require(build.returncode == 0, f'{arm} build failed; raw log retained')
        require(source_identity(path, identities[arm]['revision']) == identities[arm], 'build changed tracked source')
        binaries[arm] = target / 'release' / 'bench_runner'
    warmups = [sample(binaries[a], sources[a], root / f'warmup-{a}', -1, a) for a in ('baseline', 'candidate')]
    receipts = []
    for pair in range(PAIRS):
        for arm in order(pair):
            receipts.append(sample(binaries[arm], sources[arm], root / f'pair-{pair}-{arm}', pair, arm))
    summary = summarize(receipts)
    summary['warmups'] = warmups
    summary['source_unchanged_after_samples'] = all(
        source_identity(path, identities[arm]['revision']) == identities[arm] for arm, path in sources.items())
    summary['complete'] = summary['complete'] and all(r['exit_code'] == 0 and not r.get('error') for r in warmups)
    save(root / 'summary.json', summary)
    return 0 if summary['complete'] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', required=True)
    parser.add_argument('--candidate', required=True)
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    try:
        return run(args)
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        output = Path(args.output)
        if output.is_dir():
            save(output / 'failure.json', {'error': str(error), 'adr_c4_status': 'not_evaluated'})
        print(str(error), file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
