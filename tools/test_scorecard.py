"""Evidence semantics, independent of Cargo/network and long nightly workloads."""
import json
import copy
import os
import zipfile
import tempfile
import hashlib
import io
import subprocess
import textwrap
from types import SimpleNamespace
from unittest.mock import patch
import unittest
from pathlib import Path

import scorecard
import scorecard_metrics as metrics
import scorecard_replay as replay
import mutation_campaign as campaign
import mutation_results as campaign_results

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

    def test_replay_keeps_failed_historical_collector_but_skips_unfinished_self(self):
        for include, status, expected in [(False, 'completed', 0), (True, 'in_progress', 0),
                                          (True, 'completed', 1)]:
            with self.subTest(include=include, status=status), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                job = {'id': 9, 'name': 'scorecard', 'status': status, 'conclusion': 'failure' if status == 'completed' else None,
                       'html_url': 'https://example.test/job/9'}
                (root / 'jobs.json').write_text(json.dumps({'jobs': [job]}))
                args = SimpleNamespace(history=None, history_provenance=None, ledger=root / 'ledger', lattice=self.binary,
                    jobs=root / 'jobs.json', artifacts=root / 'artifacts', repository='owner/repo', run='7',
                    attempt='1', revision='a' * 40, include_completed_collector=include)
                scorecard.ingest(args)
                first = scorecard.export_ledger(args.ledger, self.binary)
                self.assertEqual(len(first), expected)
                scorecard.ingest(args)
                self.assertEqual(first, scorecard.export_ledger(args.ledger, self.binary))
                if expected:
                    observations = scorecard.report(first)['sections']['engineering_health']['observations']
                    self.assertEqual(observations[0]['name'], 'scorecard')
                    self.assertEqual(observations[0]['conclusion'], 'failure')

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



class ReplayEvidenceTests(unittest.TestCase):
    def test_exact_nightly_metadata_shell_handles_multiple_pages_without_unsupported_gh_flags(self):
        # Execute the production shell prefix, emulating API responses and gh's actual flag rejection.
        workflow = (scorecard.ROOT / '.github/workflows/nightly.yml').read_text()
        start = workflow.index('          mkdir -p target/scorecard target/history', workflow.index('fetch retained history'))
        end = workflow.index('          # Retention', start)
        script = textwrap.dedent(workflow[start:end])
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binary = root / 'gh'
            binary.write_text('#!/usr/bin/env python3\nimport json,sys\n'
                'args=sys.argv[1:]\n'
                'if "--slurp" in args and ("--jq" in args or "--template" in args): sys.exit(90)\n'
                'pages=[{"jobs":[{"id":1}]},{"jobs":[{"id":2}]}]\n'
                'print(json.dumps(pages if "--slurp" in args else {"id":7}))\n')
            binary.chmod(0o755)
            environment = dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH'],
                               GITHUB_REPOSITORY='owner/repo', GITHUB_RUN_ID='7', GITHUB_RUN_ATTEMPT='1')
            subprocess.run(['bash', '-e', '-c', script], cwd=root, env=environment, check=True)
            self.assertEqual(json.loads((root / 'target/jobs.json').read_text()), {'jobs': [{'id': 1}, {'id': 2}]})

    def test_replay_requires_exact_completed_nightly_source_and_unambiguous_attempt(self):
        run, job, artifact, workflow = weekly_fixture()
        run['path'] = workflow['path'] = '.github/workflows/nightly.yml'
        run['repository']['id'] = run['head_repository']['id'] = 123
        job['name'] = 'bench'
        artifact.update(name='evidence-bench', digest='sha256:' + 'a' * 64, size_in_bytes=100)
        artifact['workflow_run'].update(repository_id=123, head_repository_id=123)
        self.assertEqual(replay.validate_source(run, workflow, [job], [artifact], 'owner/repo', 'main'), [artifact])
        for field, value in [('run_attempt', 2), ('status', 'in_progress'), ('head_branch', 'fork'), ('workflow_id', 99)]:
            with self.assertRaises(ValueError):
                replay.validate_source(dict(run, **{field: value}), workflow, [job], [artifact], 'owner/repo', 'main')
        bad = copy.deepcopy(artifact)
        bad['workflow_run']['head_sha'] = 'b' * 40
        with self.assertRaises(ValueError):
            replay.validate_source(run, workflow, [job], [bad], 'owner/repo', 'main')
        with self.assertRaises(ValueError):
            replay.validate_source(run, workflow, [job], [], 'owner/repo', 'main')

    def test_replay_rejects_changed_archive_digest_and_path_traversal(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = root / 'evidence.zip'
            with zipfile.ZipFile(path, 'w') as archive:
                archive.writestr('fuzz.log', 'Done 1 runs in 601 second(s)\n')
            digest = 'sha256:' + hashlib.sha256(path.read_bytes()).hexdigest()
            replay.unpack(path, {'digest': digest}, root / 'good')
            self.assertTrue((root / 'good/fuzz.log').exists())
            with self.assertRaises(ValueError):
                replay.unpack(path, {'digest': 'sha256:' + '0' * 64}, root / 'bad')
            with zipfile.ZipFile(path, 'w') as archive:
                archive.writestr('../escaped', 'bad')
            digest = 'sha256:' + hashlib.sha256(path.read_bytes()).hexdigest()
            with self.assertRaises(ValueError):
                replay.unpack(path, {'digest': digest}, root / 'bad')

    def test_json_and_markdown_keep_original_failure_separate_from_successful_replay(self):
        run = {'id': 7, 'html_url': 'https://github.com/owner/repo/actions/runs/7', 'conclusion': 'failure',
               'run_attempt': 1, 'head_sha': 'a' * 40}
        value = scorecard.report([])
        value['run_context'] = replay.run_context(run, 'b' * 40, 'https://github.com/owner/repo/actions/runs/8')
        encoded = json.loads(json.dumps(value))
        self.assertEqual(encoded['run_context']['original_run']['conclusion'], 'failure')
        self.assertEqual(encoded['run_context']['replay']['conclusion'], 'success')
        self.assertEqual(encoded['run_context']['original_run']['url'], run['html_url'])
        rendered = scorecard.markdown(value)
        self.assertIn('Original workflow outcome: **FAILURE**', rendered)
        self.assertIn('Replay outcome: **SUCCESS**', rendered)
        self.assertIn(run['html_url'], rendered)
        self.assertIn('original workflow outcome is unchanged', rendered)

    def test_replay_source_run_is_positive_numeric(self):
        self.assertEqual(replay.positive('40794782229'), '40794782229')
        for value in ('0', '-1', 'abc', '1; echo bad', '01'):
            with self.assertRaises(replay.argparse.ArgumentTypeError):
                replay.positive(value)



def campaign_fixture(count=50):
    universes = {}
    for group, (packages, _) in campaign.GROUPS.items():
        package = packages[0]
        _, universe = mutation_fixture(['CaughtMutant'] * count)
        for mutant in universe:
            mutant['package'] = package
        universes[group] = universe
    source = {'type': 'github_workflow', 'repository': 'owner/repo', 'run_id': 1, 'attempt': 1,
              'revision': 'a' * 40, 'branch': 'main', 'workflow_id': 2, 'workflow_path': '.github/workflows/weekly.yml'}
    plan = campaign.make_plan(universes, source)
    plan['plan_sha256'] = 'd' * 64
    for group, block in plan['groups'].items():
        block['universe_sha256'] = 'b' * 64
    shards = {}
    for row in plan['matrix']['include']:
        group, index = row['group'], row['shard']
        assigned = universes[group][index::row['total']]
        raw, _ = mutation_fixture(['CaughtMutant'] * len(assigned))
        raw['outcomes'] = [{'scenario': 'Baseline', 'summary': 'Success'}] + [
            {'scenario': {'Mutant': {k: v for k, v in mutant.items() if k != 'diff'}}, 'summary': 'CaughtMutant'}
            for mutant in assigned]
        shards[f'{group}/{index}'] = {'manifest': {'schema': 1, 'source': source, 'group': group, 'shard': index,
                    'total': row['total'], 'tool_version': campaign.VERSION, 'universe_sha256': 'b' * 64,
                    'plan_sha256': 'd' * 64},
            'command': {'arguments': campaign.command(group) + ['--shard', f'{index}/{row["total"]}', '--sharding', 'round-robin', '--output', '/tmp/test-only-output']},
            'assigned': assigned, 'mutants': assigned, 'outcomes': raw,
            'job_id': 10 + len(shards), 'job_status': 'completed', 'job_conclusion': 'success',
            'artifact_id': 100 + len(shards), 'artifact_digest': 'sha256:' + 'c' * 64}
        shard = shards[f'{group}/{index}']
        shard['baseline'] = campaign.baseline_context(shard['manifest'], assigned)
        shard['baseline']['commands'] = []
        shard['baseline_logs'] = {}
        for number, arguments in enumerate(campaign.baseline_commands(group, assigned)):
            name, raw_log = f'baselines/{number}.log', b'test result: ok\n'
            shard['baseline_logs'][name] = raw_log
            shard['baseline']['commands'].append({'arguments': arguments, 'status': 'completed',
                'exit_code': 0, 'log': name, 'log_sha256': hashlib.sha256(raw_log).hexdigest()})
    return plan, universes, shards


class MutationCampaignTests(unittest.TestCase):
    def test_complete_real_assignment_union_has_no_duplicates_or_omitted_mutants(self):
        plan, universes, shards = campaign_fixture()
        assignments = {group: {} for group in campaign.GROUPS}
        for row in plan['matrix']['include']:
            group, index = row['group'], row['shard']
            assignments[group][index] = shards[f'{group}/{index}']['assigned']
        proof = campaign.partition_proof(plan, assignments)
        self.assertEqual(proof['expected_mutants'], 50 * len(campaign.GROUPS))
        self.assertEqual(proof['assigned_mutants_observed'], 50 * len(campaign.GROUPS))
        self.assertTrue(proof['union_equals_full_universe'])
        self.assertTrue(proof['no_duplicates'])
        self.assertTrue(all(row['planned'] <= 24 for row in plan['matrix']['include']))
        del assignments['dependent'][0]
        self.assertFalse(campaign.partition_proof(plan, assignments)['union_equals_full_universe'])

    def test_duplicate_extra_and_changed_mutant_assignments_are_rejected(self):
        plan, universes, shards = campaign_fixture()
        assigned = shards['dependent/0']['assigned']
        for bad in (assigned + assigned[:1], assigned[:-1], list(reversed(assigned))):
            with self.assertRaises(ValueError):
                campaign.check_assignment(plan, 'dependent', 0, bad)
        with self.assertRaises(ValueError):
            campaign.identities(assigned + assigned)

    def test_real_rust_path_labels_allow_internal_parents_without_normalizing_identity(self):
        _, mutants = mutation_fixture(['CaughtMutant'])
        mutant = mutants[0]
        mutant['file'] = 'crates/lattice-bench/src/bin/../runtime_spike/tokio_io.rs'
        identity = metrics.mutation_id(mutant)
        self.assertIn('bin/../runtime_spike', identity)
        for bad in ('../escape.rs', 'crates/../../escape.rs', '/tmp/escape.rs'):
            with self.assertRaises(ValueError):
                metrics.mutation_id(dict(mutant, file=bad))

    def test_matrix_ceiling_fails_without_dropping_universe(self):
        _, universe = mutation_fixture(['CaughtMutant'] * 3073)
        other = copy.deepcopy(universe)
        for mutant in other:
            mutant['package'] = 'different-package'
        universes = {group: copy.deepcopy(universe) for group in campaign.GROUPS}
        universes['self'] = other
        with self.assertRaisesRegex(ValueError, 'matrix ceiling'):
            campaign.make_plan(universes, {})

    def test_shard_commands_preserve_both_original_crate_groups_and_test_budgets(self):
        self.assertEqual(campaign.command('dependent'), ['cargo', 'mutants', '-p', 'lattice-core', '-p', 'lattice-lsl',
            '--test-workspace=true', '--timeout', '120', '--no-shuffle', '-j', '2'])
        command = campaign.command('self')
        self.assertEqual([command[i + 1] for i, item in enumerate(command[:-1]) if item == '-p'],
                         ['lattice-abi', 'lattice-evalkit', 'lattice-sim', 'lattice-seams', 'lattice-cli', 'lattice-bench', 'lattice-sdk'])
        self.assertEqual(command[-6:], ['--test-workspace=false', '--timeout', '120', '--no-shuffle', '-j', '2'])
        self.assertNotIn('--exclude', command)

    def test_campaign_covers_engines_api_sql_testkit_sys_and_latticed_without_overlap(self):
        covered = [package for packages, _ in campaign.GROUPS.values() for package in packages]
        self.assertEqual(len(covered), len(set(covered)), 'a crate is in two mutation groups')
        for package in ('lattice-engines', 'lattice-api', 'lattice-sql', 'lattice-testkit', 'lattice-sys', 'latticed'):
            self.assertIn(package, covered)
        # Dependents run the workspace tests; self-tested groups run their own package.
        self.assertEqual(campaign.command('engines')[-6], '--test-workspace=true')
        self.assertEqual(campaign.command('edge')[-6], '--test-workspace=false')
        self.assertEqual(campaign.baseline_commands('engines', [{'package': 'lattice-sql'}]),
                         [['cargo', 'test', '--workspace']])
        self.assertEqual(campaign.baseline_commands('edge', [{'package': 'latticed'}, {'package': 'lattice-sys'}]),
                         [['cargo', 'test', '--package', 'lattice-sys'], ['cargo', 'test', '--package', 'latticed']])

    def test_plan_matrix_order_does_not_depend_on_group_insertion_order(self):
        plan, universes, _ = campaign_fixture(count=3)
        reordered = dict(reversed(list(universes.items())))
        self.assertEqual(campaign.make_plan(reordered, plan['source'])['matrix'], plan['matrix'])

    def test_full_observed_campaign_is_complete_only_after_source_workflow_finishes(self):
        plan, universes, shards = campaign_fixture()
        observed = campaign_results.aggregate(plan, universes, shards, source_complete=True)
        self.assertTrue(observed['proof']['union_equals_full_universe'])
        self.assertEqual(observed['outcome_counts']['tested'], 50 * len(campaign.GROUPS))
        self.assertTrue(all(row['mutation_score'] == 1.0 for row in observed['modules']))
        for row in observed['modules']:
            metrics.validate_record('mutation', row)
        diagnostic = campaign_results.aggregate(plan, universes, shards, source_complete=False)
        self.assertEqual(diagnostic['scope'], 'diagnostic_only_workflow_in_progress')
        self.assertTrue(all(row['mutation_score'] is None for row in diagnostic['modules']))

    def test_missing_runner_failed_or_baseline_failed_shard_never_yields_a_complete_score(self):
        for damage in ('missing', 'runner', 'baseline'):
            plan, universes, shards = campaign_fixture()
            if damage == 'missing':
                del shards['dependent/0']
            elif damage == 'runner':
                shards['dependent/0'] = {'job_status': 'completed', 'job_conclusion': 'failure', 'job_id': 10}
            else:
                shard = shards['dependent/0']
                shard['outcomes'], _ = mutation_fixture([], planned_count=len(shard['assigned']), baseline='Failure')
                shard['job_conclusion'] = 'failure'
            observed = campaign_results.aggregate(plan, universes, shards, source_complete=True)
            self.assertFalse(observed['proof']['all_outcomes_complete'])
            self.assertTrue(all(row['mutation_score'] is None for row in observed['modules']))
            self.assertGreater(observed['outcome_counts']['unrun'], 0)
            for row in observed['modules']:
                metrics.validate_record('mutation', row)

    def test_manifest_attempt_drift_and_bad_counters_are_invalid_not_coverage(self):
        for damage in ('manifest', 'counter'):
            plan, universes, shards = campaign_fixture()
            if damage == 'manifest':
                shards['dependent/0']['manifest']['source'] = dict(plan['source'], attempt=2)
            else:
                shards['dependent/0']['outcomes']['caught'] += 1
            observed = campaign_results.aggregate(plan, universes, shards, source_complete=True)
            self.assertTrue(observed['proof']['invalid_shards'])
            self.assertTrue(all(row['mutation_score'] is None for row in observed['modules']))

    def test_survivors_and_timeouts_keep_their_denominator_and_failed_status(self):
        for summary, counter in [('MissedMutant', 'missed'), ('Timeout', 'timeout')]:
            plan, universes, shards = campaign_fixture()
            raw = shards['dependent/0']['outcomes']
            raw['outcomes'][1]['summary'] = summary
            raw['caught'] -= 1
            raw[counter] += 1
            shards['dependent/0']['job_conclusion'] = 'failure'
            result = campaign_results.aggregate(plan, universes, shards, source_complete=True)
            module = next(row for row in result['modules'] if row['group'] == 'dependent')
            self.assertEqual(module['score_status'], 'failed')
            self.assertEqual(module['counts']['survived'] if summary == 'MissedMutant' else module['counts']['timeout'], 1)
            self.assertEqual(module['score_denominator'], 50 if summary == 'MissedMutant' else 49)
            metrics.validate_record('mutation', module)

    def test_changed_command_is_invalid_even_if_all_outcomes_claim_success(self):
        plan, universes, shards = campaign_fixture()
        shards['dependent/0']['command']['arguments'].insert(2, '--check')
        result = campaign_results.aggregate(plan, universes, shards, source_complete=True)
        self.assertTrue(result['proof']['invalid_shards'])
        self.assertTrue(all(row['mutation_score'] is None for row in result['modules']))

    def test_verified_plan_and_shard_archive_fetch_support_matrix_names_and_provenance(self):
        plan, universes, shards = campaign_fixture(count=26)
        run, _, _, workflow = weekly_fixture()
        run['repository']['id'] = run['head_repository']['id'] = 123
        jobs, artifacts, archives = [], [], {}
        def add_job(name, identifier):
            job = {'id': identifier, 'run_id': run['id'], 'run_attempt': run['run_attempt'], 'head_sha': run['head_sha'],
                   'name': name, 'status': 'completed', 'conclusion': 'success', 'html_url': f'https://example.test/job/{identifier}'}
            jobs.append(job)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            plan_dir = root / 'plan'
            (plan_dir / 'assignments').mkdir(parents=True)
            assignments = {group: {} for group in campaign.GROUPS}
            for group in campaign.GROUPS:
                campaign.write(plan_dir / (group + '.json'), universes[group])
                plan['groups'][group]['universe_sha256'] = campaign.digest(plan_dir / (group + '.json'))
            for row in plan['matrix']['include']:
                group, index = row['group'], row['shard']
                assigned = shards[f'{group}/{index}']['assigned']
                assignments[group][index] = assigned
                campaign.write(plan_dir / 'assignments' / f'{group}-{index}.json', assigned)
            plan['listing_proof'] = campaign.partition_proof(plan, assignments)
            campaign.write(plan_dir / 'plan.json', plan)
            plan_hash = campaign.digest(plan_dir / 'plan.json')
            def add_artifact(name, directory, identifier):
                path = root / (name + '.zip')
                with zipfile.ZipFile(path, 'w') as archive:
                    for file in directory.rglob('*'):
                        if file.is_file():
                            archive.write(file, str(file.relative_to(directory)))
                archives[identifier] = path.read_bytes()
                artifacts.append({'id': identifier, 'name': name, 'expired': False,
                    'digest': 'sha256:' + hashlib.sha256(archives[identifier]).hexdigest(), 'size_in_bytes': len(archives[identifier]),
                    'workflow_run': {'id': run['id'], 'head_sha': run['head_sha'], 'head_branch': 'main',
                                     'repository_id': 123, 'head_repository_id': 123}})
            add_job('mutation-plan', 10)
            add_job('mutation-summary', 11)
            add_artifact('mutants-plan', plan_dir, 100)
            for number, row in enumerate(plan['matrix']['include']):
                group, index = row['group'], row['shard']
                shard = shards[f'{group}/{index}']
                directory = root / f'shard-{number}'
                (directory / 'mutants.out').mkdir(parents=True)
                shard['manifest']['universe_sha256'] = plan['groups'][group]['universe_sha256']
                shard['manifest']['plan_sha256'] = plan_hash
                shard['baseline']['plan_sha256'] = plan_hash
                shard['baseline']['universe_sha256'] = plan['groups'][group]['universe_sha256']
                for name, value in [('manifest.json', shard['manifest']), ('command.json', shard['command']),
                                    ('baseline.json', shard['baseline']),
                                    ('assigned.json', shard['assigned']), ('regenerated-universe.json', universes[group]),
                                    ('mutants.out/mutants.json', shard['mutants']), ('mutants.out/outcomes.json', shard['outcomes'])]:
                    campaign.write(directory / name, value)
                for name, raw_log in shard['baseline_logs'].items():
                    (directory / name).parent.mkdir(exist_ok=True)
                    (directory / name).write_bytes(raw_log)
                add_job(f'mutation ({group}, {index})', 20 + number)
                add_artifact(f'mutants-{group}-{index}', directory, 200 + number)
            def download(command, stdout, check):
                identifier = int(command[-1].split('/')[-2])
                stdout.write(archives[identifier])
            with patch.object(campaign_results.subprocess, 'run', side_effect=download):
                result = campaign_results.fetch('owner/repo', 'main', run, workflow, artifacts, jobs)
            self.assertTrue(result['proof']['union_equals_full_universe'])
            self.assertEqual(result['outcome_counts']['tested'], 26 * len(campaign.GROUPS))
            self.assertTrue(all(row['source']['authentication'] == 'validated_shard_artifacts_and_complete_universe_plan' for row in result['modules']))
            self.assertTrue(all(row['mutation_score'] == 1.0 for row in result['modules']))
            original = archives[200]
            artifact = next(item for item in artifacts if item['id'] == 200)
            for damage in ('missing', 'malformed', 'nested', 'tampered_log'):
                with self.subTest(damage=damage):
                    replacement = io.BytesIO()
                    with zipfile.ZipFile(io.BytesIO(original)) as old, zipfile.ZipFile(replacement, 'w') as new:
                        for name in old.namelist():
                            if damage == 'missing' and name == 'baseline.json': continue
                            data = old.read(name)
                            if damage == 'malformed' and name == 'baseline.json': data = b'{'
                            if damage == 'nested' and name == 'baseline.json':
                                data = b'{"commands":' + b'[' * 1200 + b'0' + b']' * 1200 + b'}'
                            if damage == 'tampered_log' and name == 'baselines/0.log': data += b'changed'
                            new.writestr(name, data)
                    archives[200] = replacement.getvalue()
                    artifact.update(digest='sha256:' + hashlib.sha256(archives[200]).hexdigest(),
                                    size_in_bytes=len(archives[200]))
                    with patch.object(campaign_results.subprocess, 'run', side_effect=download):
                        result = campaign_results.fetch('owner/repo', 'main', run, workflow, artifacts, jobs)
                    self.assertEqual(result['outcome_counts']['tested'], 26 * len(campaign.GROUPS))
                    self.assertFalse(result['proof']['all_baseline_scopes_verified'])
                    self.assertTrue(all(row['mutation_score'] is None for row in result['modules']))
            wrong_attempt = dict(run, run_attempt=2)
            with patch.object(campaign_results.subprocess, 'run', side_effect=download), self.assertRaises(ValueError):
                campaign_results.fetch('owner/repo', 'main', wrong_attempt, workflow, artifacts, jobs)

    def test_plan_source_must_match_exact_workflow_repository_and_revision(self):
        run, job, artifact, workflow = weekly_fixture()
        source = campaign.source_from_run(run, 'owner/repo', 'a' * 40)
        self.assertEqual(source['attempt'], 1)
        for field, value in [('head_sha', 'b' * 40), ('path', '.github/workflows/other.yml'),
                             ('head_repository', {'full_name': 'fork/repo'})]:
            with self.assertRaises(ValueError):
                campaign.source_from_run(dict(run, **{field: value}), 'owner/repo', 'a' * 40)

    def test_legacy_single_job_mutation_import_retains_both_groups_without_score(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'legacy.zip'
            with zipfile.ZipFile(path, 'w') as archive:
                for prefix, package in [('mutants', 'lattice-core'), ('mutants-crates', 'lattice-abi')]:
                    raw, planned = mutation_fixture(['CaughtMutant', 'MissedMutant'])
                    for mutant in planned: mutant['package'] = package
                    for outcome in raw['outcomes']:
                        if isinstance(outcome['scenario'], dict): outcome['scenario']['Mutant']['package'] = package
                    archive.writestr(f'{prefix}/mutants.out/outcomes.json', json.dumps(raw))
                    archive.writestr(f'{prefix}/mutants.out/mutants.json', json.dumps(planned))
            observed = metrics.import_mutation_archive(path, *weekly_fixture(), 'owner/repo', 'main')
            self.assertEqual({row['package'] for row in observed}, {'lattice-core', 'lattice-abi'})
            for row in observed:
                self.assertEqual(row['counts']['tested'], 2)
                self.assertEqual(row['counts']['survived'], 1)
                self.assertEqual(row['baseline_scope'], 'unverified')
                self.assertTrue(row['internal_baseline_passed'])
                self.assertFalse(row['baseline_passed'])
                self.assertIsNone(row['mutation_score'])
                self.assertEqual(row['score_status'], 'unverified')
                metrics.validate_record('mutation', row)
            run, job, artifact, workflow = weekly_fixture()
            payload = path.read_bytes()
            def download(command, stdout, check):
                stdout.write(payload)
            with patch.object(metrics, 'gh_json', side_effect=[workflow,
                    {'workflow_runs': [{'id': run['id']}]}, run,
                    [{'artifacts': [artifact]}], {'jobs': [job]}]), \
                 patch.object(metrics.subprocess, 'run', side_effect=download):
                fetched = metrics.fetch_weekly('owner/repo', 'main')
            self.assertEqual({row['package'] for row in fetched}, {'lattice-core', 'lattice-abi'})
            self.assertTrue(all(row['mutation_score'] is None for row in fetched))

    def test_matching_baseline_commands_do_not_unify_self_package_features(self):
        assigned = [{'package': 'lattice-sim'}, {'package': 'lattice-abi'}, {'package': 'lattice-sim'}]
        self.assertEqual(campaign.baseline_commands('self', assigned),
                         [['cargo', 'test', '--package', 'lattice-abi'],
                          ['cargo', 'test', '--package', 'lattice-sim']])
        self.assertEqual(campaign.baseline_commands('dependent', [{'package': 'lattice-core'}]),
                         [['cargo', 'test', '--workspace']])
        with self.assertRaises(ValueError):
            campaign.baseline_commands('self', [{'package': 'lattice-core'}])

    def test_old_or_bad_scope_proof_retains_survivors_but_never_qualifies_for_score(self):
        for damage in ('missing', 'boolean', 'scope', 'duplicate', 'source', 'group', 'shard',
                       'plan', 'assigned', 'log', 'digest', 'failed', 'boolean_exit',
                       'started', 'malformed', 'extra', 'nested'):
            with self.subTest(damage=damage):
                plan, universes, shards = campaign_fixture(count=2)
                shard = shards['dependent/0']
                raw = shard['outcomes']
                raw['outcomes'][1]['summary'] = 'MissedMutant'
                raw['caught'] -= 1
                raw['missed'] += 1
                shard['job_conclusion'] = 'failure'
                proof = shard['baseline']
                command = proof['commands'][0]
                if damage == 'missing': del shard['baseline']
                elif damage == 'boolean': shard['baseline'] = True
                elif damage == 'scope': command['arguments'] = ['cargo', 'test', '-p', 'lattice-core']
                elif damage == 'duplicate': proof['commands'] *= 2
                elif damage == 'source': proof['source'] = dict(proof['source'], attempt=2)
                elif damage == 'group': proof['group'] = 'self'
                elif damage == 'shard': proof['shard'] = True
                elif damage == 'plan': proof['plan_sha256'] = 'e' * 64
                elif damage == 'assigned': proof['assigned_ids'] = []
                elif damage == 'log': shard['baseline_logs'] = {}
                elif damage == 'digest': shard['baseline_logs']['baselines/0.log'] += b'tampered'
                elif damage == 'failed': command['exit_code'] = 101
                elif damage == 'boolean_exit': command['exit_code'] = False
                elif damage == 'started': command['status'] = 'started'
                elif damage == 'malformed': proof['commands'] = [None]
                elif damage == 'extra': proof['passed'] = True
                elif damage == 'nested':
                    nested = 0
                    for _ in range(1200): nested = [nested]
                    proof['extra'] = nested
                result = campaign_results.aggregate(plan, universes, shards, True)
                self.assertEqual(result['outcome_counts']['survived'], 1)
                self.assertEqual(result['outcome_counts']['tested'], 2 * len(campaign.GROUPS))
                self.assertFalse(result['proof']['all_baseline_scopes_verified'])
                self.assertFalse(result['proof']['all_baselines_passed'])
                self.assertTrue(all(row['mutation_score'] is None for row in result['modules']))
                self.assertTrue(all(row['score_status'] == 'unverified' for row in result['modules']))

    def test_matching_baseline_producer_fail_stops_and_retains_started_records(self):
        for group in ('dependent', 'self'):
            for outcome in (0, 17, 'interrupted'):
                with self.subTest(group=group, outcome=outcome), tempfile.TemporaryDirectory() as tmp:
                    plan, universes, shards = campaign_fixture(count=2)
                    assigned = shards[f'{group}/0']['assigned']
                    expected = ([['cargo', 'test', '--workspace']] if group == 'dependent' else
                                [['cargo', 'test', '--package', 'lattice-abi']])
                    output = Path(tmp) / 'output'
                    args = SimpleNamespace(plan=tmp, group=group, shard=0, output=str(output),
                                           run_metadata='metadata.json', repository='owner/repo')
                    observed = []
                    def execute(arguments, path):
                        observed.append(arguments)
                        if arguments[:2] == ['cargo', 'test']:
                            proof = json.loads((output / 'baseline.json').read_text())
                            self.assertEqual(proof['commands'][0]['status'], 'started')
                            self.assertFalse((output / 'command.json').exists())
                            path.write_bytes(b'raw baseline log\n')
                            if outcome == 'interrupted': raise OSError('interrupted')
                            return outcome
                        path.write_bytes(b'mutation fixture, no actual mutation\n')
                        return 0
                    with patch.object(campaign, 'verify_tool'), \
                         patch.object(campaign, 'load_plan', return_value=(plan, universes)), \
                         patch.object(campaign, 'checkout_revision', return_value='a' * 40), \
                         patch.object(campaign.scorecard, 'load', return_value={}), \
                         patch.object(campaign, 'source_from_run', return_value=plan['source']), \
                         patch.object(campaign, 'digest', return_value='d' * 64), \
                         patch.object(campaign, 'list_mutants', side_effect=[universes[group], assigned]), \
                         patch.object(campaign, 'run_logged', side_effect=execute):
                        if outcome == 'interrupted':
                            with self.assertRaises(OSError): campaign.shard_run(args)
                        else:
                            self.assertEqual(campaign.shard_run(args), outcome)
                    self.assertEqual(observed[:1], expected)
                    self.assertEqual(len(observed), 2 if outcome == 0 else 1)
                    if outcome == 0:
                        self.assertEqual(observed[1][:2], ['cargo', 'mutants'])
                    else:
                        self.assertFalse((output / 'command.json').exists())
                    proof = json.loads((output / 'baseline.json').read_text())
                    command = proof['commands'][0]
                    self.assertEqual(command['status'], 'started' if outcome == 'interrupted' else 'completed')
                    if outcome != 'interrupted':
                        self.assertEqual(command['exit_code'], outcome)

    def test_self_baselines_run_individually_with_actual_log_hashes(self):
        plan, _, shards = campaign_fixture(count=2)
        assigned = copy.deepcopy(shards['self/0']['assigned'])
        assigned[1]['package'] = 'lattice-sim'
        manifest = shards['self/0']['manifest']
        expected = [['cargo', 'test', '--package', 'lattice-abi'],
                    ['cargo', 'test', '--package', 'lattice-sim']]
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp)
            observed = []
            def execute(arguments, path):
                observed.append(arguments)
                path.write_bytes(('log for ' + arguments[-1]).encode())
                return 0
            with patch.object(campaign, 'run_logged', side_effect=execute):
                self.assertEqual(campaign.run_baselines(manifest, assigned, output), 0)
            self.assertEqual(observed, expected)
            proof = json.loads((output / 'baseline.json').read_text())
            shard = {'manifest': manifest, 'baseline': proof, 'baseline_logs': {
                f'baselines/{i}.log': ('log for ' + command[-1]).encode()
                for i, command in enumerate(expected)}}
            campaign_results.validate_baseline(shard, assigned, manifest)
            proof['commands'].pop()
            with self.assertRaises(ValueError):
                campaign_results.validate_baseline(shard, assigned, manifest)

    def test_consistent_manifest_and_baseline_counter_drift_is_invalid_identity(self):
        for field in ('schema', 'shard', 'total', 'run_id', 'attempt', 'workflow_id'):
            with self.subTest(field=field):
                plan, universes, shards = campaign_fixture(count=26)
                shard = shards['dependent/1']
                manifest = shard['manifest']
                if field in ('schema', 'shard', 'total'):
                    manifest[field] = True
                else:
                    manifest['source'] = dict(manifest['source'], **{field: True})
                # The forged baseline agrees with the forged producer, not with the trusted plan.
                records = shard['baseline']['commands']
                shard['baseline'] = campaign.baseline_context(manifest, shard['assigned'])
                shard['baseline']['commands'] = records
                result = campaign_results.aggregate(plan, universes, shards, True)
                self.assertTrue(result['proof']['invalid_shards'])
                self.assertEqual(next(row for row in result['shards'] if row['group'] == 'dependent'
                                      and row['shard'] == 1)['state'], 'invalid')
                self.assertTrue(all(row['mutation_score'] is None for row in result['modules']))


if __name__ == '__main__':
    unittest.main()
