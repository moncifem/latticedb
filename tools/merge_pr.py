#!/usr/bin/env python3
"""Merge reviewed PRs serially, fail closed on incomplete or stale GitHub evidence."""
import argparse
import datetime
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

FIELDS = 'state,headRefOid,headRefName,baseRefName,isDraft,mergeStateStatus,statusCheckRollup'
MINIMUM = {'gate (ubuntu-latest)', 'gate (ubuntu-24.04-arm)', 'secret-scan',
           'typos', 'commit-messages', 'msrv'}
FAILED = {'FAILURE', 'ERROR', 'TIMED_OUT', 'STARTUP_FAILURE', 'ACTION_REQUIRED', 'CANCELLED', 'STALE'}


class Unknown(Exception):
    pass


class Refused(Exception):
    pass


def gh(*args):
    try:
        result = subprocess.run(['gh', *args], capture_output=True, text=True, timeout=60, check=False)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise Unknown(str(error)) from error
    # A failing API may still print plausible JSON. Never accept that stdout.
    if result.returncode != 0:
        raise Unknown('gh command failed')
    return result.stdout


def api(*args):
    try:
        return json.loads(gh(*args))
    except (ValueError, TypeError) as error:
        raise Unknown('malformed JSON') from error


def required_contexts():
    value = json.loads(Path(__file__).with_name('branch-protection.json').read_text())
    contexts = value['required_status_checks']['contexts']
    if not isinstance(contexts, list) or not all(isinstance(x, str) and x for x in contexts) \
            or len(contexts) != len(set(contexts)) or not MINIMUM.issubset(contexts):
        raise ValueError('configured required contexts must include all six required checks')
    return set(contexts)


def check_time(value):
    if not isinstance(value, str) or not re.fullmatch(r'\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,6})?Z', value):
        raise Unknown('missing or malformed check chronology')
    try:
        result = datetime.datetime.fromisoformat(value.replace('Z', '+00:00'))
    except ValueError as error:
        raise Unknown('invalid check timestamp') from error
    if result.year < 1970:
        raise Unknown('unset check timestamp')
    return result


def latest_checks(checks, repo=None):
    """Order same-workflow Actions retries; never infer chronology for unrelated status writers."""
    grouped = {}
    for row in checks:
        if not isinstance(row, dict):
            raise Unknown('malformed check')
        kind = row.get('__typename')
        name = row.get('name') if kind == 'CheckRun' else row.get('context')
        if kind not in ('CheckRun', 'StatusContext') or not isinstance(name, str) or not name:
            raise Unknown('missing check identity')
        grouped.setdefault(name, []).append(row)
    selected = []
    for rows in grouped.values():
        if len(rows) == 1:
            selected.append(rows[0])
            continue
        # gh's StatusContext projection supplies no writer identity; createdAt alone cannot prove
        # that two legacy writers belong to the same check. Mixed record types are ambiguous too.
        if any(row['__typename'] != 'CheckRun' for row in rows):
            raise Unknown('ambiguous duplicate legacy/mixed check contexts')
        workflows = {row.get('workflowName') for row in rows if isinstance(row.get('workflowName'), str)}
        if len(workflows) != 1 or not next(iter(workflows), ''):
            raise Unknown('duplicate context has different or missing workflows')
        starts = []
        projects = set()
        for row in rows:
            if row.get('workflowName') not in workflows:
                raise Unknown('missing duplicate-check workflow identity')
            url = row.get('detailsUrl')
            match = re.fullmatch(r'https://github\.com/([^/]+/[^/]+)/actions/runs/[0-9]+/job/[0-9]+',
                                 url if isinstance(url, str) else '')
            if not match or (repo is not None and match.group(1) != repo):
                raise Unknown('duplicate context has unverifiable Actions provenance')
            projects.add(match.group(1))
            started = check_time(row.get('startedAt'))
            if row.get('status') == 'COMPLETED':
                if check_time(row.get('completedAt')) < started:
                    raise Unknown('check completed before it started')
            elif row.get('status') not in ('QUEUED', 'IN_PROGRESS', 'WAITING', 'PENDING', 'REQUESTED'):
                raise Unknown('unknown check lifecycle')
            starts.append(started)
        if len(projects) != 1 or len(set(starts)) != len(starts):
            raise Unknown('ambiguous check chronology or producer')
        # Start time identifies the newer invocation even if an older run finishes after it.
        selected.append(rows[starts.index(max(starts))])
    return selected


def assess(snapshot, required, repo=None):
    if not isinstance(snapshot, dict) or snapshot.get('state') not in ('OPEN', 'MERGED', 'CLOSED'):
        raise Unknown('unknown PR state')
    if snapshot['state'] == 'MERGED':
        return 'MERGED'
    if snapshot['state'] == 'CLOSED':
        raise Refused('closed without merging')
    head = snapshot.get('headRefOid')
    if not isinstance(head, str) or not re.fullmatch('[a-f0-9]{40}', head):
        raise Unknown('invalid head SHA')
    if type(snapshot.get('isDraft')) is not bool:
        raise Unknown('missing draft status')
    if snapshot['isDraft']:
        raise Refused('draft')
    if snapshot.get('baseRefName') != 'main':
        raise Refused('base must be main; retarget stacked PRs explicitly')
    if not isinstance(snapshot.get('headRefName'), str) or not snapshot['headRefName']:
        raise Unknown('missing branch')
    checks = snapshot.get('statusCheckRollup')
    if not isinstance(checks, list):
        raise Unknown('missing check rollup')
    observed = {}
    pending = False
    for row in latest_checks(checks, repo):
        if not isinstance(row, dict):
            raise Unknown('malformed check')
        if row.get('__typename') == 'CheckRun':
            name = row.get('name')
            status = row.get('status')
            conclusion = row.get('conclusion')
            if conclusion in FAILED:
                raise Refused('a check failed on the observed head')
            success = status == 'COMPLETED' and conclusion == 'SUCCESS'
            pending = pending or status != 'COMPLETED'
        elif row.get('__typename') == 'StatusContext':
            name = row.get('context')
            if row.get('state') in FAILED:
                raise Refused('a commit status failed on the observed head')
            success = row.get('state') == 'SUCCESS'
            pending = pending or row.get('state') not in ('SUCCESS', 'FAILURE', 'ERROR')
        else:
            raise Unknown('unknown check record type')
        if not isinstance(name, str) or not name:
            raise Unknown('missing check context')
        observed.setdefault(name, []).append(success)
    state = snapshot.get('mergeStateStatus')
    if state in ('DIRTY', 'DRAFT'):
        raise Refused('merge conflicts or draft')
    if state == 'BEHIND':
        return 'BEHIND'
    # Each configured context must appear exactly once with an explicit successful terminal state.
    if state != 'CLEAN' or pending or any(observed.get(name) != [True] for name in required):
        return 'WAIT'
    return 'CLEAN'


def snapshot(pr, repo):
    return api('pr', 'view', str(pr), '--repo', repo, '--json', FIELDS)


def children(repo, branch):
    value = api('pr', 'list', '--repo', repo, '--state', 'open', '--base', branch,
                '--json', 'number,baseRefName')
    if not isinstance(value, list) or any(not isinstance(row, dict)
            or type(row.get('number')) is not int or row['number'] <= 0
            or row.get('baseRefName') != branch for row in value):
        raise Unknown('malformed stacked-child query')
    return value


def protection_ready(repo, required):
    value = api('api', f'repos/{repo}/branches/main/protection')
    if not isinstance(value, dict):
        raise Unknown('malformed live protection')
    checks = value.get('required_status_checks')
    admins = value.get('enforce_admins')
    if not isinstance(checks, dict) or not isinstance(admins, dict):
        return False
    contexts = checks.get('contexts')
    return (checks.get('strict') is True and admins.get('enabled') is True
            and isinstance(contexts, list) and all(isinstance(name, str) for name in contexts)
            and required.issubset(contexts))


def protected_action(repo, required, dry_run):
    if protection_ready(repo, required):
        return True
    if dry_run:
        return False
    raise Refused('live strict/admin/required-context protection is not applied; owner decision required')


def merge_one(pr, repo, args, required):
    updated = set()
    stale_head = None
    for _ in range(args.tries):
        try:
            first = snapshot(pr, repo)
            state = assess(first, required, repo)
            if state == 'MERGED':
                print(f'PR {pr}: already merged')
                return True
            if state == 'BEHIND':
                if not protected_action(repo, required, args.dry_run):
                    print(f'PR {pr}: dry run, blocked by unapplied live protection')
                    return True
                if args.dry_run:
                    print(f'PR {pr}: dry run, would rebase branch and wait for new checks')
                    return True
                if first['headRefOid'] not in updated:
                    # No merge-commit fallback. Rebase refusal stops the queue.
                    try:
                        gh('pr', 'update-branch', str(pr), '--repo', repo, '--rebase')
                    except Unknown as error:
                        raise Refused('rebase update failed; no merge fallback') from error
                    updated.add(first['headRefOid'])
                    stale_head = first['headRefOid']
            elif state == 'CLEAN' and first['headRefOid'] != stale_head:
                # Retain branches, and refuse parents with open children before a merge that could
                # trigger repository-level automatic deletion. Never close/retarget children here.
                if children(repo, first['headRefName']):
                    raise Refused('open stacked children; retarget them explicitly before merging parent')
                second = snapshot(pr, repo)
                if assess(second, required, repo) != 'CLEAN' or second['headRefOid'] != first['headRefOid'] \
                        or second['headRefName'] != first['headRefName']:
                    raise Unknown('head or evidence changed during validation')
                if not protected_action(repo, required, args.dry_run):
                    print(f'PR {pr}: dry run, blocked by unapplied live protection')
                    return True
                if args.dry_run:
                    print(f'PR {pr}: dry run, would merge exact verified head {first["headRefOid"]}')
                    return True
                # Server-side expected-head check closes the final read/merge race.
                gh('pr', 'merge', str(pr), '--repo', repo, '--rebase',
                   '--match-head-commit', first['headRefOid'])
                after = snapshot(pr, repo)
                if after.get('state') == 'MERGED' and after.get('headRefOid') == first['headRefOid']:
                    print(f'PR {pr}: merged {first["headRefOid"]}')
                    return True
            elif args.dry_run:
                print(f'PR {pr}: dry run, would wait for complete successful required checks')
                return True
        except Refused as error:
            print(f'PR {pr}: refused, {error}')
            return False
        except Unknown:
            pass
        if args.sleep:
            time.sleep(args.sleep)
    print(f'PR {pr}: gave up without a verified result; not merged')
    return False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--dry-run', action='store_true')
    parser.add_argument('--tries', type=int, default=os.environ.get('MERGE_PR_TRIES', '90'))
    parser.add_argument('--sleep', type=int, default=os.environ.get('MERGE_PR_SLEEP', '20'))
    parser.add_argument('prs', type=int, nargs='+')
    args = parser.parse_args()
    if args.tries <= 0 or args.sleep < 0 or any(pr <= 0 for pr in args.prs):
        parser.error('positive tries/PR numbers and nonnegative sleep required')
    try:
        required = required_contexts()
    except (ValueError, KeyError, OSError, TypeError) as error:
        print(str(error), file=sys.stderr)
        return 1
    repo = None
    for _ in range(args.tries):
        try:
            value = api('repo', 'view', '--json', 'nameWithOwner')
            name = value.get('nameWithOwner') if isinstance(value, dict) else None
            if isinstance(name, str) and re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', name):
                repo = name
                break
        except Unknown:
            pass
        if args.sleep:
            time.sleep(args.sleep)
    if not repo:
        print('cannot determine repository', file=sys.stderr)
        return 1
    good = True
    for pr in args.prs:
        if not good:
            print(f'PR {pr}: skipped, an earlier PR was not merged')
        else:
            good = merge_one(pr, repo, args, required)
    return 0 if good else 1


if __name__ == '__main__':
    sys.exit(main())
