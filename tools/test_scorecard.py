"""Evidence semantics, independent of Cargo/network and long nightly workloads."""
import json
import tempfile
from types import SimpleNamespace
import unittest
from pathlib import Path

import scorecard


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


@unittest.skipUnless((scorecard.ROOT / 'target/debug/lattice').exists(), 'build lattice-cli for real ledger integration')
class RealLedgerTests(unittest.TestCase):
    binary = str(scorecard.ROOT / 'target/debug/lattice')

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


if __name__ == '__main__':
    unittest.main()
