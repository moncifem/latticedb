"""Evidence semantics, independent of Cargo/network and long nightly workloads."""
import json
import copy
import os
import zipfile
import tempfile
from types import SimpleNamespace
import unittest
from pathlib import Path

import scorecard
import scorecard_metrics as metrics

BINARY = Path(os.environ.get("LATTICE_SCORECARD_BIN", str(scorecard.ROOT / "target/debug/lattice")))


def entry(kind, value):
    return {'kind': kind, 'json': json.dumps(value), 'lsn': 1, 'recorded_ms': 42}


class EvidenceTests(unittest.TestCase):
    def test_empty_report_exposes_every_missing_measure_without_signoff(self):
        value = scorecard.report([])
        self.assertEqual(len(value['sections']), 10)
        for kind in ('engineering_health', 'simulation', 'fuzz', 'mutation', 'eval_gates', 'benchmarks', 'dogfood'):
            self.assertEqual(value['sections'][kind]['status'], 'missing')
        self.assertIn('unsigned', value['review_status'])
        self.assertIsNone(value['sections']['north_star_readiness']['observations'][0]['passing_of_8'])

    def test_legacy_measurements_are_preserved_without_invented_verdict(self):
        value = scorecard.report([entry('bench', {'profile': 'old', 'ns_per_op': {'x': 42}})])
        observed = value['sections']['benchmarks']['observations'][0]
        self.assertNotIn('verdict', observed)
        self.assertTrue(value['sections']['benchmarks']['missing'])

    def test_duplicate_run_attempt_job_record_is_counted_once(self):
        value = {'source': {'key': 'repo/1/2/3/sim/1'}, 'runs': 20, 'failing': 0, 'incomplete': 2}
        rows, invalid = scorecard.normalized([entry('sim', value)] * 2)
        self.assertEqual(len(rows), 1)
        self.assertEqual(invalid, [])
        changed = dict(value, runs=21)
        _, invalid = scorecard.normalized([entry('sim', value), entry('sim', changed)])
        self.assertEqual(invalid, ['conflicting duplicate source key'])

    def test_malformed_counts_cannot_become_simulation_health(self):
        value = scorecard.report([entry('sim', {'runs': True, 'failing': 0, 'incomplete': 0}),
                                  entry('sim', {'runs': 1, 'failing': 0, 'incomplete': 2}),
                                  {'kind': 'sim', 'json': 'broken'}])
        self.assertEqual(value['sections']['simulation']['status'], 'missing')
        self.assertEqual(len(value['invalid_evidence']), 3)

    def test_only_successful_real_fuzz_completion_counts(self):
        log = '#123 pulse cov: 42\nDone 123 runs in 601 second(s)\n'
        self.assertEqual(scorecard.completed_fuzz(log, 'success'), {'runs': 123, 'seconds': 601})
        for conclusion in ('cancelled', 'failure', 'timed_out', None):
            self.assertIsNone(scorecard.completed_fuzz(log, conclusion))
        self.assertIsNone(scorecard.completed_fuzz('#123 pulse\n', 'success'))
        self.assertIsNone(scorecard.completed_fuzz('Done 0 runs in 600 second(s)\n', 'success'))
        self.assertIsNone(scorecard.completed_fuzz(log + log, 'success'))

    def test_json_read_is_bounded(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'evidence.json'
            path.write_text(' ' * 20)
            original = scorecard.MAX_JSON
            try:
                scorecard.MAX_JSON = 10
                with self.assertRaisesRegex(ValueError, 'exceeds'):
                    scorecard.load(path)
            finally:
                scorecard.MAX_JSON = original


    def test_retained_history_producer_must_match_workflow_repo_branch_and_completed_run(self):
        previous = {'id': 10, 'run_attempt': 1, 'workflow_id': 2,
                    'head_sha': 'a' * 40, 'head_branch': 'main', 'status': 'completed',
                    'event': 'schedule', 'path': '.github/workflows/nightly.yml',
                    'repository': {'full_name': 'owner/repo'},
                    'head_repository': {'full_name': 'owner/repo'}, 'html_url': 'https://example.test/run/10'}
        current = dict(previous, id=11, status='in_progress')
        self.assertEqual(scorecard.validate_history_origin(previous, current, 'owner/repo', 'main', '20')['run_id'], 10)
        for changes in ({'workflow_id': 3}, {'path': '.github/workflows/other.yml'},
                        {'head_repository': {'full_name': 'fork/repo'}}, {'head_branch': 'other'},
                        {'event': 'pull_request'}, {'status': 'in_progress'}, {'head_sha': 'bad'}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                scorecard.validate_history_origin(dict(previous, **changes), current, 'owner/repo', 'main', '20')


@unittest.skipUnless(BINARY.exists(), 'build lattice-cli for real ledger integration')
class RealLedgerTests(unittest.TestCase):
    binary = str(BINARY)

    def test_ci_ingestion_uses_real_ledger_and_deduplicates_retries(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            artifact = root / 'observations/evidence-sim-clean-0'
            scorecard.record(self.binary, artifact, 'sim', {'runs': 20, 'failing': 0, 'incomplete': 1})
            jobs = [{'id': 1, 'name': 'sim (clean, 0)', 'status': 'completed', 'conclusion': 'success',
                     'html_url': 'https://example.test/job/1'},
                    {'id': 2, 'name': 'fuzz (wal_record)', 'status': 'completed', 'conclusion': 'cancelled',
                     'html_url': 'https://example.test/job/2'}]
            fuzz = root / 'observations/evidence-fuzz-wal_record'
            fuzz.mkdir(parents=True)
            (fuzz / 'fuzz.log').write_text('Done 123 runs in 601 second(s)\n')
            (root / 'jobs.json').write_text(json.dumps({'jobs': jobs}))
            args = SimpleNamespace(history=None, history_provenance=None, ledger=root / 'ledger', lattice=self.binary,
                                   jobs=root / 'jobs.json', artifacts=root / 'observations', repository='owner/repo',
                                   run='7', attempt='1', revision='a' * 40)
            scorecard.ingest(args)
            first = scorecard.export_ledger(args.ledger, self.binary)
            self.assertEqual(len(first), 3)  # two job statuses and one actual simulation
            scorecard.ingest(args)
            second = scorecard.export_ledger(args.ledger, self.binary)
            self.assertEqual(first, second)
            value = scorecard.report(second)
            self.assertEqual(value['observed_totals']['simulation_runs'], 20)
            self.assertEqual(value['sections']['fuzz']['status'], 'missing')
            self.assertEqual(value['invalid_evidence'], [])

    def test_snapshot_rejects_repair_and_never_modifies_original(self):
        with tempfile.TemporaryDirectory() as tmp:
            scorecard.record(self.binary, tmp, 'sim', {'runs': 1, 'failing': 0, 'incomplete': 0})
            segment = sorted((Path(tmp) / 'ledger').glob('*.seg'))[-1]
            original = segment.read_bytes() + b'torn tail'
            segment.write_bytes(original)
            with self.assertRaises(ValueError):
                scorecard.export_ledger(tmp, self.binary)
            self.assertEqual(segment.read_bytes(), original)



def mutation_fixture(summaries, planned_count=None, baseline='Success', ended=True):
    # Schema fixtures, never project observations: checked against cargo-mutants27 serialization.
    planned = [{'package': 'test-only', 'file': 'src/module.rs', 'name': f'mutant {n}',
                'function': None, 'span': {'start': {'line': n + 1, 'column': 1},
                                          'end': {'line': n + 1, 'column': 2}},
                'replacement': 'false', 'genre': 'FnValue', 'diff': 'test fixture'}
               for n in range(planned_count if planned_count is not None else len(summaries))]
    scenarios = [{'scenario': 'Baseline', 'summary': baseline}]
    for mutant, summary in zip(planned, summaries):
        scenario = {k: v for k, v in mutant.items() if k != 'diff'}
        scenarios.append({'scenario': {'Mutant': scenario}, 'summary': summary})
    result = {'outcomes': scenarios, 'total_mutants': len(summaries),
              'cargo_mutants_version': '27.1.0', 'start_time': '2026-10-29T00:00:00Z',
              'end_time': '2026-10-29T00:01:00Z' if ended else None}
    for field, summary in [('caught', 'CaughtMutant'), ('missed', 'MissedMutant'), ('timeout', 'Timeout'),
                           ('unviable', 'Unviable'), ('success', 'Success')]:
        result[field] = summaries.count(summary)
    return result, planned


def eval_fixture():
    return {'schema': 2, 'suite': 'test-only-suite', 'provider': 'test-only-provider', 'dataset_hash': 'a' * 64,
            'dataset': {'name': 'test-only-data', 'version': 1, 'split': 'test', 'hash': 'a' * 64, 'examples': 100,
                        'provenance': 'unit test fixture; not a project eval result'},
            'passed': True, 'metrics': [{'metric': 'f1', 'value': .9, 'ci_lo': .79, 'ci_hi': .95, 'n': 100,
                                       'bound': .8, 'confidence': .95, 'tolerance': .02,
                                       'passed': True, 'reason': ''}]}


def weekly_fixture():
    run = {'id': 1, 'run_attempt': 1, 'workflow_id': 2, 'head_sha': 'a' * 40,
           'head_branch': 'main', 'status': 'completed', 'event': 'schedule',
           'path': '.github/workflows/weekly.yml', 'repository': {'full_name': 'owner/repo'},
           'head_repository': {'full_name': 'owner/repo'}, 'conclusion': 'failure'}
    job = {'id': 3, 'run_id': 1, 'run_attempt': 1, 'head_sha': 'a' * 40,
           'name': 'mutation', 'status': 'completed', 'conclusion': 'failure', 'html_url': 'https://example.test/job/3'}
    artifact = {'id': 4, 'name': 'mutants', 'expired': False,
                'workflow_run': {'id': 1, 'head_sha': 'a' * 40, 'head_branch': 'main'}}
    workflow = {'id': 2, 'path': '.github/workflows/weekly.yml'}
    return run, job, artifact, workflow


class MetricEvidenceTests(unittest.TestCase):
    def test_mutation_denominator_and_all_outcome_counts_are_explicit(self):
        raw, planned = mutation_fixture(['CaughtMutant', 'CaughtMutant', 'MissedMutant', 'Timeout', 'Unviable'])
        row, = metrics.mutation_rows(raw, planned, 'failure')
        self.assertEqual(row['score_denominator'], 3)
        self.assertAlmostEqual(row['mutation_score'], 2 / 3)
        self.assertEqual(row['counts'], {'planned': 5, 'tested': 5, 'unrun': 0, 'killed': 2,
                                        'survived': 1, 'timeout': 1, 'unviable': 1, 'unclassified': 0})
        self.assertEqual(row['score_status'], 'failed')
        self.assertEqual(metrics.validate_record('mutation', row), row)

    def test_partial_cancelled_or_failed_baseline_never_has_passing_score(self):
        for raw, planned, conclusion in [(*mutation_fixture(['CaughtMutant'], 3, ended=False), 'failure'),
                                         (*mutation_fixture(['CaughtMutant']), 'cancelled'),
                                         (*mutation_fixture(['CaughtMutant'], baseline='Failure'), 'failure')]:
            row, = metrics.mutation_rows(raw, planned, conclusion)
            self.assertIsNone(row['mutation_score'])
            self.assertEqual(row['score_status'], 'unverified')
        raw, planned = mutation_fixture(['Timeout'])
        row, = metrics.mutation_rows(raw, planned, 'failure')
        self.assertIsNone(row['mutation_score'])
        self.assertEqual(row['score_denominator'], 0)

    def test_mutation_schema_counter_and_duplicate_mismatches_are_rejected(self):
        raw, planned = mutation_fixture(['CaughtMutant'])
        for field, value in [('caught', True), ('caught', 2), ('total_mutants', 2), ('cargo_mutants_version', '99.0.0')]:
            with self.subTest(field=field), self.assertRaises(ValueError):
                metrics.mutation_rows(dict(raw, **{field: value}), planned, 'success')
        with self.assertRaises(ValueError):
            metrics.mutation_rows(raw, planned + planned, 'success')
        bad = copy.deepcopy(raw)
        bad['outcomes'][-1]['summary'] = 'Unknown'
        with self.assertRaises(ValueError):
            metrics.mutation_rows(bad, planned, 'success')

    def test_exact_eval_gate_fields_and_provenance_are_preserved(self):
        value = metrics.eval_row(eval_fixture())
        self.assertEqual(value['verification_status'], 'verified_gate_fields')
        self.assertTrue(value['gate_verdict'])
        self.assertEqual(value['metrics'][0]['confidence'], .95)
        self.assertEqual(value['dataset']['provenance'], 'unit test fixture; not a project eval result')
        raw = eval_fixture()
        raw['metrics'][0].update(value=.80000004, ci_lo=.80000003, bound=.800000035, tolerance=0)
        self.assertFalse(raw['metrics'][0]['ci_lo'] >= raw['metrics'][0]['bound'])
        raw['metrics'][0]['passed'] = raw['passed'] = False
        self.assertFalse(metrics.eval_row(raw)['gate_verdict'])

    def test_legacy_eval_claim_is_explicitly_unverified(self):
        raw = eval_fixture()
        del raw['schema'], raw['dataset'], raw['metrics'][0]['confidence'], raw['metrics'][0]['tolerance']
        value = metrics.eval_row(raw)
        self.assertEqual(value['verification_status'], 'unverified_legacy_report')
        self.assertIsNone(value['gate_verdict'])

    def test_inconsistent_eval_gates_and_unknown_schema_are_rejected(self):
        for mutate in (lambda r: r.update(passed=False),
                       lambda r: r['metrics'][0].update(passed=False),
                       lambda r: r['metrics'][0].update(n=0),
                       lambda r: r['metrics'][0].update(confidence=1),
                       lambda r: r['metrics'][0].update(tolerance=-1),
                       lambda r: r['metrics'][0].update(ci_lo=.96),
                       lambda r: r.update(schema=99),
                       lambda r: r['dataset'].update(hash='b' * 64)):
            raw = eval_fixture()
            mutate(raw)
            with self.assertRaises(ValueError):
                metrics.eval_row(raw)

    def test_invalid_numeric_types_do_not_become_metric_evidence(self):
        raw, planned = mutation_fixture(['CaughtMutant'])
        row, = metrics.mutation_rows(raw, planned, 'success')
        with self.assertRaises(ValueError):
            metrics.validate_record('mutation', dict(row, mutation_score=True))
        with self.assertRaises(ValueError):
            metrics.validate_record('mutation', dict(row, score_denominator=True))
        evaluate = eval_fixture()
        evaluate['metrics'][0]['value'] = 10 ** 1000
        with self.assertRaises(ValueError):
            metrics.eval_row(evaluate)
        evaluate = eval_fixture()
        evaluate['schema'] = True
        with self.assertRaises(ValueError):
            metrics.eval_row(evaluate)

    def test_weekly_trust_boundary_requires_exact_workflow_repository_ref_and_attempt(self):
        run, job, artifact, workflow = weekly_fixture()
        origin = metrics.validate_weekly_origin(run, job, artifact, workflow, 'owner/repo', 'main')
        self.assertEqual(origin['authentication'], 'legacy_unambiguous_attempt_one')
        self.assertEqual(origin['run_conclusion'], 'failure')
        for field, value in [('workflow_id', 99), ('head_branch', 'other'), ('status', 'in_progress'),
                             ('head_repository', {'full_name': 'fork/repo'}), ('run_attempt', 2)]:
            with self.assertRaises(ValueError):
                metrics.validate_weekly_origin(dict(run, **{field: value}), job, artifact, workflow, 'owner/repo', 'main')
        manifest = {key: run[key] for key in ('id', 'run_attempt', 'head_sha', 'workflow_id', 'path', 'head_branch')}
        self.assertEqual(metrics.validate_weekly_origin(run, job, artifact, workflow, 'owner/repo', 'main', manifest)['authentication'],
                         'exact_producer_manifest')
        manifest['run_attempt'] = 2
        with self.assertRaises(ValueError):
            metrics.validate_weekly_origin(run, job, artifact, workflow, 'owner/repo', 'main', manifest)

    def test_real_schema_zip_import_binds_module_and_raw_file_hashes(self):
        raw, planned = mutation_fixture(['CaughtMutant', 'MissedMutant'])
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'mutants.zip'
            with zipfile.ZipFile(path, 'w') as archive:
                archive.writestr('mutants/mutants.out/outcomes.json', json.dumps(raw))
                archive.writestr('mutants/mutants.out/mutants.json', json.dumps(planned))
            values = metrics.import_mutation_archive(path, *weekly_fixture(), 'owner/repo', 'main')
            self.assertEqual(len(values), 1)
            self.assertEqual(values[0]['source']['artifact_id'], 4)
            self.assertRegex(values[0]['source']['outcomes_sha256'], r'^[a-f0-9]{64}$')
            self.assertEqual(values[0]['counts']['survived'], 1)

    def test_invalid_stored_metric_is_missing_and_diagnostic_not_a_pass(self):
        raw = eval_fixture()
        raw['passed'] = False
        value = scorecard.report([entry('eval', raw)])
        self.assertEqual(value['sections']['eval_gates']['status'], 'missing')
        self.assertIn('suite verdict', value['invalid_evidence'][0])

    @unittest.skipUnless(BINARY.exists(), 'build lattice-cli for real ledger integration')
    def test_metric_evidence_roundtrips_and_deduplicates_in_real_lattice(self):
        raw, planned = mutation_fixture(['CaughtMutant'])
        mutation, = metrics.mutation_rows(raw, planned, 'success')
        mutation['source'] = {'key': 'owner/repo/1/1/3/mutation/unit-test'}
        evaluate = metrics.eval_row(eval_fixture())
        evaluate['source'] = {'key': 'owner/repo/1/1/3/eval/unit-test'}
        with tempfile.TemporaryDirectory() as ledger:
            for _ in range(2):
                metrics.append_unique(str(BINARY), ledger, 'mutation', [mutation])
                metrics.append_unique(str(BINARY), ledger, 'eval', [evaluate])
            entries = scorecard.export_ledger(ledger, str(BINARY))
            self.assertEqual(len(entries), 2)
            value = scorecard.report(entries)
            self.assertEqual(value['invalid_evidence'], [])
            self.assertEqual(value['sections']['mutation']['status'], 'observed')
            self.assertEqual(value['sections']['eval_gates']['status'], 'observed')


if __name__ == '__main__':
    unittest.main()
