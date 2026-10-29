#!/usr/bin/env python3
"""Exercise the real merger entrypoint using fake gh JSON, including server-side races."""
import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import merge_pr

HEAD = 'a' * 40
NEXT = 'b' * 40


def clean(head=HEAD):
    return {'state': 'OPEN', 'headRefOid': head, 'headRefName': 'feature', 'baseRefName': 'main',
            'isDraft': False, 'mergeStateStatus': 'CLEAN', 'statusCheckRollup': [
                {'__typename': 'CheckRun', 'name': name, 'status': 'COMPLETED', 'conclusion': 'SUCCESS'}
                for name in sorted(merge_pr.MINIMUM)]}


FAKE = r'''
import json,os,sys
from pathlib import Path
path=Path(os.environ['FAKE_STATE'])
s=json.loads(path.read_text());args=sys.argv[1:];s.setdefault('calls',[]).append(args)
def finish(value,rc=0):
 path.write_text(json.dumps(s)); print(json.dumps(value));sys.exit(rc)
def response(key,default):
 values=s.get(key,[default]);value=values.pop(0) if len(values)>1 else values[0];s[key]=values
 if isinstance(value,dict) and 'api_rc' in value: finish(value.get('stdout'),value['api_rc'])
 return value
if args[:1]==['api']:
 finish(response('protection',{'required_status_checks':{'strict':True,'contexts':['gate (ubuntu-latest)','gate (ubuntu-24.04-arm)','secret-scan','typos','commit-messages','msrv']},'enforce_admins':{'enabled':True}}))
if args[:2]==['repo','view']: finish(response('repos',{'nameWithOwner':'example/repo'}))
if args[:2]==['pr','view']: finish(response('views',{}))
if args[:2]==['pr','list']: finish(response('children',[]))
if args[:2]==['pr','update-branch']:
 if '--rebase' not in args: finish({},9)
 if s.get('update_rc',0): finish({},s['update_rc'])
 s['views']=s['after_update'];finish({})
if args[:2]==['pr','merge']:
 if '--delete-branch' in args or '--match-head-commit' not in args: finish({},8)
 expected=args[args.index('--match-head-commit')+1]
 if expected!=s.get('server_head','a'*40): finish({'error':'head changed'},1)
 s['views']=[{'state':'MERGED','headRefOid':expected}];finish({})
finish({},10)
'''


class MergerTests(unittest.TestCase):
    def invoke(self, scenario, *extra):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fake = root / 'gh'
            fake.write_text('#!' + sys.executable + '\n' + FAKE)
            fake.chmod(0o755)
            state = root / 'state.json'
            state.write_text(json.dumps(scenario))
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH'], FAKE_STATE=str(state),
                       PYTHONDONTWRITEBYTECODE='1')
            process = subprocess.run(['/bin/bash', str(Path(__file__).with_name('merge-pr.sh')),
                                      '--tries', '3', '--sleep', '0', *extra], env=env,
                                     capture_output=True, text=True, timeout=20, check=False)
            return process, json.loads(state.read_text())['calls']

    def merges(self, calls):
        return [args for args in calls if args[:2] == ['pr', 'merge']]

    def test_complete_json_passes_and_merge_is_exact_head_without_branch_deletion(self):
        result, calls = self.invoke({'views': [clean()]}, '1')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(len(self.merges(calls)), 1)
        self.assertIn('--match-head-commit', self.merges(calls)[0])
        self.assertEqual(self.merges(calls)[0][-1], HEAD)
        self.assertNotIn('--delete-branch', self.merges(calls)[0])

    def test_legacy_success_status_contexts_are_supported(self):
        value = clean()
        value['statusCheckRollup'] = [{'__typename': 'StatusContext', 'context': n, 'state': 'SUCCESS'}
                                    for n in merge_pr.MINIMUM]
        result, calls = self.invoke({'views': [value]}, '1')
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(len(self.merges(calls)), 1)

    def test_clean_empty_missing_pending_skipped_or_duplicate_contexts_never_merge(self):
        for damage in ('empty', 'missing', 'pending', 'skipped', 'duplicate', 'null'):
            value = clean()
            if damage == 'empty': value['statusCheckRollup'] = []
            elif damage == 'missing': value['statusCheckRollup'].pop()
            elif damage == 'pending': value['statusCheckRollup'][0]['status'] = 'IN_PROGRESS'
            elif damage == 'skipped': value['statusCheckRollup'][0]['conclusion'] = 'SKIPPED'
            elif damage == 'duplicate': value['statusCheckRollup'].append(value['statusCheckRollup'][0])
            else: value['statusCheckRollup'] = None
            result, calls = self.invoke({'views': [value]}, '1')
            self.assertNotEqual(result.returncode, 0, damage)
            self.assertFalse(self.merges(calls), damage)

    def test_draft_bad_sha_bad_base_and_failed_optional_check_refuse(self):
        for field, replacement in [('isDraft', True), ('isDraft', None), ('headRefOid', 'aaa'),
                                   ('baseRefName', 'parent'), ('mergeStateStatus', 'DIRTY')]:
            value = clean(); value[field] = replacement
            result, calls = self.invoke({'views': [value]}, '1')
            self.assertNotEqual(result.returncode, 0, field)
            self.assertFalse(self.merges(calls), field)
        value = clean()
        value['statusCheckRollup'].append({'__typename': 'StatusContext', 'context': 'optional', 'state': 'FAILURE'})
        result, calls = self.invoke({'views': [value]}, '1')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.merges(calls))

    def test_head_push_between_snapshots_cannot_merge_unverified_head(self):
        changed = clean(NEXT)
        changed['statusCheckRollup'] = []
        result, calls = self.invoke({'views': [clean(), changed]}, '1')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.merges(calls))

    def test_head_push_after_final_read_is_rejected_by_server_expected_sha(self):
        result, calls = self.invoke({'views': [clean()], 'server_head': NEXT}, '1')
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.merges(calls))
        self.assertTrue(all(call[-1] == HEAD for call in self.merges(calls)))
        self.assertNotIn('merged ' + HEAD, result.stdout)

    def test_nonzero_api_with_plausible_json_is_never_accepted(self):
        for key, stdout in [('views', clean()), ('repos', {'nameWithOwner': 'example/repo'}),
                            ('children', []), ('protection', {})]:
            scenario = {'views': [clean()], key: [{'api_rc': 1, 'stdout': stdout}]}
            result, calls = self.invoke(scenario, '1')
            self.assertNotEqual(result.returncode, 0, key)
            self.assertFalse(self.merges(calls), key)

    def test_transient_api_error_retries_with_a_fresh_complete_snapshot(self):
        result, calls = self.invoke({'views': [{'api_rc': 1, 'stdout': clean()}, clean()]}, '1')
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(len(self.merges(calls)), 1)

    def test_rebase_failure_does_not_fall_back_to_merge_update(self):
        value = clean(); value['mergeStateStatus'] = 'BEHIND'
        result, calls = self.invoke({'views': [value], 'update_rc': 1}, '1')
        self.assertNotEqual(result.returncode, 0)
        updates = [c for c in calls if c[:2] == ['pr', 'update-branch']]
        self.assertEqual(len(updates), 1)
        self.assertIn('--rebase', updates[0])
        self.assertFalse(self.merges(calls))

    def test_rebase_waits_for_new_head_and_new_required_gates(self):
        value = clean(); value['mergeStateStatus'] = 'BEHIND'
        result, calls = self.invoke({'views': [value], 'after_update': [clean(), clean(NEXT)],
                                    'server_head': NEXT}, '1')
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertTrue(all(c[-1] == NEXT for c in self.merges(calls)))

    def test_open_stacked_children_refuse_parent_without_any_mutation(self):
        result, calls = self.invoke({'views': [clean()],
                                    'children': [[{'number': 2, 'baseRefName': 'feature'}]]}, '1')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('stacked children', result.stdout)
        self.assertFalse(self.merges(calls))
        self.assertFalse(any(c[:2] == ['pr', 'edit'] for c in calls))

    def test_dry_run_is_read_only_and_failure_stops_serial_queue(self):
        result, calls = self.invoke({'views': [clean()]}, '--dry-run', '1')
        self.assertEqual(result.returncode, 0)
        self.assertFalse(self.merges(calls))
        value = clean(); value['isDraft'] = True
        result, calls = self.invoke({'views': [value]}, '1', '2')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('PR 2: skipped', result.stdout)
        self.assertFalse(any(c[:3] == ['pr', 'view', '2'] for c in calls))

    def test_unapplied_or_incomplete_live_protection_blocks_updates_and_merges(self):
        for mode in ('CLEAN', 'BEHIND'):
            for damage in ('strict', 'admins', 'contexts', 'missing'):
                live = {'required_status_checks': {'strict': True, 'contexts': sorted(merge_pr.MINIMUM)},
                        'enforce_admins': {'enabled': True}}
                if damage == 'strict': live['required_status_checks']['strict'] = False
                elif damage == 'admins': live['enforce_admins']['enabled'] = False
                elif damage == 'contexts': live['required_status_checks']['contexts'].pop()
                else: live = {}
                value = clean(); value['mergeStateStatus'] = mode
                result, calls = self.invoke({'views': [value], 'protection': [live]}, '1')
                self.assertNotEqual(result.returncode, 0, (mode, damage))
                self.assertFalse(self.merges(calls))
                self.assertFalse(any(c[:2] == ['pr', 'update-branch'] for c in calls))
                self.assertFalse(any('-X' in c or '--method' in c for c in calls))
        result, calls = self.invoke({'views': [clean()], 'protection': [{}]}, '--dry-run', '1')
        self.assertEqual(result.returncode, 0)
        self.assertIn('blocked by unapplied live protection', result.stdout)
        self.assertFalse(self.merges(calls))

    def actual_history(self):
        path = Path(__file__).with_name('fixtures') / 'pr288-check-history.json'
        return json.loads(path.read_text())

    def test_actual_pr288_raw_history_selects_later_reviewed_runs(self):
        snapshot = self.actual_history()
        self.assertEqual(snapshot['headRefOid'], '7d835be29bc6d28ffba387090160346cc0286e21')
        selected = {row.get('name'): row for row in merge_pr.latest_checks(
            snapshot['statusCheckRollup'], 'moncifem/latticedb')}
        for name, job in [('risk-gate', '123649261808'), ('claims', '123649262673')]:
            self.assertEqual(selected[name]['conclusion'], 'SUCCESS')
            self.assertEqual(selected[name]['detailsUrl'],
                'https://github.com/moncifem/latticedb/actions/runs/40931051075/job/' + job)
        # Preserve the real pending x86 gate: selecting retry history must not create a green PR.
        self.assertEqual(merge_pr.assess(snapshot, merge_pr.MINIMUM, 'moncifem/latticedb'), 'WAIT')

    def governed_snapshot(self):
        value = clean()
        value['statusCheckRollup'].extend(copy.deepcopy([
            row for row in self.actual_history()['statusCheckRollup']
            if row.get('name') in ('risk-gate', 'claims')]))
        return value

    def test_actual_governance_history_does_not_poison_later_successful_gates(self):
        value = self.governed_snapshot()
        self.assertEqual(merge_pr.assess(value, merge_pr.MINIMUM, 'moncifem/latticedb'), 'CLEAN')
        # An older execution finishing late must not replace the later invocation.
        old = next(row for row in value['statusCheckRollup'] if row.get('conclusion') == 'FAILURE')
        old['completedAt'] = '2026-10-29T19:00:00Z'
        self.assertEqual(merge_pr.assess(value, merge_pr.MINIMUM, 'moncifem/latticedb'), 'CLEAN')

    def test_latest_pending_or_failed_governance_run_never_uses_older_pass(self):
        for outcome in ('pending', 'failed'):
            value = self.governed_snapshot()
            latest = copy.deepcopy(next(row for row in merge_pr.latest_checks(value['statusCheckRollup'])
                                        if row['name'] == 'risk-gate'))
            latest['startedAt'] = '2026-10-29T19:00:00Z'
            latest['completedAt'] = '2026-10-29T19:00:01Z'
            if outcome == 'pending':
                latest.update(status='IN_PROGRESS', conclusion='', completedAt='0001-01-01T00:00:00Z')
                value['statusCheckRollup'].append(latest)
                self.assertEqual(merge_pr.assess(value, merge_pr.MINIMUM, 'moncifem/latticedb'), 'WAIT')
            else:
                latest['conclusion'] = 'FAILURE'
                value['statusCheckRollup'].append(latest)
                with self.assertRaises(merge_pr.Refused):
                    merge_pr.assess(value, merge_pr.MINIMUM, 'moncifem/latticedb')

    def test_ambiguous_retry_identity_or_chronology_is_unknown(self):
        for damage in ('tied', 'missing_start', 'missing_finish', 'backward', 'workflow', 'app', 'repository'):
            value = self.governed_snapshot()
            history = [r for r in value['statusCheckRollup'] if r.get('name') == 'risk-gate']
            old, latest = history[0], history[-1]
            if damage == 'tied': old['startedAt'] = latest['startedAt']
            elif damage == 'missing_start': old.pop('startedAt')
            elif damage == 'missing_finish': old.pop('completedAt')
            elif damage == 'backward': old['completedAt'] = '2026-10-29T18:00:00Z'
            elif damage == 'workflow': old['workflowName'] = 'other-app'
            elif damage == 'app': old['detailsUrl'] = 'https://external.example/check/1'
            else: old['detailsUrl'] = 'https://github.com/other/repo/actions/runs/1/job/2'
            with self.assertRaises(merge_pr.Unknown, msg=damage):
                merge_pr.assess(value, merge_pr.MINIMUM, 'moncifem/latticedb')

    def test_duplicate_legacy_writers_or_mixed_types_cannot_be_ordered_by_timestamp_alone(self):
        for mixed in (False, True):
            value = clean()
            name = next(iter(merge_pr.MINIMUM))
            if not mixed:
                value['statusCheckRollup'] = [r for r in value['statusCheckRollup'] if r['name'] != name]
            for timestamp in ('2026-10-29T18:00:00Z', '2026-10-29T19:00:00Z'):
                value['statusCheckRollup'].append({'__typename': 'StatusContext', 'context': name,
                                                 'state': 'SUCCESS', 'createdAt': timestamp})
            with self.assertRaises(merge_pr.Unknown): merge_pr.assess(value, merge_pr.MINIMUM)

    def test_missing_configuration_cannot_remove_the_six_context_minimum(self):
        required = merge_pr.required_contexts()
        self.assertTrue(merge_pr.MINIMUM.issubset(required))
        value = clean()
        self.assertEqual(merge_pr.assess(value, required | {'new-required'}), 'WAIT')


if __name__ == '__main__':
    unittest.main()
