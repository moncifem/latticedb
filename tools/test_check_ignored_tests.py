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


def flow(command, head='on:\n  schedule: [{ cron: "0 2 * * *" }]\n', job_if=''):
    cond = f'    if: {job_if}\n' if job_if else ''
    return {'nightly.yml': f'{head}jobs:\n  a:\n{cond}    steps:\n      - run: {command}\n'}


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
        self.assertEqual(check.problems(tests, {}, [], (7,)), [])

    def test_pending_phase_that_is_not_listed_is_reported(self):
        found = check.problems(check.ignored_tests(PENDING, PATH), {}, [], (3, 8))
        self.assertEqual(len(found), 1)
        self.assertIn('phase 7', found[0])

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
        found = check.problems(check.ignored_tests(PENDING, PATH), {}, allow, (7,))
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

    def test_zero_ignored_tests_is_an_error(self):
        self.assertEqual(check.problems([], {}, []), ['no ignored test found under crates/ and xtask/; the scan is broken'])

    def test_cfg_attr_ignore_is_found(self):
        text = '#[test]\n#[cfg_attr(not(feature = "x"), ignore = "volume: slow")]\nfn slow() {}\n'
        self.assertEqual(check.ignored_tests(text, PATH), [(PATH, 'slow', 'volume: slow')])

    def test_cfg_attr_ignore_without_reason_is_found(self):
        text = '#[cfg_attr(unix, ignore)]\nfn slow() {}\n'
        self.assertEqual(check.ignored_tests(text, PATH), [(PATH, 'slow', '')])

    def test_multi_line_attribute_is_found(self):
        text = '#[test]\n#[ignore =\n    "volume: slow"\n]\nfn slow() {}\n'
        self.assertEqual(check.ignored_tests(text, PATH), [(PATH, 'slow', 'volume: slow')])

    def test_raw_string_reason_with_brackets_is_found(self):
        text = '#[ignore = r#"volume: "quoted" ] and #[x]"#]\n#[test]\nfn slow() {}\n'
        self.assertEqual(check.ignored_tests(text, PATH), [(PATH, 'slow', 'volume: "quoted" ] and #[x]')])

    def test_word_ignore_inside_a_reason_or_name_is_not_an_attribute_hit(self):
        text = '#[doc = "ignore this"]\nfn a() {}\n#[test_ignored]\nfn b() {}\n#[allow(clippy::ignore_x)]\nfn c() {}\n'
        self.assertEqual(check.ignored_tests(text, PATH), [])

    def test_unclassifiable_attribute_mentioning_ignore_fails(self):
        with self.assertRaises(ValueError):
            check.ignored_tests('#[allow(ignore)]\nfn a() {}\n', PATH)

    def test_attribute_text_inside_a_string_or_comment_is_not_scanned(self):
        text = 'let s = "#[ignore = \\"phase 1\\"]";\n// #[ignore]\n/// #[ignore]\nfn a() {}\n'
        self.assertEqual(check.ignored_tests(text, PATH), [])

    def test_function_far_below_the_attribute_is_found(self):
        text = '#[ignore = "volume: x"]\n' + '// note\n' * 20 + 'fn far() {}\n'
        self.assertEqual(check.ignored_tests(text, PATH), [(PATH, 'far', 'volume: x')])

    def test_workflow_that_is_only_manual_does_not_count(self):
        wf = flow('cargo test -- --ignored big_sweep', head='on:\n  workflow_dispatch:\n')
        self.assertEqual(len(check.problems(check.ignored_tests(VOLUME, PATH), wf, [])), 1)

    def test_commented_trigger_does_not_count(self):
        wf = flow('cargo test -- --ignored big_sweep', head='on:\n  # schedule: nightly\n  workflow_dispatch:\n')
        self.assertEqual(len(check.problems(check.ignored_tests(VOLUME, PATH), wf, [])), 1)

    def test_disabled_or_manual_input_job_does_not_count(self):
        for cond in ('false', '${{ false }}', 'inputs.sql_proptest == true', '${{ inputs.x }}'):
            wf = flow('cargo test -- --ignored big_sweep', job_if=cond)
            self.assertEqual(len(check.problems(check.ignored_tests(VOLUME, PATH), wf, [])), 1, cond)

    def test_job_with_an_ordinary_condition_counts(self):
        wf = flow('cargo test -- --ignored big_sweep', job_if="github.event_name == 'schedule'")
        self.assertEqual(check.problems(check.ignored_tests(VOLUME, PATH), wf, []), [])

    def test_repository_ignored_tests_are_accounted_for(self):
        import tomllib
        workflows = {p.name: p.read_text() for p in (check.ROOT / '.github/workflows').glob('*.yml')}
        config = tomllib.loads((check.ROOT / 'tools/ignored-tests.toml').read_text())
        tests = check.collect(check.ROOT)
        self.assertGreaterEqual(len(tests), 15)
        self.assertEqual(check.problems(tests, workflows, config.get('allow', []), config.get('pending_phases', [])), [])

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
