#!/usr/bin/env python3
"""Manual package diagnostics, deliberately outside weekly/global score ingestion."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

from mutation_campaign import VERSION, digest, identities, verify_tool, write
from scorecard_metrics import require


def command(package, regex=''):
    args = ['cargo', 'mutants', '-p', package, '--test-workspace=false', '--timeout', '120',
            '--no-shuffle', '-j', '2']
    return args + (['-F', regex] if regex else [])


def source():
    require(os.environ.get('GITHUB_ACTIONS') == 'true'
            and os.environ.get('GITHUB_EVENT_NAME') == 'workflow_dispatch',
            'planning/execution is restricted to remote manual workflow runs')
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    require(revision == os.environ['GITHUB_SHA'], 'checkout differs from workflow source SHA')
    return {'repository': os.environ['GITHUB_REPOSITORY'], 'run_id': os.environ['GITHUB_RUN_ID'],
            'attempt': os.environ['GITHUB_RUN_ATTEMPT'], 'revision': revision,
            'workflow': '.github/workflows/mutation-crate.yml'}


def validate_package(package):
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--no-deps', '--format-version', '1'], text=True))
    members = set(metadata['workspace_members'])
    require(package in [p['name'] for p in metadata['packages'] if p['id'] in members],
            'package must be an exact workspace package name')


def listing(package, regex, path, shard=None):
    args = command(package, regex) + ['--list', '--json']
    if shard is not None:
        args += ['--shard', f'{shard[0]}/{shard[1]}', '--sharding', 'round-robin']
    with path.open('wb') as stream:
        subprocess.run(args, stdout=stream, check=True)
    return json.loads(path.read_text())


def make_plan(full, selected, package, regex, maximum, origin):
    require(type(maximum) is int and maximum in (1, 2, 4, 8, 16), 'invalid maximum shards')
    full_ids, selected_ids = identities(full), identities(selected)
    require(full_ids and selected_ids, 'empty universe or selection')
    selected_set = set(selected_ids)
    require([i for i in full_ids if i in selected_set] == selected_ids,
            'selection is not an ordered subset of the full universe')
    require(regex or full_ids == selected_ids, 'unfiltered selection omitted mutants')
    total = min(maximum, len(selected_ids))
    return {'schema': 1, 'scope': 'filtered diagnostic; no global score' if regex else
            'single package; no global score', 'source': origin, 'package': package,
            'filter': regex, 'maximum_shards': maximum, 'tool_version': VERSION,
            'arguments': command(package, regex), 'full_ids': full_ids, 'selected_ids': selected_ids,
            'sharding': 'round-robin', 'matrix': {'include': [
                {'shard': i, 'total': total, 'count': len(selected_ids[i::total])}
                for i in range(total)]}}


def check_assignment(plan, shard, actual):
    total = len(plan['matrix']['include'])
    require(type(shard) is int and 0 <= shard < total, 'invalid shard')
    expected = plan['selected_ids'][shard::total]
    require(expected and identities(actual) == expected, 'regenerated assignment differs')


def plan_run(args):
    verify_tool()
    validate_package(args.package)
    out = Path(args.output)
    out.mkdir(parents=True, exist_ok=False)
    full = listing(args.package, '', out / 'full.json')
    selected = listing(args.package, args.regex, out / 'selected.json')
    plan = make_plan(full, selected, args.package, args.regex, args.maximum, source())
    plan['digests'] = {name: digest(out / name) for name in ('full.json', 'selected.json')}
    (out / 'assignments').mkdir()
    for row in plan['matrix']['include']:
        name = f'assignments/{row["shard"]}.json'
        actual = listing(args.package, args.regex, out / name, (row['shard'], row['total']))
        check_assignment(plan, row['shard'], actual)
        plan['digests'][name] = digest(out / name)
    write(out / 'plan.json', plan)
    write(out / 'manifest.json', {'plan_sha256': digest(out / 'plan.json'), 'source': plan['source'],
                                'tool_version': VERSION, 'arguments': plan['arguments'],
                                'filter': args.regex, 'digests': plan['digests']})
    with Path(os.environ['GITHUB_OUTPUT']).open('a') as stream:
        stream.write('matrix=' + json.dumps(plan['matrix'], separators=(',', ':')) + '\n')


def load_plan(directory):
    directory = Path(directory)
    plan = json.loads((directory / 'plan.json').read_text())
    manifest = json.loads((directory / 'manifest.json').read_text())
    require(digest(directory / 'plan.json') == manifest['plan_sha256'], 'plan digest differs')
    expected_files = {'full.json', 'selected.json'} | {
        f'assignments/{row["shard"]}.json' for row in plan['matrix']['include']}
    require(set(plan['digests']) == expected_files, 'missing or unexpected artifact digest')
    for name, expected in plan['digests'].items():
        require(digest(directory / name) == expected, 'planning artifact digest differs')
    full = json.loads((directory / 'full.json').read_text())
    selected = json.loads((directory / 'selected.json').read_text())
    expected = make_plan(full, selected, plan['package'], plan['filter'], plan['maximum_shards'], source())
    require({k: v for k, v in plan.items() if k != 'digests'} == expected,
            'plan identity, source, scope or assignment differs')
    require(manifest == {'plan_sha256': digest(directory / 'plan.json'), 'source': plan['source'],
                        'tool_version': VERSION, 'arguments': plan['arguments'],
                        'filter': plan['filter'], 'digests': plan['digests']}, 'manifest differs')
    for row in plan['matrix']['include']:
        check_assignment(plan, row['shard'], json.loads(
            (directory / f'assignments/{row["shard"]}.json').read_text()))
    return plan


def logged(args, directory, name):
    write(directory / (name + '-command.json'), {'arguments': args, 'status': 'started'})
    with (directory / (name + '.log')).open('wb') as stream:
        code = subprocess.run(args, stdout=stream, stderr=subprocess.STDOUT).returncode
    write(directory / (name + '-command.json'), {'arguments': args, 'exit_code': code})
    print((directory / (name + '.log')).read_text(errors='replace'))
    return code


def shard_run(args):
    out = Path(args.output)
    out.mkdir(parents=True, exist_ok=False)
    verify_tool()
    plan = load_plan(args.plan)
    validate_package(plan['package'])
    total = len(plan['matrix']['include'])
    mutation = plan['arguments'] + ['--shard', f'{args.shard}/{total}', '--sharding',
                                  'round-robin', '--output', str(out / 'mutants-out')]
    baseline = ['cargo', 'test', '--package', plan['package']]
    write(out / 'manifest.json', {'plan_sha256': digest(Path(args.plan) / 'plan.json'),
                                'source': plan['source'], 'tool_version': VERSION,
                                'filter': plan['filter'], 'shard': args.shard,
                                'scope': plan['scope'], 'digests': plan['digests'],
                                'arguments': mutation, 'baseline_arguments': baseline})
    full = listing(plan['package'], '', out / 'regenerated-full.json')
    selected = listing(plan['package'], plan['filter'], out / 'regenerated-selected.json')
    require(identities(full) == plan['full_ids'] and identities(selected) == plan['selected_ids'],
            'regenerated full universe or selected universe differs')
    assigned = listing(plan['package'], plan['filter'], out / 'assigned.json', (args.shard, total))
    check_assignment(plan, args.shard, assigned)
    code = logged(baseline, out, 'baseline')
    if code:
        return code
    return logged(mutation, out, 'mutation')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='mode', required=True)
    plan = sub.add_parser('plan')
    plan.add_argument('--package', required=True)
    plan.add_argument('--regex', default='')
    plan.add_argument('--maximum', type=int, default=16)
    plan.add_argument('--output', required=True)
    shard = sub.add_parser('shard')
    shard.add_argument('--plan', required=True)
    shard.add_argument('--shard', type=int, required=True)
    shard.add_argument('--output', required=True)
    args = parser.parse_args()
    return plan_run(args) if args.mode == 'plan' else shard_run(args)


if __name__ == '__main__':
    sys.exit(main())
