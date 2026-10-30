import os
import tempfile
import unittest

import check_expected_tests as c

LISTING = "a::one: test\na::two: test\na::three: test\n3 tests, 0 benchmarks\n"


class CheckExpectedTests(unittest.TestCase):
    def test_extra_tests_are_fine(self):
        self.assertEqual(c.missing("a::one\n", LISTING), [])

    def test_a_missing_name_is_reported_by_name(self):
        self.assertEqual(c.missing("a::one\na::gone\n", LISTING), ["a::gone"])

    def test_comments_and_blank_lines_are_ignored(self):
        self.assertEqual(c.missing("# note\n\na::two\n", LISTING), [])

    def test_a_prefix_or_substring_does_not_match(self):
        self.assertEqual(c.missing("a::on\n", LISTING), ["a::on"])

    def test_cli_exit_codes(self):
        with tempfile.TemporaryDirectory() as d:
            m, listing = os.path.join(d, "m"), os.path.join(d, "l")
            open(listing, "w").write(LISTING)
            open(m, "w").write("a::one\n")
            self.assertEqual(c.main(["x", m, listing]), 0)
            open(m, "w").write("a::gone\n")
            self.assertEqual(c.main(["x", m, listing]), 1)
            open(listing, "w").write("")
            self.assertEqual(c.main(["x", m, listing]), 1)


if __name__ == "__main__":
    unittest.main()
