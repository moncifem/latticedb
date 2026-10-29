#!/usr/bin/env python3
"""Validate sharded mutation artifacts; a partial campaign never becomes a complete score."""
import argparse
import hashlib
import json
import re
import stat
import subprocess
import tempfile
import zipfile
from pathlib import Path, PurePosixPath

import mutation_campaign as campaign
import scorecard
import scorecard_metrics as metrics
from scorecard_metrics import require


def validate_baseline(shard, assigned, trusted_manifest):
    require(not shard.get('baseline_read_error'), 'matching-scope baseline JSON malformed')
    proof = shard.get('baseline')
    require(isinstance(proof, dict), 'matching-scope baseline proof missing or malformed')
    expected = campaign.baseline_context(trusted_manifest, assigned)
    context = {key: value for key, value in proof.items() if key != 'commands'}
    require(campaign.same_json(context, expected),
            'baseline source, plan or assignment differs')
    commands = campaign.baseline_commands(trusted_manifest['group'], assigned)
    records = proof.get('commands')
    require(isinstance(records, list) and len(records) == len(commands), 'baseline scopes missing or duplicated')
    logs = shard.get('baseline_logs', {})
    require(isinstance(logs, dict), 'baseline log collection malformed')
    for index, (record, arguments) in enumerate(zip(records, commands)):
        name = f'baselines/{index}.log'
        require(isinstance(record, dict) and set(record) ==
                {'arguments', 'status', 'exit_code', 'log', 'log_sha256'}, 'baseline command incomplete or malformed')
        require(record['arguments'] == arguments and record['status'] == 'completed'
                and type(record['exit_code']) is int and record['exit_code'] == 0
                and record['log'] == name, 'baseline scope, completion or exit status differs')
        raw = logs.get(name)
        require(isinstance(raw, bytes) and len(raw) <= scorecard.MAX_JSON
                and record['log_sha256'] == hashlib.sha256(raw).hexdigest(), 'baseline log missing or digest differs')


def aggregate(plan, universes, shards, source_complete):
    assignments = {group: {} for group in plan['groups']}
    modules, diagnostics, invalid, tested_ids = {}, [], [], set()
    for group, universe in universes.items():
        for mutant in universe:
            key = (group, mutant['package'], mutant['file'])
            row = modules.setdefault(key, dict.fromkeys(('planned', 'tested', 'unrun', 'killed', 'survived',
                                                        'timeout', 'unviable', 'unclassified'), 0))
            row['planned'] += 1
    for item in plan['matrix']['include']:
        group, index = item['group'], item['shard']
        key = f'{group}/{index}'
        shard = shards.get(key)
        diagnostic = {'group': group, 'shard': index, 'planned': item['planned'], 'state': 'missing'}
        diagnostics.append(diagnostic)
        if shard is None:
            continue
        diagnostic.update({k: shard.get(k) for k in ('job_id', 'job_status', 'job_conclusion', 'artifact_id', 'artifact_digest')})
        if shard.get('artifact_id') is None:
            diagnostic['state'] = 'missing_artifact_after_' + str(shard.get('job_conclusion') or shard.get('job_status'))
            continue
        try:
            require(not shard.get('generation_error'), shard.get('generation_error', 'generation proof missing'))
            manifest = shard['manifest']
            require(isinstance(manifest, dict), 'missing mutation shard manifest')
            trusted_manifest = {'schema': 1, 'source': plan['source'], 'group': group, 'shard': index,
                                'total': item['total'], 'tool_version': campaign.VERSION,
                                'universe_sha256': plan['groups'][group]['universe_sha256'],
                                'plan_sha256': plan.get('plan_sha256')}
            require(isinstance(manifest.get('source'), dict)
                    and all(type(manifest.get(field)) is int for field in ('schema', 'shard', 'total'))
                    and all(type(manifest.get('source', {}).get(field)) is int
                            for field in ('run_id', 'attempt', 'workflow_id'))
                    and campaign.same_json({key: manifest.get(key) for key in trusted_manifest}, trusted_manifest),
                    'shard manifest/plan mismatch')
            if shard.get('outcomes') is not None:
                expected_command = campaign.command(group) + ['--shard', f'{index}/{item["total"]}', '--sharding', 'round-robin', '--output']
                actual_command = shard.get('command', {}).get('arguments', [])
                require(len(actual_command) == len(expected_command) + 1
                        and actual_command[:-1] == expected_command and isinstance(actual_command[-1], str)
                        and actual_command[-1], 'executed mutation command changed flags or assignment')
            assigned = shard['assigned']
            campaign.check_assignment(plan, group, index, assigned)
            assignments[group][index] = assigned
            try:
                validate_baseline(shard, assigned, trusted_manifest)
                diagnostic['baseline_scope'] = 'verified_matching_scope'
            except (ValueError, KeyError, TypeError) as error:
                diagnostic['baseline_scope'] = 'unverified'
                diagnostic['baseline_scope_error'] = str(error)
            if shard.get('mutants') is None or shard.get('outcomes') is None:
                diagnostic['state'] = 'not_executed_or_interrupted'
                continue
            require(campaign.identities(shard['mutants']) == campaign.identities(assigned), 'executed mutants differ from assigned universe')
            rows = metrics.mutation_rows(shard['outcomes'], assigned, shard.get('job_conclusion'))
            outcome_ids = campaign.identities([entry['scenario']['Mutant'] for entry in shard['outcomes']['outcomes']
                                               if isinstance(entry.get('scenario'), dict) and 'Mutant' in entry['scenario']])
            require(not tested_ids.intersection(outcome_ids), 'mutant outcome counted more than once')
            for row in rows:
                counts = modules[(group, row['package'], row['module'])]
                for count in ('tested', 'killed', 'survived', 'timeout', 'unviable', 'unclassified'):
                    counts[count] += row['counts'][count]
            tested_ids.update(outcome_ids)
            diagnostic['internal_baseline_passed'] = bool(rows) and all(row['baseline_passed'] for row in rows)
            diagnostic['baseline_passed'] = (diagnostic['baseline_scope'] == 'verified_matching_scope'
                                             and diagnostic['internal_baseline_passed'])
            diagnostic['start_time'] = rows[0]['start_time'] if rows else None
            diagnostic['end_time'] = rows[0]['end_time'] if rows else None
            diagnostic['tested'] = len(outcome_ids)
            diagnostic['state'] = ('complete' if shard.get('job_status') == 'completed'
                                   and all(row['run_complete'] for row in rows) else 'partial_or_runner_failed')
        except (ValueError, KeyError, TypeError) as error:
            diagnostic['state'] = 'invalid'
            diagnostic['error'] = str(error)
            invalid.append(key + ': ' + str(error))
    proof = campaign.partition_proof(plan, assignments)
    classified = sum(row['unclassified'] for row in modules.values()) == 0
    proof.update({'all_outcomes_classified': classified, 'invalid_shards': invalid, 'source_workflow_completed': source_complete,
                  'all_outcomes_complete': all(row['state'] == 'complete' for row in diagnostics),
                  'all_baseline_scopes_verified': all(row.get('baseline_scope') == 'verified_matching_scope' for row in diagnostics),
                  'all_baselines_passed': all(row.get('baseline_passed') is True for row in diagnostics)})
    complete = (source_complete and proof['union_equals_full_universe'] and not invalid
                and proof['all_outcomes_complete'] and classified)
    records = []
    starts = [row['start_time'] for row in diagnostics if row.get('start_time')]
    ends = [row['end_time'] for row in diagnostics if row.get('end_time')]
    for (group, package, module), counts in sorted(modules.items()):
        counts['unrun'] = counts['planned'] - counts['tested']
        denominator = counts['killed'] + counts['survived']
        eligible = complete and proof['all_baselines_passed'] and denominator > 0 and counts['unclassified'] == 0
        records.append({'schema': 1, 'group': group, 'package': package, 'module': module, 'counts': counts,
            'planned_run': proof['expected_mutants'], 'tested_run': len(tested_ids), 'run_complete': complete,
            'baseline_passed': proof['all_baselines_passed'], 'score_denominator': denominator,
            'denominator_policy': metrics.DENOMINATOR, 'mutation_score': counts['killed'] / denominator if eligible else None,
            'score_status': 'unverified' if not eligible else 'failed' if counts['survived'] or counts['timeout'] else 'observed_no_survivors',
            'job_conclusion': 'partial' if not complete else 'failure' if any(row.get('job_conclusion') != 'success' for row in diagnostics) else 'success',
            'start_time': min(starts) if starts else None, 'end_time': max(ends) if complete and ends else None,
            'cargo_mutants_version': campaign.VERSION, 'campaign_proof': proof, 'shards': diagnostics})
    totals = {key: sum(row[key] for row in modules.values()) for key in ('planned', 'tested', 'unrun', 'killed', 'survived', 'timeout', 'unviable', 'unclassified')}
    return {'schema': 1, 'outcome_counts': totals, 'scope': 'completed_source' if source_complete else 'diagnostic_only_workflow_in_progress',
            'source': plan['source'], 'proof': proof, 'shards': diagnostics, 'modules': records}


def validate_origin(run, workflow, repository, branch, diagnostic):
    require(run.get('repository', {}).get('full_name') == repository
            and run.get('head_repository', {}).get('full_name') == repository
            and run.get('head_branch') == branch and run.get('path') == workflow.get('path') == '.github/workflows/weekly.yml'
            and run.get('workflow_id') == workflow.get('id') and run.get('event') in ('schedule', 'workflow_dispatch')
            and re.fullmatch(r'[a-f0-9]{40}', str(run.get('head_sha', '')))
            and (diagnostic or run.get('status') == 'completed'), 'untrusted/incomplete mutation source')


def extract_archive(artifact, run, repository, directory):
    origin = artifact.get('workflow_run', {})
    require(artifact.get('expired') is False and origin.get('id') == run['id']
            and origin.get('head_sha') == run['head_sha'] and origin.get('head_branch') == run['head_branch']
            and origin.get('repository_id') == run['repository']['id']
            and origin.get('head_repository_id') == run['head_repository']['id']
            and re.fullmatch(r'sha256:[a-f0-9]{64}', str(artifact.get('digest', '')))
            and 0 < artifact.get('size_in_bytes', 0) <= scorecard.MAX_JSON, 'mutation artifact provenance/bounds mismatch')
    directory = Path(directory)
    directory.mkdir(parents=True)
    path = directory / 'artifact.zip'
    with path.open('wb') as stream:
        subprocess.run(['gh', 'api', f'repos/{repository}/actions/artifacts/{artifact["id"]}/zip'], stdout=stream, check=True)
    require(campaign.digest(path) == artifact['digest'].removeprefix('sha256:'), 'mutation artifact archive digest mismatch')
    with zipfile.ZipFile(path) as archive:
        items = archive.infolist()
        require(len(items) <= 2000 and len(items) == len({item.filename for item in items})
                and sum(item.file_size for item in items) <= scorecard.MAX_JSON, 'mutation archive expansion/duplicates')
        for item in items:
            name = PurePosixPath(item.filename)
            require(not name.is_absolute() and '..' not in name.parts
                    and not stat.S_ISLNK(item.external_attr >> 16), 'unsafe mutation artifact path')
            if not (item.filename.endswith('.json') or re.fullmatch(r'baselines/\d+\.log', item.filename)):
                continue
            target = directory / item.filename
            target.parent.mkdir(parents=True, exist_ok=True)
            with archive.open(item) as stream:
                raw = stream.read(scorecard.MAX_JSON + 1)
            require(len(raw) <= scorecard.MAX_JSON and len(raw) == item.file_size, 'mutation archive entry length')
            target.write_bytes(raw)


def fetch(repository, branch, run, workflow, artifacts, jobs, diagnostic=False):
    validate_origin(run, workflow, repository, branch, diagnostic)
    plan_jobs = [job for job in jobs if job['name'] == 'mutation-plan']
    require(len(plan_jobs) == 1 and plan_jobs[0].get('run_id') == run['id']
            and plan_jobs[0].get('run_attempt') == run['run_attempt'] and plan_jobs[0].get('head_sha') == run['head_sha']
            and plan_jobs[0].get('status') == 'completed' and plan_jobs[0].get('conclusion') == 'success',
            'complete-universe planning job did not succeed with matching provenance')
    plans = [artifact for artifact in artifacts if artifact['name'] == 'mutants-plan']
    require(len(plans) == 1, 'missing/duplicate complete-universe plan artifact')
    with tempfile.TemporaryDirectory(prefix='lattice-mutation-campaign-') as tmp:
        root = Path(tmp)
        extract_archive(plans[0], run, repository, root / 'plan')
        plan, universes = campaign.load_plan(root / 'plan')
        expected_source = campaign.source_from_run(run, repository, run['head_sha'])
        require(campaign.same_json(plan['source'], expected_source), 'mutation plan source run/attempt/revision mismatch')
        plan_hash = campaign.digest(root / 'plan/plan.json')
        plan['plan_sha256'] = plan_hash
        shards = {}
        for row in plan['matrix']['include']:
            group, index = row['group'], row['shard']
            name = f'mutants-{group}-{index}'
            job_name = f'mutation ({group}, {index})'
            selected_jobs = [job for job in jobs if job['name'] == job_name]
            selected_artifacts = [artifact for artifact in artifacts if artifact['name'] == name]
            require(len(selected_jobs) <= 1 and len(selected_artifacts) <= 1, 'duplicate matrix producer/job artifact')
            if not selected_jobs:
                require(not selected_artifacts, 'mutation artifact has no attributable matrix job')
                continue
            job = selected_jobs[0]
            require(job.get('run_id') == run['id'] and job.get('run_attempt') == run['run_attempt']
                    and job.get('head_sha') == run['head_sha'], 'matrix mutation job provenance mismatch')
            if not selected_artifacts:
                shards[f'{group}/{index}'] = {'job_id': job['id'], 'job_status': job['status'],
                                             'job_conclusion': job.get('conclusion')}
                continue
            artifact = selected_artifacts[0]
            target = root / name
            extract_archive(artifact, run, repository, target)
            def read(relative):
                path = target / relative
                return scorecard.load(path) if path.exists() else None
            manifest = read('manifest.json')
            # A malformed/missing new proof must not erase otherwise valid legacy outcomes.
            baseline_error = None
            try:
                baseline = read('baseline.json')
            except (ValueError, TypeError, RecursionError) as error:
                baseline, baseline_error = None, str(error)
            generation_error = None
            if not isinstance(manifest, dict) or manifest.get('plan_sha256') != plan_hash:
                generation_error = 'mutation shard did not bind the exact plan bytes'
            regenerated = read('regenerated-universe.json')
            if regenerated is None or campaign.identities(regenerated) != plan['groups'][group]['ids']:
                generation_error = 'matrix regenerated universe missing/mismatched'
            elif campaign.digest(target / 'regenerated-universe.json') != plan['groups'][group]['universe_sha256']:
                generation_error = 'matrix regenerated full source bytes changed'
            shards[f'{group}/{index}'] = {'generation_error': generation_error, 'manifest': manifest, 'command': read('command.json') or {}, 'assigned': read('assigned.json'),
                'baseline': baseline, 'baseline_read_error': baseline_error,
                'baseline_logs': {str(path.relative_to(target)): path.read_bytes()
                                  for path in (target / 'baselines').glob('*.log')},
                'mutants': read('mutants.out/mutants.json'), 'outcomes': read('mutants.out/outcomes.json'),
                'job_id': job['id'], 'job_status': job['status'], 'job_conclusion': job.get('conclusion'),
                'artifact_id': artifact['id'], 'artifact_digest': artifact['digest']}
        result = aggregate(plan, universes, shards, run.get('status') == 'completed')
        summary_jobs = [job for job in jobs if job['name'] == 'mutation-summary']
        require(len(summary_jobs) == 1, 'missing/ambiguous mutation summary source job')
        summary = summary_jobs[0]
        require(summary.get('run_id') == run['id'] and summary.get('run_attempt') == run['run_attempt']
                and summary.get('head_sha') == run['head_sha'], 'mutation summary source job mismatch')
        for row in result['modules']:
            key = hashlib.sha256((row['group'] + '\n' + row['package'] + '\n' + row['module']).encode()).hexdigest()
            row['source'] = {'key': f'{repository}/{run["id"]}/{run["run_attempt"]}/{summary["id"]}/mutation/{key}',
                'repository': repository, 'run_id': run['id'], 'attempt': run['run_attempt'], 'job_id': summary['id'],
                'revision': run['head_sha'], 'url': summary['html_url'], 'workflow_id': workflow['id'],
                'workflow_path': workflow['path'], 'run_conclusion': run.get('conclusion'),
                'authentication': 'validated_shard_artifacts_and_complete_universe_plan',
                'plan_artifact_id': plans[0]['id'], 'plan_artifact_digest': plans[0]['digest'], 'plan_sha256': plan_hash}
        return result


def paged(endpoint, key):
    pages = metrics.gh_json(endpoint, paginate=True)
    return [item for page in pages for item in page[key]]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('repository', 'branch', 'run', 'attempt', 'output'):
        parser.add_argument('--' + name, required=True)
    args = parser.parse_args()
    repo = f'repos/{args.repository}'
    run = metrics.gh_json(f'{repo}/actions/runs/{args.run}')
    require(str(run['id']) == args.run and str(run['run_attempt']) == args.attempt, 'diagnostic run/attempt mismatch')
    workflow = metrics.gh_json(f'{repo}/actions/workflows/weekly.yml')
    artifacts = paged(f'{repo}/actions/runs/{args.run}/artifacts?per_page=100', 'artifacts')
    jobs = paged(f'{repo}/actions/runs/{args.run}/attempts/{args.attempt}/jobs?per_page=100', 'jobs')
    result = fetch(args.repository, args.branch, run, workflow, artifacts, jobs, diagnostic=True)
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    campaign.write(output / 'campaign.json', result)
    print(json.dumps({'scope': result['scope'], 'proof': result['proof'], 'outcome_counts': result['outcome_counts'], 'shards': result['shards']}, indent=2))
    require(not result['proof']['invalid_shards'] and result['proof']['union_equals_full_universe']
            and result['proof']['all_outcomes_complete'] and result['proof']['all_baselines_passed']
            and result['proof']['all_outcomes_classified'],
            'mutation campaign has missing, invalid, incomplete or failed-baseline shards')


if __name__ == '__main__':
    main()
