#!/usr/bin/env python3
"""Selection, assignment and execution-order contracts for manual diagnostics."""
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from argparse import Namespace

import focused_mutation as focused


def mutants(count):
    return [{'package': 'lattice-core', 'file': 'crates/lattice-core/src/log.rs',
             'name': f'mutation {index}'} for index in range(count)]


class FocusedMutationTests(unittest.TestCase):
    def plan(self, full=None, selected=None, regex='', maximum=16):
        full = mutants(19) if full is None else full
        return focused.make_plan(full, full if selected is None else selected,
                                 'lattice-core', regex, maximum, {'revision': 'abc'})

    def test_defaults_cover_full_package_exactly_once(self):
        full = mutants(19)
        plan = self.plan(full)
        self.assertEqual(len(plan['matrix']['include']), 16)
        assigned = []
        for row in plan['matrix']['include']:
            selection = full[row['shard']::row['total']]
            focused.check_assignment(plan, row['shard'], selection)
            assigned.extend(focused.identities(selection))
        self.assertCountEqual(assigned, plan['full_ids'])
        self.assertEqual(len(assigned), len(set(assigned)))

    def test_fewer_mutants_never_make_empty_shards(self):
        for maximum in (1, 2, 4, 8, 16):
            plan = self.plan(mutants(3), maximum=maximum)
            self.assertEqual(len(plan['matrix']['include']), min(maximum, 3))
            self.assertTrue(all(row['count'] > 0 for row in plan['matrix']['include']))

    def test_filtered_scope_and_arguments(self):
        full = mutants(9)
        plan = self.plan(full, full[1::2], regex='append|queued', maximum=2)
        self.assertIn('diagnostic', plan['scope'])
        self.assertEqual(len(plan['full_ids']), 9)
        self.assertEqual(len(plan['selected_ids']), 4)
        self.assertEqual(plan['arguments'][-2:], ['-F', 'append|queued'])
        self.assertIn('--test-workspace=false', plan['arguments'])
        self.assertEqual(plan['arguments'][plan['arguments'].index('--timeout') + 1], '120')
        self.assertEqual(plan['arguments'][plan['arguments'].index('-j') + 1], '2')

    def test_invalid_selection_and_budgets_rejected(self):
        for full, selected, regex, maximum in [
                ([], [], '', 16), (mutants(3), [], 'missing', 16),
                (mutants(3), mutants(4), 'extra', 16),
                (mutants(3), list(reversed(mutants(3))), 'reverse', 16),
                (mutants(3), mutants(2), '', 16),
                (mutants(3), mutants(3), '', 3),
                (mutants(3), mutants(3), '', True),
                (mutants(2), mutants(2) * 2, 'duplicate', 16)]:
            with self.subTest(maximum=maximum, selected=selected):
                with self.assertRaises(ValueError):
                    self.plan(full, selected, regex, maximum)

    def test_missing_duplicate_wrong_and_out_of_bounds_assignments_rejected(self):
        plan = self.plan(mutants(3), maximum=2)
        for shard, selection in [(0, []), (0, mutants(3)), (1, mutants(1)),
                                 (-1, mutants(1)), (2, mutants(1)), (True, mutants(1))]:
            with self.subTest(shard=shard, selection=selection):
                with self.assertRaises(ValueError):
                    focused.check_assignment(plan, shard, selection)

    def test_invalid_package_is_not_passed_to_mutants(self):
        metadata = {'workspace_members': ['core'], 'packages': [
            {'name': 'lattice-core', 'id': 'core'}, {'name': 'external', 'id': 'external'}]}
        with patch.object(focused.subprocess, 'check_output', return_value=json.dumps(metadata)):
            focused.validate_package('lattice-core')
            for invalid in ('external', '--workspace', 'lattice-core; echo unsafe', ''):
                with self.assertRaises(ValueError):
                    focused.validate_package(invalid)

    def test_manifest_digest_and_provenance_tampering_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory)
            full = mutants(3)
            plan = self.plan(full, maximum=2)
            plan['digests'] = {}
            (out / 'assignments').mkdir()
            for name, value in [('full.json', full), ('selected.json', full),
                                ('assignments/0.json', full[::2]),
                                ('assignments/1.json', full[1::2])]:
                focused.write(out / name, value)
                plan['digests'][name] = focused.digest(out / name)
            focused.write(out / 'plan.json', plan)
            manifest = {'plan_sha256': focused.digest(out / 'plan.json'), 'source': plan['source'],
                        'tool_version': focused.VERSION, 'arguments': plan['arguments'],
                        'filter': plan['filter'], 'digests': plan['digests']}
            focused.write(out / 'manifest.json', manifest)
            with patch.object(focused, 'source', return_value=plan['source']):
                self.assertEqual(focused.load_plan(out), plan)
                with (out / 'full.json').open('a') as stream:
                    stream.write(' ')
                with self.assertRaises(ValueError):
                    focused.load_plan(out)
                focused.write(out / 'full.json', full)
                manifest['filter'] = 'tampered'
                focused.write(out / 'manifest.json', manifest)
                with self.assertRaises(ValueError):
                    focused.load_plan(out)
            focused.write(out / 'manifest.json', {**manifest, 'filter': ''})
            with patch.object(focused, 'source', return_value={'revision': 'other'}):
                with self.assertRaises(ValueError):
                    focused.load_plan(out)

    def test_baseline_failure_stops_mutation_and_success_runs_after_baseline(self):
        plan = self.plan(mutants(1), maximum=1)
        for baseline_status in (0, 17):
            with tempfile.TemporaryDirectory() as directory:
                args = Namespace(output=str(Path(directory) / 'result'), plan=directory, shard=0)
                with patch.object(focused, 'verify_tool'), \
                     patch.object(focused, 'validate_package'), \
                     patch.object(focused, 'load_plan', return_value=plan), \
                     patch.object(focused, 'digest', return_value='digest'), \
                     patch.object(focused, 'listing', return_value=mutants(1)), \
                     patch.object(focused, 'logged', side_effect=[baseline_status, 0]) as logged:
                    plan['digests'] = {}
                    self.assertEqual(focused.shard_run(args), baseline_status)
                    self.assertEqual(logged.call_args_list[0].args[0],
                                     ['cargo', 'test', '--package', 'lattice-core'])
                    self.assertEqual(logged.call_count, 2 if baseline_status == 0 else 1)

    def test_regeneration_drift_stops_before_tests(self):
        plan = self.plan(mutants(1), maximum=1)
        plan['digests'] = {}
        with tempfile.TemporaryDirectory() as directory:
            args = Namespace(output=str(Path(directory) / 'result'), plan=directory, shard=0)
            with patch.object(focused, 'verify_tool'), \
                 patch.object(focused, 'validate_package'), \
                 patch.object(focused, 'load_plan', return_value=plan), \
                 patch.object(focused, 'digest', return_value='digest'), \
                 patch.object(focused, 'listing', return_value=mutants(2)), \
                 patch.object(focused, 'logged') as logged:
                with self.assertRaises(ValueError):
                    focused.shard_run(args)
                logged.assert_not_called()


if __name__ == '__main__':
    unittest.main()
