#!/usr/bin/env python3
"""Strict mutation/Evalkit evidence adapters; examples never become automatic project results."""
import argparse
import hashlib
import json
import math
import posixpath
import re
import subprocess
import tempfile
import urllib.parse
import zipfile
from pathlib import Path, PurePosixPath

import scorecard

SUMMARIES = {'CaughtMutant': 'killed', 'MissedMutant': 'survived', 'Timeout': 'timeout',
             'Unviable': 'unviable', 'Success': 'unclassified', 'Failure': 'unclassified'}
COUNTERS = {'caught': 'killed', 'missed': 'survived', 'timeout': 'timeout',
            'unviable': 'unviable', 'success': 'success'}
DENOMINATOR = 'killed / (killed + survived); excludes timeout, unviable, unclassified and unrun'


def require(condition, why):
    if not condition:
        raise ValueError(why)


def text(value, name, limit=4096):
    require(isinstance(value, str) and 0 < len(value) <= limit, 'invalid ' + name)
    return value


def finite(value):
    try:
        return isinstance(value, (float, int)) and not isinstance(value, bool) and math.isfinite(value)
    except OverflowError:
        return False


def mutation_id(mutant):
    require(isinstance(mutant, dict), 'mutant must be an object')
    text(mutant.get('package'), 'package', 256)
    file = text(mutant.get('file'), 'module path')
    # Rust #[path] modules can produce internal parent segments. This remains a raw identity
    # label, never a path we open; accept it only when lexical normalization stays within the repo.
    normalized = posixpath.normpath(file)
    require(not PurePosixPath(file).is_absolute() and normalized not in ('.', '..')
            and not normalized.startswith('../'), 'module path must stay relative to the repository')
    text(mutant.get('name'), 'mutant name')
    # mutants.json adds a diff; the scenario copy does not. Identity uses common producer fields.
    fields = {k: mutant.get(k) for k in ('package', 'file', 'name', 'function', 'span', 'replacement', 'genre')}
    return json.dumps(fields, sort_keys=True, separators=(',', ':'), allow_nan=False)


def mutation_rows(outcomes, planned, conclusion):
    require(isinstance(outcomes, dict) and isinstance(planned, list), 'invalid mutation report shape')
    version = text(outcomes.get('cargo_mutants_version'), 'cargo-mutants version', 64)
    require(re.fullmatch(r'27\.\d+\.\d+(?:[-+][\w.-]+)?', version), 'unsupported cargo-mutants schema version')
    require(isinstance(outcomes.get('outcomes'), list), 'missing scenario outcomes')
    modules, universe, seen = {}, {}, set()
    for mutant in planned:
        identity = mutation_id(mutant)
        require(identity not in universe, 'duplicate generated mutant')
        module = (mutant['package'], mutant['file'])
        universe[identity] = module
        row = modules.setdefault(module, dict.fromkeys(('planned', 'killed', 'survived', 'timeout', 'unviable', 'unclassified'), 0))
        row['planned'] += 1
    baseline = []
    counters = dict.fromkeys(('killed', 'survived', 'timeout', 'unviable', 'unclassified', 'success'), 0)
    for outcome in outcomes['outcomes']:
        require(isinstance(outcome, dict), 'invalid scenario outcome')
        summary = outcome.get('summary')
        require(summary in SUMMARIES, 'unknown mutation outcome')
        scenario = outcome.get('scenario')
        if scenario == 'Baseline':
            baseline.append(summary)
            continue
        require(isinstance(scenario, dict) and set(scenario) == {'Mutant'}, 'unknown scenario')
        identity = mutation_id(scenario['Mutant'])
        require(identity in universe and identity not in seen, 'unknown or duplicate tested mutant')
        seen.add(identity)
        modules[universe[identity]][SUMMARIES[summary]] += 1
        counters[SUMMARIES[summary]] += 1
        counters['success'] += int(summary == 'Success')
    require(scorecard.count(outcomes.get('total_mutants')) and outcomes['total_mutants'] == len(seen),
            'mutation total does not match individual outcomes')
    for producer, normalized in COUNTERS.items():
        require(scorecard.count(outcomes.get(producer)) and outcomes[producer] == counters[normalized],
                'mutation summary counter mismatch: ' + producer)
    ended = outcomes.get('end_time')
    started = text(outcomes.get('start_time'), 'mutation start time', 128)
    if ended is not None:
        ended = text(ended, 'mutation end time', 128)
        require(scorecard.datetime.datetime.fromisoformat(ended.replace('Z', '+00:00')) >=
                scorecard.datetime.datetime.fromisoformat(started.replace('Z', '+00:00')),
                'mutation end precedes start')
    baseline_passed = bool(baseline) and all(value == 'Success' for value in baseline)
    complete = bool(ended) and len(seen) == len(universe) and conclusion in ('success', 'failure')
    rows = []
    for (package, file), counts in sorted(modules.items()):
        counts['tested'] = sum(counts[k] for k in ('killed', 'survived', 'timeout', 'unviable', 'unclassified'))
        counts['unrun'] = counts['planned'] - counts['tested']
        denominator = counts['killed'] + counts['survived']
        eligible = complete and baseline_passed and counts['unclassified'] == 0 and denominator > 0
        # A diagnostic ratio from a partial subset is never a passing module score.
        score = counts['killed'] / denominator if eligible else None
        gate = ('unverified' if not eligible else 'failed' if counts['survived'] or counts['timeout']
                else 'observed_no_survivors')
        rows.append({'schema': 1, 'package': package, 'module': file, 'counts': counts,
                     'denominator_policy': DENOMINATOR, 'score_denominator': denominator,
                     'mutation_score': score, 'score_status': gate, 'run_complete': complete,
                     'baseline_passed': baseline_passed, 'job_conclusion': conclusion,
                     'planned_run': len(universe), 'tested_run': len(seen), 'start_time': started,
                     'end_time': ended, 'cargo_mutants_version': version})
    return rows


def eval_row(report):
    require(isinstance(report, dict), 'eval report must be an object')
    require(report.get('schema') is None or (type(report['schema']) is int and report['schema'] in (1, 2)), 'unsupported eval schema version')
    text(report.get('suite'), 'suite', 256)
    text(report.get('provider'), 'provider', 256)
    require(re.fullmatch(r'[a-f0-9]{64}', str(report.get('dataset_hash', ''))), 'invalid dataset hash')
    require(isinstance(report.get('passed'), bool), 'invalid suite verdict')
    metrics = report.get('metrics')
    require(isinstance(metrics, list) and 0 < len(metrics) <= 1024, 'empty or oversized eval metrics')
    names, legacy = set(), report.get('schema') != 2
    recomputed = []
    for metric in metrics:
        require(isinstance(metric, dict), 'invalid eval metric')
        name = text(metric.get('metric'), 'metric', 256)
        require(name not in names, 'duplicate metric')
        names.add(name)
        require(isinstance(metric.get('passed'), bool) and isinstance(metric.get('reason'), str), 'invalid metric verdict')
        require(finite(metric.get('bound')), 'invalid gate bound')
        if not all(key in metric for key in ('confidence', 'tolerance')):
            legacy = True
        else:
            require(finite(metric['confidence']) and 0 < metric['confidence'] < 1
                    and finite(metric['tolerance']) and metric['tolerance'] >= 0, 'invalid confidence/tolerance')
        value, lo, hi, n = (metric.get(key) for key in ('value', 'ci_lo', 'ci_hi', 'n'))
        require(scorecard.count(n), 'invalid eval sample count')
        require(all(v is None or finite(v) for v in (value, lo, hi)), 'invalid eval numeric fields')
        measured = all(finite(v) for v in (value, lo, hi)) and n > 0 and lo <= value <= hi
        if metric['passed']:
            require(measured and value >= metric['bound'], 'passing metric has invalid/missing measurements or is below its bound')
        if not legacy:
            passed = measured and value >= metric['bound'] and lo >= metric['bound'] - metric['tolerance']
            require(metric['passed'] == passed, 'metric verdict disagrees with gate fields')
            recomputed.append(passed)
    require(report['passed'] == all(metric['passed'] for metric in metrics), 'suite verdict disagrees with metric verdicts')
    if not legacy:
        dataset = report.get('dataset')
        require(isinstance(dataset, dict), 'missing dataset provenance')
        text(dataset.get('name'), 'dataset name', 256)
        text(dataset.get('provenance'), 'dataset provenance')
        require(scorecard.count(dataset.get('version')) and dataset['version'] > 0
                and dataset.get('split') in ('dev', 'test') and dataset.get('hash') == report['dataset_hash'],
                'invalid dataset version/split/hash')
        require(scorecard.count(dataset.get('examples'))
                and all(metric['n'] <= dataset['examples'] for metric in metrics), 'invalid dataset/metric sample coverage')
        require(report['passed'] == all(recomputed), 'suite verdict disagrees with metric verdicts')
    result = dict(report)
    result['verification_status'] = 'unverified_legacy_report' if legacy else 'verified_gate_fields'
    result['gate_verdict'] = None if legacy else report['passed']
    return result


def validate_record(kind, value):
    """Validate ledger observations again during rendering, without trusting stored score claims."""
    if kind == 'eval':
        return eval_row(value)
    require(kind == 'mutation' and type(value.get('schema')) is int and value['schema'] == 1, 'unknown mutation record schema')
    text(value.get('package'), 'mutation package', 256)
    text(value.get('module'), 'mutation module')
    require(isinstance(value.get('run_complete'), bool) and isinstance(value.get('baseline_passed'), bool),
            'invalid mutation completeness/baseline')
    counts = value.get('counts')
    require(isinstance(counts, dict) and all(scorecard.count(counts.get(k)) for k in
            ('planned', 'tested', 'unrun', 'killed', 'survived', 'timeout', 'unviable', 'unclassified')), 'invalid mutation counts')
    require(counts['tested'] == sum(counts[k] for k in ('killed', 'survived', 'timeout', 'unviable', 'unclassified'))
            and counts['planned'] == counts['tested'] + counts['unrun'], 'mutation count arithmetic')
    require(scorecard.count(value.get('planned_run')) and scorecard.count(value.get('tested_run'))
            and counts['planned'] <= value['planned_run'] and counts['tested'] <= value['tested_run'] <= value['planned_run'],
            'mutation run coverage mismatch')
    if value['run_complete']:
        require(value['planned_run'] == value['tested_run'] and counts['unrun'] == 0
                and value.get('job_conclusion') in ('success', 'failure') and value.get('end_time') is not None,
                'partial or cancelled mutation run cannot be complete')
    require(value.get('mutation_score') is None or finite(value['mutation_score']), 'invalid mutation score number')
    require(scorecard.count(value.get('score_denominator')), 'invalid mutation denominator count')
    denominator = counts['killed'] + counts['survived']
    require(value.get('score_denominator') == denominator and value.get('denominator_policy') == DENOMINATOR,
            'mutation denominator policy mismatch')
    eligible = value.get('run_complete') is True and value.get('baseline_passed') is True and denominator > 0 and counts['unclassified'] == 0
    score = counts['killed'] / denominator if eligible else None
    status = ('unverified' if not eligible else 'failed' if counts['survived'] or counts['timeout'] else 'observed_no_survivors')
    require(value.get('mutation_score') == score and value.get('score_status') == status, 'mutation score/status mismatch')
    return value


def gh_json(endpoint, paginate=False):
    raw = subprocess.check_output(['gh', 'api'] + (['--paginate', '--slurp'] if paginate else []) + [endpoint])
    require(len(raw) <= scorecard.MAX_JSON, 'GitHub response exceeds evidence limit')
    return json.loads(raw)


def validate_weekly_origin(run, job, artifact, workflow, repository, branch, manifest=None):
    require(run.get('repository', {}).get('full_name') == repository
            and run.get('head_repository', {}).get('full_name') == repository
            and run.get('head_branch') == branch and run.get('status') == 'completed'
            and run.get('event') in ('schedule', 'workflow_dispatch')
            and run.get('workflow_id') == workflow.get('id')
            and workflow.get('path') == run.get('path') == '.github/workflows/weekly.yml'
            and re.fullmatch(r'[a-f0-9]{40}', str(run.get('head_sha', ''))), 'weekly origin mismatch')
    require(job.get('name') == 'mutation' and job.get('status') == 'completed'
            and job.get('run_id') == run['id'] and job.get('run_attempt') == run['run_attempt']
            and job.get('head_sha') == run['head_sha'], 'mutation job origin mismatch')
    producer = artifact.get('workflow_run', {})
    require(artifact.get('name') == 'mutants' and artifact.get('expired') is False
            and producer.get('id') == run['id'] and producer.get('head_sha') == run['head_sha']
            and producer.get('head_branch') == branch, 'mutation artifact origin mismatch')
    if manifest is None:
        require(run.get('run_attempt') == 1, 'legacy artifact has ambiguous rerun attempt')
        proof = 'legacy_unambiguous_attempt_one'
    else:
        for field in ('id', 'run_attempt', 'head_sha', 'workflow_id', 'path', 'head_branch'):
            require(manifest.get(field) == run.get(field), 'mutation producer manifest mismatch')
        proof = 'exact_producer_manifest'
    return {'repository': repository, 'run_id': run['id'], 'attempt': run['run_attempt'],
            'job_id': job['id'], 'revision': run['head_sha'], 'url': job['html_url'],
            'artifact_id': artifact['id'], 'workflow_id': workflow['id'], 'workflow_path': workflow['path'],
            'branch': branch, 'authentication': proof, 'run_conclusion': run.get('conclusion'),
            'job_conclusion': job.get('conclusion')}


def zip_json(archive, name):
    item = archive.getinfo(name)
    require(item.file_size <= scorecard.MAX_JSON, 'archived JSON exceeds 64 MiB')
    with archive.open(item) as stream:
        raw = stream.read(scorecard.MAX_JSON + 1)
    require(len(raw) <= scorecard.MAX_JSON, 'archived JSON exceeds 64 MiB')
    return json.loads(raw), hashlib.sha256(raw).hexdigest()


def import_mutation_archive(path, run, job, artifact, workflow, repository, branch):
    with zipfile.ZipFile(path) as archive:
        require(len(archive.namelist()) == len(set(archive.namelist())), 'duplicate archive paths')
        manifests = [name for name in archive.namelist() if name == 'mutants-producer.json']
        manifest = zip_json(archive, manifests[0])[0] if manifests else None
        source = validate_weekly_origin(run, job, artifact, workflow, repository, branch, manifest)
        outcomes = [name for name in archive.namelist() if name.endswith('/mutants.out/outcomes.json')]
        require(outcomes and len(set(outcomes)) == len(outcomes), 'missing or duplicate mutation outcomes files')
        records = []
        expected_groups = {'mutants/mutants.out/outcomes.json', 'mutants-crates/mutants.out/outcomes.json'}
        require(set(outcomes).issubset(expected_groups), 'unexpected mutation artifact layout')
        for name in sorted(outcomes):
            planned_path = name.removesuffix('outcomes.json') + 'mutants.json'
            report, report_hash = zip_json(archive, name)
            planned, planned_hash = zip_json(archive, planned_path)
            for row in mutation_rows(report, planned, job['conclusion']):
                identity = hashlib.sha256((name + '\n' + row['package'] + '\n' + row['module']).encode()).hexdigest()
                row['artifact_groups_observed'] = sorted(set(outcomes))
                row['artifact_groups_missing'] = sorted(expected_groups - set(outcomes))
                row['source'] = dict(source, key=f'{repository}/{run["id"]}/{run["run_attempt"]}/{job["id"]}/mutation/{identity}',
                                     artifact_path=name, outcomes_sha256=report_hash, mutants_sha256=planned_hash)
                records.append(row)
        return records


def fetch_weekly(repository, branch):
    """Select only via the configured workflow and its run-scoped artifact list, then verify origin."""
    workflow = gh_json(f'repos/{repository}/actions/workflows/weekly.yml')
    require(workflow.get('path') == '.github/workflows/weekly.yml', 'wrong weekly workflow')
    quoted = urllib.parse.quote(branch, safe='')
    runs = gh_json(f'repos/{repository}/actions/workflows/{workflow["id"]}/runs?branch={quoted}&status=completed&per_page=20')
    for candidate in runs['workflow_runs']:
        run = gh_json(f'repos/{repository}/actions/runs/{candidate["id"]}')
        if run.get('event') not in ('schedule', 'workflow_dispatch'):
            continue
        artifact_pages = gh_json(f'repos/{repository}/actions/runs/{run["id"]}/artifacts?per_page=100', paginate=True)
        artifacts = {'artifacts': [artifact for page in artifact_pages for artifact in page['artifacts']]}
        if any(artifact['name'] == 'mutants-plan' for artifact in artifacts['artifacts']):
            import mutation_results
            jobs = mutation_results.paged(f'repos/{repository}/actions/runs/{run["id"]}/attempts/{run["run_attempt"]}/jobs?per_page=100', 'jobs')
            return mutation_results.fetch(repository, branch, run, workflow, artifacts['artifacts'], jobs)['modules']
        matching = [a for a in artifacts['artifacts'] if a['name'] == 'mutants' and not a['expired']]
        if not matching:
            continue
        require(len(matching) == 1, 'ambiguous mutation artifacts')
        jobs = gh_json(f'repos/{repository}/actions/runs/{run["id"]}/attempts/{run["run_attempt"]}/jobs?per_page=100')
        mutation = [job for job in jobs['jobs'] if job['name'] == 'mutation']
        require(len(mutation) == 1, 'missing/ambiguous mutation job')
        artifact = matching[0]
        with tempfile.TemporaryDirectory(prefix='lattice-mutation-') as tmp:
            path = Path(tmp) / 'mutants.zip'
            with path.open('wb') as stream:
                subprocess.run(['gh', 'api', f'repos/{repository}/actions/artifacts/{artifact["id"]}/zip'], stdout=stream, check=True)
            return import_mutation_archive(path, run, mutation[0], artifact, workflow, repository, branch)
    return []


def append_unique(binary, ledger, kind, observations):
    existing, invalid = scorecard.normalized(scorecard.export_ledger(ledger, binary))
    require(not invalid, 'cannot append to invalid existing evidence')
    seen = {(r['kind'], r['value'].get('source', {}).get('key')): r['value'] for r in existing}
    for value in observations:
        key = (kind, value['source']['key'])
        if key in seen:
            require(seen[key] == value, 'conflicting metric evidence source key')
        else:
            scorecard.record(binary, ledger, kind, value)
            seen[key] = value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    weekly = sub.add_parser('ingest-weekly')
    weekly.add_argument('--repository', required=True)
    weekly.add_argument('--branch', required=True)
    evaluate = sub.add_parser('ingest-eval')
    evaluate.add_argument('--report', required=True)
    evaluate.add_argument('--source', required=True, help='explicit caller-supplied producer metadata; not artifact authentication')
    for command in (weekly, evaluate):
        command.add_argument('--ledger', required=True)
        command.add_argument('--lattice', required=True)
    args = parser.parse_args()
    if args.command == 'ingest-weekly':
        append_unique(args.lattice, args.ledger, 'mutation', fetch_weekly(args.repository, args.branch))
    else:
        with Path(args.report).open('rb') as stream:
            raw = stream.read(scorecard.MAX_JSON + 1)
        require(len(raw) <= scorecard.MAX_JSON, 'eval report exceeds 64 MiB')
        value = eval_row(json.loads(raw))
        source = scorecard.load(args.source)
        require(isinstance(source, dict), 'source must be an object')
        for field in ('repository', 'run_id', 'attempt', 'job_id', 'revision'):
            require(field in source, 'missing source ' + field)
        require(re.fullmatch(r'[a-f0-9]{40}', str(source['revision'])), 'invalid producer revision')
        require(all(str(source[k]).isdigit() and int(source[k]) > 0 for k in ('run_id', 'attempt', 'job_id')),
                'invalid producer run/attempt/job')
        require(re.fullmatch(r'[\w.-]+/[\w.-]+', str(source['repository'])), 'invalid producer repository')
        digest = hashlib.sha256(raw).hexdigest()
        source['key'] = f'{source["repository"]}/{source["run_id"]}/{source["attempt"]}/{source["job_id"]}/eval/{digest}'
        source['authentication'] = 'caller_supplied_report_and_run_association'
        source['report_sha256'] = digest
        value['source'] = source
        append_unique(args.lattice, args.ledger, 'eval', [value])


if __name__ == '__main__':
    main()
