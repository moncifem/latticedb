import unittest

import check_ignored_tests as check

VOLUME = '''
#[test]
#[ignore = "volume: 100,000 seeds"]
fn big_sweep() {}
'''
PENDING = '''
#[test]
#[ignore = "phase 7: history question"]
fn ns2() {}
'''
PATH = 'crates/x/tests/sweeps.rs'


def flow(command):
    return {'nightly.yml': f'jobs:\n  a:\n    steps:\n      - run: {command}\n'}


class IgnoredTests(unittest.TestCase):
    def test_reason_and_name_are_read(self):
        self.assertEqual(check.ignored_tests(VOLUME, PATH), [(PATH, 'big_sweep', 'volume: 100,000 seeds')])

    def test_bare_ignore_has_an_empty_reason(self):
        text = '#[test]\n#[ignore]\nfn quiet() {}\n'
        self.assertEqual(check.ignored_tests(text, PATH), [(PATH, 'quiet', '')])

    def test_macro_generated_function_keeps_its_placeholder(self):
        text = '#[test]\n#[ignore = "volume: slice"]\nfn $test() { run(); }\n'
        self.assertEqual(check.ignored_tests(text, PATH), [(PATH, '$test', 'volume: slice')])

    def test_commented_ignore_is_skipped(self):
        self.assertEqual(check.ignored_tests('// #[ignore = "x"]\nfn a() {}\n', PATH), [])

    def test_ignore_without_a_function_is_an_error(self):
        with self.assertRaises(ValueError):
            check.ignored_tests('#[ignore = "x"]\n' + '// c\n' * 10, PATH)

    def test_pending_specification_needs_nothing(self):
        tests = check.ignored_tests(PENDING, PATH)
        self.assertEqual(check.problems(tests, {}, []), [])

    def test_unrun_test_is_reported_with_the_fix(self):
        found = check.problems(check.ignored_tests(VOLUME, PATH), {}, [])
        self.assertEqual(len(found), 1)
        self.assertIn('`big_sweep` is run by no workflow', found[0])
        self.assertIn('--test sweeps', found[0])

    def test_name_on_an_ignored_line_counts(self):
        wf = flow('cargo test -p x --release --test sweeps -- --ignored big_sweep --nocapture')
        self.assertEqual(check.problems(check.ignored_tests(VOLUME, PATH), wf, []), [])

    def test_name_without_ignored_flag_does_not_count(self):
        wf = flow('cargo test -p x --release big_sweep')
        self.assertEqual(len(check.problems(check.ignored_tests(VOLUME, PATH), wf, [])), 1)

    def test_name_must_be_a_whole_word(self):
        wf = flow('cargo test --release -- --ignored big_sweep_other')
        self.assertEqual(len(check.problems(check.ignored_tests(VOLUME, PATH), wf, [])), 1)

    def test_commented_workflow_line_does_not_count(self):
        wf = {'nightly.yml': '    # cargo test -- --ignored big_sweep\n'}
        self.assertEqual(len(check.problems(check.ignored_tests(VOLUME, PATH), wf, [])), 1)

    def test_test_file_with_ignored_runs_every_ignored_test_in_it(self):
        wf = flow('cargo test -p x --release --test other --test sweeps -- --ignored')
        self.assertEqual(check.problems(check.ignored_tests(VOLUME, PATH), wf, []), [])

    def test_test_file_selection_needs_the_ignored_flag(self):
        wf = flow('cargo test -p x --release --test sweeps')
        self.assertEqual(len(check.problems(check.ignored_tests(VOLUME, PATH), wf, [])), 1)

    def test_test_file_selection_applies_only_to_integration_tests(self):
        wf = flow('cargo test -p x --test sweeps -- --ignored')
        lib = check.ignored_tests(VOLUME, 'crates/x/src/sweeps.rs')
        self.assertEqual(len(check.problems(lib, wf, [])), 1)

    def test_allow_entry_with_a_reason_passes(self):
        allow = [{'path': PATH, 'name': 'big_sweep', 'reason': 'run through the example binary instead'}]
        self.assertEqual(check.problems(check.ignored_tests(VOLUME, PATH), {}, allow), [])

    def test_allow_entry_needs_every_field(self):
        tests = check.ignored_tests(VOLUME, PATH)
        for missing in ('path', 'name', 'reason'):
            entry = {'path': PATH, 'name': 'big_sweep', 'reason': 'why'}
            entry[missing] = '  '
            found = check.problems(tests, {}, [entry])
            self.assertTrue(any('needs path, name and a reason' in m for m in found), missing)

    def test_stale_allow_entry_is_reported(self):
        allow = [{'path': PATH, 'name': 'gone', 'reason': 'why'}]
        found = check.problems([], {}, allow)
        self.assertEqual(found, [f'tools/ignored-tests.toml: {PATH}::gone matches no ignored test'])

    def test_allow_entry_for_a_test_a_workflow_runs_is_reported(self):
        wf = flow('cargo test -- --ignored big_sweep')
        allow = [{'path': PATH, 'name': 'big_sweep', 'reason': 'why'}]
        found = check.problems(check.ignored_tests(VOLUME, PATH), wf, allow)
        self.assertEqual(found, [f'tools/ignored-tests.toml: {PATH}::big_sweep is run by a workflow; remove its entry'])

    def test_duplicate_allow_entry_is_reported(self):
        entry = {'path': PATH, 'name': 'big_sweep', 'reason': 'why'}
        found = check.problems(check.ignored_tests(VOLUME, PATH), {}, [entry, dict(entry)])
        self.assertEqual(len(found), 1)
        self.assertIn('duplicate entry', found[0])

    def test_same_name_in_another_file_is_not_covered_by_an_entry(self):
        allow = [{'path': PATH, 'name': 'big_sweep', 'reason': 'why'}]
        other = check.ignored_tests(VOLUME, 'crates/y/tests/other.rs')
        found = check.problems(other, {}, allow)
        self.assertEqual(len(found), 2)

    def test_repository_ignored_tests_are_accounted_for(self):
        import tomllib
        workflows = {p.name: p.read_text() for p in (check.ROOT / '.github/workflows').glob('*.yml')}
        allow = tomllib.loads((check.ROOT / 'tools/ignored-tests.toml').read_text()).get('allow', [])
        self.assertEqual(check.problems(check.collect(check.ROOT), workflows, allow), [])

    def test_the_two_volume_tests_of_issue_455_are_found_and_run(self):
        tests = {(p, n) for p, n, _ in check.collect(check.ROOT)}
        workflows = {p.name: p.read_text() for p in (check.ROOT / '.github/workflows').glob('*.yml')}
        commands = check.ignored_commands(workflows)
        for path, name in (
            ('crates/lattice-testkit/tests/convergence_replicas.rs', 'convergence_at_gate_volume'),
            ('crates/lattice-testkit/tests/fencing_races.rs', 'superseded_turns_are_fenced_across_profiles_volume'),
        ):
            self.assertIn((path, name), tests)
            self.assertTrue(check.run_by_workflow(path, name, commands), name)


if __name__ == '__main__':
    unittest.main()
