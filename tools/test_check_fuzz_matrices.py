import tempfile
import unittest
from pathlib import Path

import check_fuzz_matrices as check

CARGO = '''
[[bin]]
name = "trivial"
[[bin]]
name = "alpha"
[[bin]]
name = "beta"
'''


def workflow(targets):
    return f'jobs:\n  fuzz:\n    strategy:\n      matrix:\n        target: [{targets}]\n'


class CheckFuzzMatrices(unittest.TestCase):
    def test_complete_matrices_pass(self):
        flows = {'a.yml': workflow('alpha, beta'), 'b.yml': workflow('beta,alpha')}
        self.assertEqual(check.problems(CARGO, flows), [])

    def test_missing_target_is_reported_per_workflow(self):
        flows = {'a.yml': workflow('alpha'), 'b.yml': workflow('alpha, beta')}
        self.assertEqual(check.problems(CARGO, flows), ['a.yml: fuzz target `beta` is not in the matrix'])

    def test_unknown_target_is_reported(self):
        flows = {'a.yml': workflow('alpha, beta, gamma')}
        self.assertEqual(check.problems(CARGO, flows),
                         ['a.yml: matrix lists `gamma`, which is not a bin in fuzz/Cargo.toml'])

    def test_trivial_is_exempt_either_way(self):
        self.assertEqual(check.problems(CARGO, {'a.yml': workflow('alpha, beta, trivial')}), [])

    def test_matrix_must_be_found_exactly_once(self):
        with self.assertRaises(ValueError):
            check.problems(CARGO, {'a.yml': 'jobs: {}\n'})
        with self.assertRaises(ValueError):
            check.problems(CARGO, {'a.yml': workflow('alpha, beta') + workflow('alpha')})

    def test_duplicate_matrix_entry_is_rejected(self):
        with self.assertRaises(ValueError):
            check.problems(CARGO, {'a.yml': workflow('alpha, alpha, beta')})

    def test_target_without_seeds_is_reported(self):
        flows = {'a.yml': workflow('alpha, beta')}
        self.assertEqual(check.problems(CARGO, flows, {'alpha'}),
                         ['fuzz/seeds/beta: no seed files for fuzz target `beta`'])

    def test_seeded_targets_pass_and_trivial_needs_no_seeds(self):
        flows = {'a.yml': workflow('alpha, beta')}
        self.assertEqual(check.problems(CARGO, flows, {'alpha', 'beta'}), [])

    def test_seeds_are_not_checked_unless_given(self):
        self.assertEqual(check.problems(CARGO, {'a.yml': workflow('alpha, beta')}), [])

    def test_seeded_targets_ignores_empty_directories_and_stray_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'full').mkdir()
            (root / 'full' / 'seed').write_bytes(b'x')
            (root / 'empty').mkdir()
            (root / 'nested').mkdir()
            (root / 'nested' / 'inner').mkdir()
            (root / 'stray.py').write_text('')
            self.assertEqual(check.seeded_targets(root), {'full'})

    def test_repository_matrices_are_complete_and_seeded(self):
        cargo = (check.ROOT / 'fuzz/Cargo.toml').read_text()
        flows = {n: (check.ROOT / '.github/workflows' / n).read_text() for n in check.MATRICES}
        self.assertEqual(check.problems(cargo, flows, check.seeded_targets(check.ROOT / 'fuzz/seeds'), check.load_settings()), [])


SETTINGS = {
    'defaults': {'timeout_secs': 20, 'rss_limit_mb': 2048},
    'targets': {'alpha': {'max_len': 128, 'basis': 'a'}, 'beta': {'max_len': 4096, 'basis': 'b'}},
}
RUN_OK = (
    '      - run: |\n'
    '          FUZZ_FLAGS=$(python3 ../tools/check_fuzz_matrices.py flags "$T")\n'
    '          cargo fuzz run x "c" -- $FUZZ_FLAGS -max_total_time=600\n'
)


def copy_settings(**changes):
    out = {'defaults': dict(SETTINGS['defaults']), 'targets': {k: dict(v) for k, v in SETTINGS['targets'].items()}}
    for key, value in changes.items():
        out['defaults' if key in out['defaults'] else 'targets'][key] = value
    return out


class CheckFuzzSettings(unittest.TestCase):
    def run_problems(self, settings=None, text=None):
        flows = {'a.yml': workflow('alpha, beta') + (RUN_OK if text is None else text)}
        return check.problems(CARGO, flows, None, SETTINGS if settings is None else settings)

    def test_complete_settings_and_flags_pass(self):
        self.assertEqual(self.run_problems(), [])

    def test_flags_are_built_from_defaults_and_the_target(self):
        self.assertEqual(check.flags_for(SETTINGS, 'beta'), '-timeout=20 -rss_limit_mb=2048 -max_len=4096')

    def test_flags_for_an_unknown_target_raise(self):
        with self.assertRaises(KeyError):
            check.flags_for(SETTINGS, 'gamma')

    def test_missing_target_entry_is_reported(self):
        settings = copy_settings()
        del settings['targets']['beta']
        self.assertEqual(self.run_problems(settings), ['fuzz/settings.toml: no [targets.beta] entry for fuzz target `beta`'])

    def test_entry_for_a_removed_target_is_reported(self):
        settings = copy_settings()
        settings['targets']['gamma'] = {'max_len': 1, 'basis': 'x'}
        self.assertEqual(self.run_problems(settings),
                         ['fuzz/settings.toml: [targets.gamma] is not a fuzz target in fuzz/Cargo.toml'])

    def test_bad_max_len_is_reported(self):
        for bad in (0, -1, check.MAX_LEN_CEILING + 1, '4096', True, None):
            settings = copy_settings()
            settings['targets']['alpha']['max_len'] = bad
            found = self.run_problems(settings)
            self.assertEqual(len(found), 1, bad)
            self.assertIn('[targets.alpha] max_len', found[0])

    def test_max_len_ceiling_itself_is_allowed(self):
        settings = copy_settings()
        settings['targets']['alpha']['max_len'] = check.MAX_LEN_CEILING
        self.assertEqual(self.run_problems(settings), [])

    def test_missing_or_blank_basis_is_reported(self):
        for bad in (None, '', '   ', 3):
            settings = copy_settings()
            settings['targets']['beta']['basis'] = bad
            found = self.run_problems(settings)
            self.assertEqual(len(found), 1, bad)
            self.assertIn('[targets.beta] needs a `basis`', found[0])

    def test_timeout_must_fit_inside_the_shortest_run(self):
        for bad in (0, check.TIMEOUT_CEILING + 1, None, 1.5, True):
            settings = copy_settings()
            settings['defaults']['timeout_secs'] = bad
            found = self.run_problems(settings)
            self.assertEqual(len(found), 1, bad)
            self.assertIn('timeout_secs', found[0])
        settings = copy_settings()
        settings['defaults']['timeout_secs'] = check.TIMEOUT_CEILING
        self.assertEqual(self.run_problems(settings), [])

    def test_rss_limit_is_required(self):
        settings = copy_settings()
        del settings['defaults']['rss_limit_mb']
        self.assertEqual(len(self.run_problems(settings)), 1)

    def test_fuzz_run_without_flags_is_reported_with_its_line(self):
        text = '      - run: cargo fuzz run x c -- -max_total_time=600\n'
        found = self.run_problems(text=text + '          FUZZ=x # flags\n')
        self.assertTrue(any('does not pass $FUZZ_FLAGS' in m for m in found), found)
        self.assertTrue(any('never builds the limits' in m for m in found), found)

    def test_flags_variable_on_a_continuation_line_counts(self):
        text = ('      - run: |\n'
                '          F=$(python3 ../tools/check_fuzz_matrices.py flags "$T")\n'
                '          cargo fuzz run x c -- \\\n'
                '            ${FUZZ_FLAGS} -max_total_time=1\n')
        self.assertEqual(self.run_problems(text=text), [])

    def test_flags_on_a_later_command_do_not_count(self):
        text = ('      - run: |\n'
                '          FUZZ_FLAGS=$(python3 ../tools/check_fuzz_matrices.py flags "$T")\n'
                '          cargo fuzz run x c -- -max_total_time=1\n'
                '          echo $FUZZ_FLAGS\n')
        found = self.run_problems(text=text)
        self.assertEqual(len(found), 1)
        self.assertIn('a.yml:', found[0])

    def test_every_run_command_is_checked(self):
        found = self.run_problems(text=RUN_OK + '      - run: cargo fuzz run y c -- -merge=1\n')
        self.assertEqual(len(found), 1)
        self.assertIn('does not pass $FUZZ_FLAGS', found[0])

    def test_workflow_without_fuzz_run_needs_no_flags(self):
        self.assertEqual(self.run_problems(text=''), [])

    def test_flags_variable_only_in_a_comment_does_not_count(self):
        for text in (
            '      - run: |\n          F=$(python3 ../tools/check_fuzz_matrices.py flags "$T")\n'
            '          # cargo fuzz run x c -- $FUZZ_FLAGS\n          cargo fuzz run x c -- -max_total_time=1\n',
            '      - run: |\n          F=$(python3 ../tools/check_fuzz_matrices.py flags "$T")\n'
            '          cargo fuzz run x c -- -max_total_time=1 # $FUZZ_FLAGS\n',
            '      - run: |\n          F=$(python3 ../tools/check_fuzz_matrices.py flags "$T")\n'
            '          cargo fuzz run x c -- \\\n            # $FUZZ_FLAGS\n            -max_total_time=1\n',
        ):
            found = self.run_problems(text=text)
            self.assertTrue(any('does not pass $FUZZ_FLAGS' in m for m in found), text)

    def test_fuzz_run_mentioned_only_in_a_comment_is_ignored(self):
        self.assertEqual(self.run_problems(text='      # cargo fuzz run x c\n'), [])

    def test_other_workflow_with_fuzz_run_is_checked(self):
        flows = {'a.yml': workflow('alpha, beta') + RUN_OK}
        other = {'extra.yml': '      - run: cargo fuzz run x c -- -max_total_time=1\n'}
        found = check.problems(CARGO, flows, None, SETTINGS, other)
        self.assertEqual(len(found), 2)
        self.assertTrue(all(m.startswith('extra.yml') for m in found))

    def test_other_workflow_that_passes_the_flags_is_fine(self):
        flows = {'a.yml': workflow('alpha, beta')}
        self.assertEqual(check.problems(CARGO, flows, None, SETTINGS, {'extra.yml': RUN_OK}), [])

    def test_every_repository_workflow_with_fuzz_run_passes_the_flags(self):
        for path in sorted((check.ROOT / '.github/workflows').glob('*.yml')):
            text = path.read_text()
            if check.FUZZ_RUN.search(check.without_comments(text)):
                self.assertEqual(check.workflow_flag_problems(path.name, text), [], path.name)

    def test_flags_command_prints_the_repository_settings(self):
        settings = check.load_settings()
        self.assertEqual(check.main(['flags', 'pgwire']), 0)
        self.assertEqual(check.main(['flags', 'no_such_target']), 1)
        self.assertEqual(check.main(['flags']), 2)
        self.assertIn('pgwire', settings['targets'])


if __name__ == '__main__':
    unittest.main()
