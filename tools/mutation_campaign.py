#!/usr/bin/env python3
"""Complete-universe mutation shards. Listing is not mutation execution or passing evidence."""
import argparse
import hashlib
import json
import math
import subprocess
import sys
from pathlib import Path

import scorecard
import scorecard_metrics as metrics
from scorecard_metrics import require

VERSION = '27.1.0'
MAX_PER_SHARD = 24
GROUPS = {
    'dependent': (['lattice-core', 'lattice-lsl'], True),
    'self': (['lattice-abi', 'lattice-evalkit', 'lattice-sim', 'lattice-seams',
              'lattice-cli', 'lattice-bench', 'lattice-sdk'], False),
}


def command(group):
    packages, workspace = GROUPS[group]
    args = ['cargo', 'mutants']
    for package in packages:
        args += ['-p', package]
    return args + [f'--test-workspace={str(workspace).lower()}', '--timeout', '120', '--no-shuffle', '-j', '2']


def baseline_commands(group, assigned):
    packages = sorted({mutant['package'] for mutant in assigned})
    require(packages and set(packages).issubset(GROUPS[group][0]), 'baseline assignment has unexpected packages')
    # Each self-group mutant tests its own package; combining packages can unify features.
    return ([['cargo', 'test', '--workspace']] if GROUPS[group][1] else
            [['cargo', 'test', '--package', package] for package in packages])


def baseline_context(manifest, assigned):
    return {key: manifest[key] for key in
            ('source', 'group', 'shard', 'total', 'plan_sha256', 'universe_sha256')} | {
                'schema': 1, 'assigned_ids': identities(assigned)}


def run_logged(arguments, path):
    with path.open('wb') as log:
        with subprocess.Popen(arguments, stdout=subprocess.PIPE, stderr=subprocess.STDOUT) as child:
            require(child.stdout is not None, 'child has no output stream')
            for line in child.stdout:
                log.write(line)
                log.flush()
                sys.stdout.buffer.write(line)
                sys.stdout.buffer.flush()
            return child.wait()


def run_baselines(manifest, assigned, output):
    proof = baseline_context(manifest, assigned)
    proof['commands'] = [{'arguments': arguments, 'status': 'pending', 'log': f'baselines/{index}.log'}
                         for index, arguments in enumerate(baseline_commands(manifest['group'], assigned))]
    (output / 'baselines').mkdir()
    write(output / 'baseline.json', proof)
    for record in proof['commands']:
        record['status'] = 'started'
        write(output / 'baseline.json', proof)
        path = output / record['log']
        code = run_logged(record['arguments'], path)
        record.update(status='completed', exit_code=code, log_sha256=digest(path))
        write(output / 'baseline.json', proof)
        if code:
            return code
    return 0


def identities(mutants):
    require(isinstance(mutants, list), 'mutant universe must be an array')
    result = [hashlib.sha256(metrics.mutation_id(mutant).encode()).hexdigest() for mutant in mutants]
    require(len(result) == len(set(result)), 'duplicate generated mutant identities')
    return result


def digest(path):
    with Path(path).open('rb') as stream:
        raw = stream.read(scorecard.MAX_JSON + 1)
    require(len(raw) <= scorecard.MAX_JSON, 'mutation input exceeds 64 MiB')
    return hashlib.sha256(raw).hexdigest()


def write(path, value):
    Path(path).write_text(json.dumps(value, sort_keys=True, indent=2) + '\n')


def make_plan(universes, source):
    require(set(universes) == set(GROUPS), 'missing/extra mutation crate group')
    groups, matrix = {}, []
    for group, universe in universes.items():
        ids = identities(universe)
        require(ids, 'empty mutation group cannot establish coverage')
        total = math.ceil(len(ids) / MAX_PER_SHARD)
        groups[group] = {'ids': ids, 'count': len(ids), 'shards': total,
                         'command': command(group), 'universe_file': group + '.json'}
        matrix += [{'group': group, 'shard': index, 'total': total, 'planned': len(ids[index::total])}
                   for index in range(total)]
    require(len(matrix) <= 256, 'complete plan exceeds GitHub matrix ceiling; review a different full-universe shard size')
    return {'schema': 1, 'tool_version': VERSION, 'source': source, 'sharding': 'round-robin',
            'max_per_shard': MAX_PER_SHARD, 'groups': groups, 'matrix': {'include': matrix}}


def check_assignment(plan, group, index, mutants):
    block = plan['groups'][group]
    require(type(index) is int and 0 <= index < block['shards'], 'invalid shard index')
    actual = identities(mutants)
    require(actual == block['ids'][index::block['shards']], 'actual shard differs from complete-universe assignment')
    return actual


def partition_proof(plan, assignments):
    """Independent observed assignment union, retaining missing shards instead of assuming coverage."""
    seen, missing, observed, expected = set(), [], 0, 0
    for group, block in plan['groups'].items():
        expected += len(block['ids'])
        known = assignments.get(group, {})
        require(set(known).issubset(set(range(block['shards']))), 'unexpected shard assignment')
        for index in range(block['shards']):
            if index not in known:
                missing.append(f'{group}/{index}')
                continue
            ids = check_assignment(plan, group, index, known[index])
            require(not seen.intersection(ids), 'mutant assigned more than once')
            seen.update(ids)
            observed += len(ids)
    all_ids = [identity for block in plan['groups'].values() for identity in block['ids']]
    require(len(all_ids) == len(set(all_ids)), 'groups overlap in generated universe')
    complete = not missing and seen == set(all_ids)
    return {'expected_mutants': expected, 'assigned_mutants_observed': observed, 'missing_shards': missing,
            'no_duplicates': True, 'union_equals_full_universe': complete}


def source_from_run(run, repository, revision):
    require(run.get('repository', {}).get('full_name') == repository
            and run.get('head_repository', {}).get('full_name') == repository
            and run.get('head_sha') == revision and run.get('path') == '.github/workflows/weekly.yml'
            and run.get('event') in ('schedule', 'workflow_dispatch'), 'planning source provenance mismatch')
    return {'type': 'github_workflow', 'repository': repository, 'run_id': run['id'], 'attempt': run['run_attempt'],
            'revision': revision, 'branch': run['head_branch'], 'workflow_id': run['workflow_id'], 'workflow_path': run['path']}


def checkout_revision(clean=False):
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=scorecard.ROOT, text=True).strip()
    if clean:
        status = subprocess.check_output(['git', 'status', '--porcelain'], cwd=scorecard.ROOT, text=True).strip()
        require(not status, 'CI mutation planning/execution requires a clean committed checkout')
    return revision


def verify_tool():
    version = subprocess.check_output(['cargo', 'mutants', '--version'], text=True).strip()
    require(version == 'cargo-mutants ' + VERSION, 'mutation tool version differs from plan')


def list_mutants(group, path, shard=None):
    args = command(group) + ['--list', '--json']
    if shard is not None:
        index, total = shard
        args += ['--shard', f'{index}/{total}', '--sharding', 'round-robin']
    with Path(path).open('wb') as stream:
        subprocess.run(args, stdout=stream, check=True)
    return scorecard.load(path)


def plan_run(args):
    verify_tool()
    output = Path(args.output)
    require(not output.exists(), 'mutation plan output must be new')
    output.mkdir(parents=True)
    revision = checkout_revision(clean=not args.local_list_smoke)
    source = ({'type': 'local_list_smoke', 'revision': revision, 'scope': 'listing only; no mutation execution or CI evidence'}
              if args.local_list_smoke else source_from_run(scorecard.load(args.run_metadata), args.repository, revision))
    universes = {group: list_mutants(group, output / (group + '.json')) for group in GROUPS}
    plan = make_plan(universes, source)
    for group, block in plan['groups'].items():
        block['universe_sha256'] = digest(output / block['universe_file'])
    # Fail the matrix ceiling before issuing any shard execution. Listings below are real cargo
    # selections, independent of this script's Python partition model; no tests or mutants run.
    assignments = {group: {} for group in GROUPS}
    assignment_dir = output / 'assignments'
    assignment_dir.mkdir()
    for row in plan['matrix']['include']:
        path = assignment_dir / f'{row["group"]}-{row["shard"]}.json'
        assignments[row['group']][row['shard']] = list_mutants(row['group'], path, (row['shard'], row['total']))
    plan['listing_proof'] = partition_proof(plan, assignments)
    require(plan['listing_proof']['union_equals_full_universe'], 'generated shard listing union is incomplete')
    write(output / 'plan.json', plan)
    if args.github_output:
        with Path(args.github_output).open('a') as stream:
            stream.write('matrix=' + json.dumps(plan['matrix'], separators=(',', ':')) + '\n')
    print(json.dumps({'scope': source['type'], 'counts': {g: b['count'] for g, b in plan['groups'].items()},
                      'shards': len(plan['matrix']['include']), 'listing_proof': plan['listing_proof']}))


def load_plan(directory):
    directory = Path(directory)
    plan = scorecard.load(directory / 'plan.json')
    require(plan.get('schema') == 1 and plan.get('tool_version') == VERSION
            and plan.get('sharding') == 'round-robin' and set(plan['groups']) == set(GROUPS), 'unsupported mutation plan')
    universes = {}
    for group, block in plan['groups'].items():
        require(block.get('command') == command(group) and block.get('universe_file') == group + '.json', 'mutation flags/universe changed')
        path = directory / block['universe_file']
        require(digest(path) == block['universe_sha256'], 'full mutation universe digest mismatch')
        universes[group] = scorecard.load(path)
        require(identities(universes[group]) == block['ids'], 'universe identity mismatch')
    expected = make_plan(universes, plan['source'])
    require(expected['matrix'] == plan['matrix'] and plan['max_per_shard'] == MAX_PER_SHARD, 'mutation plan matrix changed')
    for group in GROUPS:
        for field in ('count', 'shards', 'command', 'ids', 'universe_file'):
            require(plan['groups'][group][field] == expected['groups'][group][field], 'mutation plan group changed')
    assignments = {group: {} for group in GROUPS}
    for row in plan['matrix']['include']:
        path = directory / 'assignments' / f'{row["group"]}-{row["shard"]}.json'
        assignments[row['group']][row['shard']] = scorecard.load(path)
    require(partition_proof(plan, assignments) == plan['listing_proof']
            and plan['listing_proof']['union_equals_full_universe'], 'real listing partition proof changed')
    return plan, universes


def shard_run(args):
    verify_tool()
    plan, universes = load_plan(args.plan)
    group, index = args.group, args.shard
    block = plan['groups'][group]
    revision = checkout_revision(clean=True)
    require(source_from_run(scorecard.load(args.run_metadata), args.repository, revision) == plan['source'],
            'shard run/attempt differs from the exact planning source')
    require(plan['source'].get('type') == 'github_workflow' and plan['source']['revision'] == revision,
            'shard must execute the exact CI plan revision')
    output = Path(args.output)
    require(not output.exists(), 'shard output must be new')
    output.mkdir(parents=True)
    manifest = {'schema': 1, 'source': plan['source'], 'group': group, 'shard': index, 'total': block['shards'],
                'tool_version': VERSION, 'universe_sha256': block['universe_sha256'], 'plan_sha256': digest(Path(args.plan) / 'plan.json')}
    write(output / 'manifest.json', manifest)
    actual = list_mutants(group, output / 'regenerated-universe.json')
    require(identities(actual) == identities(universes[group]), 'regenerated complete universe drifted')
    assigned = list_mutants(group, output / 'assigned.json', (index, block['shards']))
    check_assignment(plan, group, index, assigned)
    baseline_code = run_baselines(manifest, assigned, output)
    if baseline_code:
        return baseline_code
    arguments = command(group) + ['--shard', f'{index}/{block["shards"]}', '--sharding', 'round-robin', '--output', str(output)]
    write(output / 'command.json', {'arguments': arguments})
    code = run_logged(arguments, output / 'command.log')
    write(output / 'execution.json', {'exit_code': code, 'command': arguments, 'scope': 'actual mutation execution'})
    evidence_files = [path for path in output.rglob('*') if path.is_file()]
    payload_bytes = sum(path.stat().st_size for path in evidence_files)
    write(output / 'evidence-bounds.json', {'payload_bytes_excluding_bounds_record': payload_bytes,
        'maximum_expanded_artifact_bytes': scorecard.MAX_JSON, 'logs_omitted': False})
    require(sum(path.stat().st_size for path in output.rglob('*') if path.is_file()) <= scorecard.MAX_JSON,
            'full shard diagnostics exceed 64 MiB; retained without omitting logs, collection is invalid')
    produced = output / 'mutants.out/mutants.json'
    if produced.exists():
        require(identities(scorecard.load(produced)) == identities(assigned), 'executed mutant universe differs from assigned shard')
    return code


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    generate = sub.add_parser('plan')
    generate.add_argument('--output', required=True)
    generate.add_argument('--run-metadata')
    generate.add_argument('--repository')
    generate.add_argument('--github-output')
    generate.add_argument('--local-list-smoke', action='store_true')
    shard = sub.add_parser('shard')
    shard.add_argument('--plan', required=True)
    shard.add_argument('--group', choices=GROUPS, required=True)
    shard.add_argument('--shard', type=int, required=True)
    shard.add_argument('--output', required=True)
    shard.add_argument('--run-metadata', required=True)
    shard.add_argument('--repository', required=True)
    args = parser.parse_args()
    if args.command == 'plan':
        require(args.local_list_smoke or (args.run_metadata and args.repository), 'CI plan needs source metadata')
        plan_run(args)
    else:
        raise SystemExit(shard_run(args))


if __name__ == '__main__':
    main()
