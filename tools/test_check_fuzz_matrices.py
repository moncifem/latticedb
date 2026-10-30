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
        self.assertEqual(check.problems(cargo, flows, check.seeded_targets(check.ROOT / 'fuzz/seeds')), [])


if __name__ == '__main__':
    unittest.main()
