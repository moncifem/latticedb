#!/usr/bin/env python3
"""Replay retained nightly observations without rerunning any producer or rewriting history."""
import argparse
import hashlib
import json
import re
import stat
import subprocess
import zipfile
from pathlib import Path, PurePosixPath
from types import SimpleNamespace

import scorecard
from scorecard_metrics import require


def positive(value):
    if not re.fullmatch(r'[1-9][0-9]*', value):
        raise argparse.ArgumentTypeError('source run must be a positive numeric ID')
    return value


def validate_source(run, workflow, jobs, artifacts, repository, branch):
    require(workflow.get('path') == run.get('path') == '.github/workflows/nightly.yml'
            and run.get('workflow_id') == workflow.get('id')
            and run.get('repository', {}).get('full_name') == repository
            and run.get('head_repository', {}).get('full_name') == repository
            and run.get('head_branch') == branch and run.get('status') == 'completed'
            and run.get('event') in ('schedule', 'workflow_dispatch')
            and re.fullmatch(r'[a-f0-9]{40}', str(run.get('head_sha', ''))), 'untrusted/incomplete nightly source')
    # Existing per-producer artifacts do not contain attempt manifests. Attempt one is unambiguous;
    # reruns require a richer producer manifest rather than assigning old artifacts a new attempt.
    require(run.get('run_attempt') == 1, 'source artifact attempt is ambiguous after a rerun')
    require(jobs and len({job['id'] for job in jobs}) == len(jobs), 'empty/duplicate source jobs')
    for job in jobs:
        require(job.get('run_id') == run['id'] and job.get('run_attempt') == run['run_attempt']
                and job.get('head_sha') == run['head_sha'] and job.get('status') == 'completed', 'source job provenance mismatch')
    expected = {}
    for job in jobs:
        sim = re.fullmatch(r'sim \(([^,]+), (\d+)\)', job['name'])
        fuzz = re.fullmatch(r'fuzz \(([^)]+)\)', job['name'])
        name = ('evidence-sim-' + '-'.join(sim.groups()) if sim else
                'evidence-fuzz-' + fuzz[1] if fuzz else 'evidence-bench' if job['name'] == 'bench' else None)
        if name:
            require(name not in expected, 'duplicate artifact producer')
            expected[name] = job
    selected = [a for a in artifacts if a['name'].startswith('evidence-')]
    require(len(selected) == len(expected) and {a['name'] for a in selected} == set(expected),
            'source artifacts do not cover exactly the declared producer jobs')
    for artifact in selected:
        origin = artifact.get('workflow_run', {})
        require(artifact.get('expired') is False and origin.get('id') == run['id']
                and origin.get('head_sha') == run['head_sha'] and origin.get('head_branch') == branch
                and origin.get('repository_id') == run['repository']['id']
                and origin.get('head_repository_id') == run['head_repository']['id']
                and re.fullmatch(r'sha256:[a-f0-9]{64}', str(artifact.get('digest', '')))
                and 0 < artifact.get('size_in_bytes', 0) <= scorecard.MAX_JSON, 'source artifact provenance/bounds mismatch')
    return selected


def unpack(path, artifact, output):
    require(hashlib.sha256(path.read_bytes()).hexdigest() == artifact['digest'].removeprefix('sha256:'), 'artifact archive digest mismatch')
    with zipfile.ZipFile(path) as archive:
        items = archive.infolist()
        require(len(items) <= 1000 and sum(item.file_size for item in items) <= scorecard.MAX_JSON,
                'artifact expansion exceeds bounds')
        require(len({item.filename for item in items}) == len(items), 'duplicate artifact archive paths')
        for item in items:
            name = PurePosixPath(item.filename)
            require(not name.is_absolute() and '..' not in name.parts
                    and not stat.S_ISLNK(item.external_attr >> 16), 'unsafe artifact archive path')
            if not (re.fullmatch(r'ledger/[0-9]{20}\.seg', item.filename) or item.filename == 'fuzz.log'):
                continue
            destination = output / item.filename
            destination.parent.mkdir(parents=True, exist_ok=True)
            with archive.open(item) as stream:
                raw = stream.read(scorecard.MAX_JSON + 1)
            require(len(raw) == item.file_size and len(raw) <= scorecard.MAX_JSON, 'artifact entry length mismatch')
            destination.write_bytes(raw)


def fetch_json(endpoint, output, paginate=False):
    command = ['gh', 'api'] + (['--paginate', '--slurp'] if paginate else []) + [endpoint]
    with output.open('wb') as stream:
        subprocess.run(command, stdout=stream, check=True)
    return scorecard.load(output)


def replay(args):
    require(re.fullmatch(r'[\w.-]+/[\w.-]+', args.repository), 'invalid repository')
    require(re.fullmatch(r'[a-f0-9]{40}', args.code_revision), 'invalid replay code revision')
    head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=scorecard.ROOT, text=True).strip()
    require(head == args.code_revision, 'replay code revision does not match checkout')
    dirty = subprocess.check_output(['git', 'status', '--porcelain'], cwd=scorecard.ROOT, text=True).strip()
    require(not dirty, 'replay requires a clean committed checkout')
    output = Path(args.output)
    require(not output.exists(), 'replay output must be new')
    output.mkdir(parents=True)
    repo = f'repos/{args.repository}'
    run = fetch_json(f'{repo}/actions/runs/{args.source_run}', output / 'source-run.json')
    workflow = fetch_json(f'{repo}/actions/workflows/nightly.yml', output / 'source-workflow.json')
    require(str(run['id']) == args.source_run, 'source run ID mismatch')
    # Exact same supported two-stage pagination used by normal nightly: raw gh pages, then jq.
    fetch_json(f'{repo}/actions/runs/{args.source_run}/attempts/{run["run_attempt"]}/jobs?per_page=100',
               output / 'job-pages.json', paginate=True)
    with (output / 'jobs.json').open('wb') as stream:
        subprocess.run(['jq', '{jobs: [.[].jobs[]]}', str(output / 'job-pages.json')], stdout=stream, check=True)
    jobs = scorecard.load(output / 'jobs.json')['jobs']
    pages = fetch_json(f'{repo}/actions/runs/{args.source_run}/artifacts?per_page=100', output / 'artifact-pages.json', paginate=True)
    artifacts = [artifact for page in pages for artifact in page['artifacts']]
    selected = validate_source(run, workflow, jobs, artifacts, args.repository, args.branch)
    archive_dir = output / 'archives'
    archive_dir.mkdir()
    observations = output / 'observations'
    for artifact in selected:
        path = archive_dir / f'{artifact["id"]}.zip'
        with path.open('wb') as stream:
            subprocess.run(['gh', 'api', f'{repo}/actions/artifacts/{artifact["id"]}/zip'], stdout=stream, check=True)
        require(path.stat().st_size <= scorecard.MAX_JSON, 'artifact download exceeds bounds')
        unpack(path, artifact, observations / artifact['name'])
    ledger = output / 'ledger'
    scorecard.ingest(SimpleNamespace(history=None, history_provenance=None, ledger=ledger, lattice=args.lattice,
        jobs=output / 'jobs.json', artifacts=observations, repository=args.repository,
        run=str(run['id']), attempt=str(run['run_attempt']), revision=run['head_sha']))
    entries = scorecard.export_ledger(ledger, args.lattice)
    # Re-ingestion of the same source must be idempotent.
    scorecard.ingest(SimpleNamespace(history=None, history_provenance=None, ledger=ledger, lattice=args.lattice,
        jobs=output / 'jobs.json', artifacts=observations, repository=args.repository,
        run=str(run['id']), attempt=str(run['run_attempt']), revision=run['head_sha']))
    require(entries == scorecard.export_ledger(ledger, args.lattice), 'replay ingestion was not idempotent')
    value = scorecard.report(entries)
    require(not value['invalid_evidence'], 'invalid replay evidence')
    check = scorecard.cli(args.lattice, 'ledger', 'check', '--dir', str(ledger))
    (output / 'thresholds.txt').write_text(check)
    provenance = {'source_run': run, 'source_artifacts': selected, 'replay_code_revision': args.code_revision,
                  'ledger_cli_sha256': hashlib.sha256(Path(args.lattice).read_bytes()).hexdigest(),
                  'replay_worktree_clean': True,
                  'scope': 'Existing producer observations replayed; original workflow conclusion preserved. No producers rerun. No phase signoff.'}
    (output / 'replay-provenance.json').write_text(json.dumps(provenance, indent=2) + '\n')
    (output / 'scorecard.json').write_text(json.dumps(value, indent=2) + '\n')
    (output / 'scorecard.md').write_text(scorecard.markdown(value))
    print(json.dumps({'source_run': run['id'], 'source_attempt': run['run_attempt'], 'source_revision': run['head_sha'],
                      'source_conclusion': run['conclusion'], 'replay_code_revision': args.code_revision,
                      'ledger_entries': len(entries), 'simulation_runs': value['observed_totals']['simulation_runs'],
                      'simulation_violations': value['observed_totals']['simulation_violations'],
                      'simulation_incomplete': value['observed_totals']['simulation_incomplete'],
                      'completed_fuzz_seconds_by_target': value['observed_totals']['completed_fuzz_seconds_by_target'],
                      'invalid_evidence': value['invalid_evidence']}, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source-run', required=True, type=positive)
    for name in ('repository', 'branch', 'lattice', 'output', 'code-revision'):
        parser.add_argument('--' + name, required=True)
    replay(parser.parse_args())


if __name__ == '__main__':
    main()
