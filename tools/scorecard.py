#!/usr/bin/env python3
"""Observed scorecard evidence, using Lattice's unchanged opaque ledger payloads.

Only Python's standard library is required. A report never signs a phase gate.
"""
import argparse
import datetime
import hashlib
import json
import re
import shutil
import subprocess
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MAX_JSON = 64 * 1024 * 1024
MAX_RESULT = 64 * 1024


def load(path):
    with Path(path).open('rb') as stream:
        raw = stream.read(MAX_JSON + 1)
    if len(raw) > MAX_JSON:
        raise ValueError(f'{path}: evidence exceeds 64 MiB')
    return json.loads(raw)


def cli(binary, *args):
    command = [binary] if binary else ['cargo', 'run', '-q', '-p', 'lattice-cli', '--']
    return subprocess.check_output(command + list(args), cwd=ROOT, text=True)


def export_ledger(directory, binary=None):
    """Read a regular-file snapshot, rejecting recovery repairs; never alter source bytes.

    This is not an atomic snapshot of a concurrent writer. Ingestion owns its ledger exclusively.
    """
    source = Path(directory) / 'ledger'
    if not source.exists():
        return []
    if source.is_symlink() or not source.is_dir():
        raise ValueError('ledger directory must be a real directory')
    with tempfile.TemporaryDirectory(prefix='lattice-scorecard-') as tmp:
        dest = Path(tmp) / 'ledger'
        dest.mkdir()
        before = {}
        for segment in sorted(source.glob('*.seg')):
            if not re.fullmatch(r'\d{20}\.seg', segment.name) or segment.is_symlink() or not segment.is_file():
                raise ValueError('ledger segments must be regular files with numeric names')
            shutil.copyfile(segment, dest / segment.name)
            before[segment.name] = hashlib.sha256((dest / segment.name).read_bytes()).hexdigest()
        if not before:
            return []
        entries = json.loads(cli(binary, 'ledger', 'export', '--dir', tmp))
        after = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in dest.glob('*.seg')}
        if before != after:
            raise ValueError('ledger snapshot required repair; source retained, evidence rejected')
        return entries


def normalized(entries):
    """Preserve old entries; dedupe sourced evidence by stable producer/run/attempt/job/record key."""
    result, seen, invalid = [], {}, []
    if not isinstance(entries, list):
        raise ValueError('export must contain an array')
    for entry in entries:
        try:
            kind = entry['kind']
            value = json.loads(entry['json'])
            if not isinstance(kind, str) or not isinstance(value, dict):
                raise ValueError('expected kind and JSON object')
            source = value.get('source')
            if source is not None:
                if not isinstance(source, dict) or not isinstance(source.get('key'), str) or not source['key']:
                    raise ValueError('invalid source key')
                key = (kind, source['key'])
                if key in seen:
                    if seen[key] != value:
                        raise ValueError('conflicting duplicate source key')
                    continue
                seen[key] = value
            result.append({'kind': kind, 'value': value, 'recorded_ms': entry.get('recorded_ms')})
        except (ValueError, TypeError, KeyError) as error:
            invalid.append(str(error))
    return result, invalid


def count(value):
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def completed_fuzz(log, conclusion):
    # A configured budget, timeout, cancelled job or partial progress line proves no completed budget.
    if conclusion != 'success':
        return None
    matches = re.findall(r'^Done (\d+) runs in (\d+) second\(s\)', log, re.MULTILINE)
    if len(matches) != 1:
        return None
    runs, seconds = map(int, matches[0])
    return {'runs': runs, 'seconds': seconds} if runs > 0 and seconds > 0 else None


def section(observations, missing):
    return {'status': 'observed' if observations else 'missing',
            'observations': observations, 'missing': missing}


def report(entries, root=ROOT):
    rows, invalid = normalized(entries)
    by = lambda kind: [e['value'] for e in rows if e['kind'] == kind]
    jobs = by('ci-job')
    sims = by('sim')
    valid_sims = [s for s in sims if all(count(s.get(k)) for k in ('runs', 'failing', 'incomplete'))
                  and s['failing'] <= s['runs'] and s['incomplete'] <= s['runs']]
    invalid += ['invalid simulation counts'] * (len(sims) - len(valid_sims))
    fuzz = [f for f in by('fuzz') if f.get('conclusion') == 'success'
            and count(f.get('seconds')) and f['seconds'] > 0 and count(f.get('runs')) and f['runs'] > 0]
    invalid += ['invalid or incomplete fuzz observation'] * (len(by('fuzz')) - len(fuzz))
    benches = by('bench')
    import scorecard_metrics
    metrics = {'mutation': [], 'eval': []}
    for kind in metrics:
        for observation in by(kind):
            try:
                metrics[kind].append(scorecard_metrics.validate_record(kind, observation))
                if kind == 'mutation' and observation.get('campaign_proof', {}).get('invalid_shards'):
                    invalid.append('invalid mutation shards: ' + '; '.join(observation['campaign_proof']['invalid_shards']))
            except (ValueError, TypeError, KeyError) as error:
                invalid.append(f'invalid {kind} observation: {error}')
    pending, named = [], {f'I{i}': [] for i in range(1, 15)}
    # This inventory is deliberately source-only; comments and test names cannot prove execution.
    for path in sorted((root / 'crates').rglob('*.rs')):
        text = path.read_text()
        pending += [{'file': str(path.relative_to(root)), 'reason': m}
                    for m in re.findall(r'#\[ignore\s*=\s*"(phase [^"]+)"\]', text)]
        for n in re.findall(r'\bfn inv_i(\d+)_', text):
            if f'I{n}' in named:
                named[f'I{n}'].append(str(path.relative_to(root)))
    direction = root / 'docs/spec/direction.md'
    risk_rows = [line.strip() for line in direction.read_text().splitlines()
                 if re.match(r'\|\s*R\d+\s*\|', line)] if direction.exists() else []
    fuzz_seconds = {}
    for observation in fuzz:
        target = observation.get('target', 'unspecified')
        fuzz_seconds[target] = fuzz_seconds.get(target, 0) + observation['seconds']
    durations = []
    for job in jobs:
        if job.get('status') == 'completed' and job.get('started_at') and job.get('completed_at'):
            start = datetime.datetime.fromisoformat(job['started_at'].replace('Z', '+00:00'))
            end = datetime.datetime.fromisoformat(job['completed_at'].replace('Z', '+00:00'))
            durations.append({'name': job.get('name'), 'seconds': (end - start).total_seconds(),
                              'source': job.get('source')})
    return {
        'schema': 1,
        'observed_totals': {'simulation_runs': sum(s['runs'] for s in valid_sims),
                            'simulation_violations': sum(s['failing'] for s in valid_sims),
                            'simulation_incomplete': sum(s['incomplete'] for s in valid_sims),
                            'completed_fuzz_seconds_by_target': fuzz_seconds,
                            'completed_job_durations': durations},
        'review_status': 'unsigned; P1-77 coverage incomplete',
        'history_scope': 'Retained artifact/local ledger observations only; gaps and expired artifacts are unknown. '
                         'Totals are observed lower bounds, not lifetime budgets. Static inventories are not pass evidence. Retained records are workflow producer observations, not independent attestations.',
        'provenance': [j.get('source') for j in jobs],
        'retained_artifact_origins': by('history-origin'),
        'invalid_evidence': invalid,
        'sections': {
            'engineering_health': section(jobs, ['PR gate duration unless ingested from PR runs',
                'flaky tests, currently open nightly failures and time to fix require issue/quarantine history']),
            'invariant_coverage': section([{'declared_source_names': named, 'execution_status': 'unverified'}],
                ['I1-I14 enforced/verified/monitored classifications need reviewed execution evidence']),
            'simulation': section(valid_sims, ['planted-bug detection rate requires a measured detection report',
                'retained shard results do not prove all nightly shards completed']),
            'fuzz': section(fuzz, ['only successful libFuzzer completion summaries count; missing/cancelled logs do not',
                'lifetime cumulative hours are unknown outside retained observations']),
            'mutation': section(metrics['mutation'], ['only the reported planned universe/observed outcomes are covered; unrun, timeout and unviable counts are not killed mutants']),
            'eval_gates': section(metrics['eval'], ['legacy reports without exact thresholds/confidence/tolerance remain unverified; suites without actual reports remain missing']),
            'benchmarks': section(benches, ['legacy records lacking verdict are measurements only',
                'ns/op microbenchmarks do not establish p50/p99 commit latency or product throughput']),
            'risk_register': section([{'declared_rows': risk_rows, 'mitigation_status': 'unverified'}] if risk_rows else [],
                ['reviewed severity counts and overdue mitigation dates are missing']),
            'north_star_readiness': section([{'pending_specs': pending, 'passing_of_8': None}],
                ['NS1-NS8 executable acceptance results and reviewed requirements done per phase are missing']),
            'dogfood': section([{'retained_results_stored_in_lattice': len(rows)}] if rows else [],
                ['share of project work in Lattice and history question latency are unmeasured']),
        },
    }


def markdown(value):
    lines = ['# Project scorecard', '', value['review_status'], '', value['history_scope'], '']
    context = value.get('run_context')
    if context:
        original, replay = context['original_run'], context['replay']
        lines += [f'Original workflow outcome: **{original["conclusion"].upper()}** '
                  f'([run {original["id"]}]({original["url"]})).', '',
                  f'Replay outcome: **{replay["conclusion"].upper()}**. '
                  'Evidence collection only; the original workflow outcome is unchanged.', '']
    if value['invalid_evidence']:
        lines += ['Invalid evidence: ' + '; '.join(value['invalid_evidence']), '']
    lines += ['Observed retained totals:', '', '```json', json.dumps(value['observed_totals'], indent=2), '```', '']
    for name, data in value['sections'].items():
        lines += ['## ' + name.replace('_', ' '), '', 'Status: ' + data['status'], '']
        for observation in data['observations']:
            lines += ['```json', json.dumps(observation, sort_keys=True, indent=2), '```', '']
        lines += ['Missing: ' + '; '.join(data['missing']), '']
    return '\n'.join(lines)


def record(binary, directory, kind, value):
    raw = json.dumps(value, separators=(',', ':'), allow_nan=False)
    if len(raw.encode()) > MAX_RESULT:
        raise ValueError('result exceeds 64 KiB')
    with tempfile.TemporaryDirectory(prefix='lattice-result-') as tmp:
        path = Path(tmp) / 'result.json'
        path.write_text(raw)
        cli(binary, 'ledger', 'record', '--dir', str(directory), '--kind', kind, '--json', str(path))


def ingest(args):
    """Rebuild serialized history from prior JSON plus per-job WALs; one writer only."""
    previous = load(args.history) if args.history and Path(args.history).exists() else []
    current = export_ledger(args.ledger, args.lattice)
    rows, invalid = normalized(previous + current)
    if invalid:
        raise ValueError('; '.join(invalid))
    seen = {(r['kind'], r['value'].get('source', {}).get('key')) for r in normalized(current)[0]}
    for row in rows:
        key = (row['kind'], row['value'].get('source', {}).get('key'))
        if key[1] is None:
            raise ValueError('retained CI history must have stable source keys')
        if key not in seen:
            record(args.lattice, args.ledger, row['kind'], row['value'])
            seen.add(key)
    jobs = load(args.jobs)['jobs']
    if args.history_provenance and Path(args.history_provenance).exists():
        origin = load(args.history_provenance)
        # Describe the authenticated artifact boundary separately from its producer claims.
        collector = next(job['id'] for job in jobs if job['name'] == 'scorecard')
        origin['source'] = {'key': f'{args.repository}/{args.run}/{args.attempt}/{collector}/history',
                            'repository': args.repository, 'run_id': args.run,
                            'attempt': args.attempt, 'job_id': collector, 'revision': args.revision}
        key = ('history-origin', origin['source']['key'])
        if key not in seen:
            record(args.lattice, args.ledger, 'history-origin', origin)
            seen.add(key)
    base = f'{args.repository}/{args.run}/{args.attempt}'
    for job in jobs:
        # Live collection cannot observe its own final outcome. Replay may retain the completed
        # historical collector, whose failure is part of the source run's engineering history.
        if job['name'] == 'scorecard' and not (
                getattr(args, 'include_completed_collector', False) and job['status'] == 'completed'):
            continue
        source = {'key': f'{base}/{job["id"]}', 'repository': args.repository,
                  'run_id': args.run, 'attempt': args.attempt, 'job_id': job['id'],
                  'revision': args.revision, 'url': job['html_url']}
        value = {'source': source, 'name': job['name'], 'status': job['status'],
                 'conclusion': job['conclusion'], 'started_at': job.get('started_at'),
                 'completed_at': job.get('completed_at')}
        key = ('ci-job', source['key'])
        if key not in seen:
            record(args.lattice, args.ledger, 'ci-job', value)
            seen.add(key)
        # Matrix names and artifact names are fixed in nightly.yml, never passed to a shell.
        sim = re.fullmatch(r'sim \(([^,]+), (\d+)\)', job['name'])
        artifact = 'evidence-sim-' + '-'.join(sim.groups()) if sim else 'evidence-bench' if job['name'] == 'bench' else None
        if artifact:
            for entry in export_ledger(Path(args.artifacts) / artifact, args.lattice):
                payload = json.loads(entry['json'])
                payload['source'] = dict(source, key=f'{source["key"]}/{entry["kind"]}/{entry["lsn"]}')
                payload['job_conclusion'] = job['conclusion']
                payload['producer_recorded_ms'] = entry['recorded_ms']
                key = (entry['kind'], payload['source']['key'])
                if key not in seen:
                    record(args.lattice, args.ledger, entry['kind'], payload)
                    seen.add(key)
        match = re.fullmatch(r'fuzz \(([^)]+)\)', job['name'])
        if match:
            path = Path(args.artifacts) / f'evidence-fuzz-{match[1]}' / 'fuzz.log'
            result = completed_fuzz(path.read_text(errors='replace'), job['conclusion']) if path.exists() else None
            if result:
                payload = dict(result, source=source, target=match[1], conclusion='success')
                key = ('fuzz', source['key'])
                if key not in seen:
                    record(args.lattice, args.ledger, 'fuzz', payload)
                    seen.add(key)


def validate_history_origin(previous, current, repository, branch, artifact_id):
    for run in (previous, current):
        if (run.get('repository', {}).get('full_name') != repository
                or run.get('head_repository', {}).get('full_name') != repository
                or run.get('head_branch') != branch
                or run.get('path') != '.github/workflows/nightly.yml'
                or run.get('event') not in ('schedule', 'workflow_dispatch')
                or not re.fullmatch(r'[a-f0-9]{40}', run.get('head_sha', ''))
                or not count(run.get('id')) or not count(run.get('run_attempt'))):
            raise ValueError('history producer repository/ref/workflow/provenance mismatch')
    if (previous['workflow_id'] != current['workflow_id'] or previous['id'] == current['id']
            or previous.get('status') != 'completed'):
        raise ValueError('history producer must be a completed different run of this workflow')
    return {'artifact_id': artifact_id, 'repository': repository, 'branch': branch,
            'run_id': previous['id'], 'attempt': previous['run_attempt'],
            'revision': previous['head_sha'], 'workflow_id': previous['workflow_id'],
            'workflow_path': previous['path'], 'conclusion': previous.get('conclusion'),
            'url': previous['html_url']}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    restore = sub.add_parser('restore-history')
    restore.add_argument('--archive', required=True)
    restore.add_argument('--output', required=True)
    for name in ('run-metadata', 'current-metadata', 'repository', 'branch', 'artifact-id'):
        restore.add_argument('--' + name, required=True)
    display = sub.add_parser('report')
    display.add_argument('--ledger', default='target/ledger')
    display.add_argument('--output')
    display.add_argument('--lattice')
    display.add_argument('--producer')
    collect = sub.add_parser('ingest')
    for name in ('ledger', 'lattice', 'jobs', 'artifacts', 'repository', 'run', 'attempt', 'revision'):
        collect.add_argument('--' + name, required=True)
    collect.add_argument('--history')
    collect.add_argument('--history-provenance')
    args = parser.parse_args()
    if args.command == 'restore-history':
        origin = validate_history_origin(load(args.run_metadata), load(args.current_metadata),
                                         args.repository, args.branch, args.artifact_id)
        with zipfile.ZipFile(args.archive) as archive:
            manifest = archive.getinfo('producer.json')
            if manifest.file_size > MAX_RESULT:
                raise ValueError('producer manifest exceeds 64 KiB')
            with archive.open(manifest) as stream:
                producer = json.loads(stream.read(MAX_RESULT + 1))
            expected = load(args.run_metadata)
            for field in ('id', 'run_attempt', 'head_sha', 'workflow_id', 'path', 'head_branch'):
                if producer.get(field) != expected.get(field):
                    raise ValueError('retained artifact run/attempt/revision manifest mismatch')
            item = archive.getinfo('history.json')
            if item.file_size > MAX_JSON:
                raise ValueError('retained history exceeds 64 MiB')
            # Do not extract arbitrary artifact paths, including symlinks or traversal names.
            with archive.open(item) as stream:
                raw = stream.read(MAX_JSON + 1)
            if len(raw) > MAX_JSON:
                raise ValueError('retained history exceeds 64 MiB')
            if not isinstance(json.loads(raw), list):
                raise ValueError('history must be an array')
            entries = json.loads(raw)
            rows, invalid = normalized(entries)
            if invalid:
                raise ValueError('; '.join(invalid))
            for row in rows:
                source = row['value'].get('source', {})
                base = f'{args.repository}/{source.get("run_id")}/{source.get("attempt")}/{source.get("job_id")}'
                if (source.get('repository') != args.repository
                        or not re.fullmatch(r'[a-f0-9]{40}', source.get('revision', ''))
                        or not str(source.get('run_id', '')).isdigit()
                        or not str(source.get('attempt', '')).isdigit()
                        or not str(source.get('job_id', '')).isdigit()
                        or not (source.get('key') == base or source.get('key', '').startswith(base + '/'))):
                    raise ValueError('retained observation has invalid producer provenance')
            Path(args.output).write_bytes(raw)
            Path(args.output).with_name('history-provenance.json').write_text(json.dumps(origin))
        return
    if args.command == 'ingest':
        ingest(args)
        return
    entries = export_ledger(args.ledger, args.lattice)
    value = report(entries)
    rendered = markdown(value)
    if args.output:
        output = Path(args.output)
        output.mkdir(parents=True, exist_ok=True)
        (output / 'scorecard.json').write_text(json.dumps(value, indent=2) + '\n')
        (output / 'scorecard.md').write_text(rendered)
        (output / 'history.json').write_text(json.dumps(entries, indent=2) + '\n')
        if args.producer:
            producer = load(args.producer)
            manifest = {key: producer[key] for key in ('id', 'run_attempt', 'head_sha', 'workflow_id', 'path', 'head_branch')}
            (output / 'producer.json').write_text(json.dumps(manifest, indent=2) + '\n')
    print(rendered)
    if value['invalid_evidence']:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
