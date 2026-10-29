import os
import tempfile
import unittest

import changelog as c

OLD = """# Changelog

Intro.

## [Unreleased]

### Added

- Old entry one.

### Fixed

- Old fix.

## [0.1.0-alpha.0]

### Added

- Ancient.
"""


class NameTests(unittest.TestCase):
    def test_valid(self):
        self.assertEqual(c.parse_name("315-changelog-fragments.added.md"), (315, "changelog-fragments", "added"))

    def test_invalid(self):
        for bad in [
            "315.added.md", "x-thing.added.md", "315-Thing.added.md", "315-thing.bogus.md",
            "315-thing.added.txt", "315-thing.md", "315--thing.added.md", "315-thing-.added.md",
        ]:
            self.assertIsNone(c.parse_name(bad), bad)


class TextTests(unittest.TestCase):
    def test_good(self):
        self.assertEqual(c.check_text("Adds a thing.\nSecond line.\n"), [])

    def test_bad(self):
        cases = {
            "": "empty",
            "   \n": "empty",
            "No newline": "newline",
            "Two\n\n": "single newline",
            "- bullet\n": "bullet",
            "* bullet\n": "bullet",
            "## Head\n": "headings",
            "trailing \n": "trailing",
            "mail a@b.com\n": "email",
            "Co-Authored-By: x\n": "attribution",
            "a\r\n": "LF",
            "x" * 2001 + "\n": "longer",
        }
        for text, word in cases.items():
            self.assertTrue(any(word in p for p in c.check_text(text)), (text[:20], c.check_text(text)))


class PrTests(unittest.TestCase):
    def test_added_filter(self):
        changed = [
            ("added", "changelog.d/1-a.added.md"),
            ("modified", "changelog.d/2-b.added.md"),
            ("removed", "changelog.d/3-c.added.md"),
            ("added", "changelog.d/README.md"),
            ("added", "docs/changelog.d/4-d.added.md"),
            ("renamed", "changelog.d/5-e.fixed.md"),
        ]
        self.assertEqual(c.fragments_added(changed), ["changelog.d/1-a.added.md", "changelog.d/5-e.fixed.md"])

    def test_added_keeps_dotfiles_and_nested_paths(self):
        changed = [("added", "changelog.d/.x"), ("added", "changelog.d/sub/1-a.added.md")]
        self.assertEqual(c.fragments_added(changed), ["changelog.d/.x", "changelog.d/sub/1-a.added.md"])

    def test_one_required(self):
        one = "changelog.d/1-a.added.md"
        self.assertEqual(c.check_pr([one], []), [])
        self.assertTrue(c.check_pr([], []))
        self.assertTrue(c.check_pr([one, "changelog.d/2-b.fixed.md"], []))

    def test_misnamed_files_do_not_count_as_the_fragment(self):
        for bad in ["changelog.d/.x", "changelog.d/x.md", "changelog.d/1-a.bogus.md", "changelog.d/sub/1-a.added.md"]:
            with self.subTest(path=bad):
                self.assertTrue(c.check_pr([bad], []))
                # Even next to a valid fragment: the PR is reported, not silently accepted.
                self.assertTrue(c.check_pr(["changelog.d/1-a.added.md", bad], []))
                self.assertTrue(c.check_pr([bad], [], "dependabot[bot]"))

    def test_escape_data(self):
        self.assertEqual(c.escape_data("a%b\r\nc"), "a%25b%0D%0Ac")

    def test_label(self):
        self.assertEqual(c.check_pr([], ["no-changelog"]), [])
        self.assertTrue(c.check_pr(["x"], ["no-changelog"]))

    def test_dependabot(self):
        self.assertEqual(c.check_pr([], [], "dependabot[bot]"), [])
        self.assertTrue(c.check_pr([], [], "someone"))


class CompileTests(unittest.TestCase):
    FR = {
        "20-b.added.md": "New B.\nSecond line.\n",
        "3-a.added.md": "New A.\n",
        "9-c.security.md": "Sec.\n",
        "11-d.changed.md": "Changed.\n",
    }

    def test_compile(self):
        out = c.compile_changelog(OLD, "0.1.0-alpha.1", "2026-11-01", self.FR)
        expected = """# Changelog

Intro.

## [Unreleased]

## [0.1.0-alpha.1] - 2026-11-01

### Added

- Old entry one.

- New A.

- New B.
  Second line.

### Changed

- Changed.

### Fixed

- Old fix.

### Security

- Sec.

## [0.1.0-alpha.0]

### Added

- Ancient.
"""
        self.assertEqual(out, expected)

    def test_fragments_only(self):
        old = "# C\n\n## [Unreleased]\n\n## [0.0.1]\n\n- x\n"
        out = c.compile_changelog(old, "0.0.2", "2026-01-01", {"1-a.fixed.md": "F.\n"})
        self.assertIn("## [0.0.2] - 2026-01-01\n\n### Fixed\n\n- F.\n", out)
        self.assertIn("## [0.0.1]", out)

    def test_legacy_only(self):
        out = c.compile_changelog(OLD, "0.2.0", "2026-01-01", {})
        self.assertIn("- Old entry one.", out)

    def test_refusals(self):
        with self.assertRaises(ValueError):
            c.compile_changelog(OLD, "0.1.0-alpha.0", "d", {})
        with self.assertRaises(ValueError):
            c.compile_changelog(OLD, "v1", "d", {})
        with self.assertRaises(ValueError):
            c.compile_changelog("# C\n\n## [Unreleased]\n\n## [0.0.1]\n", "0.0.2", "d", {})
        with self.assertRaises(ValueError):
            c.compile_changelog("# C\n", "0.0.2", "d", {})

    def test_duplicate_legacy_headings_fail(self):
        dup = "# C\n\n## [Unreleased]\n\n### Added\n\n- a\n\n### Added\n\n- b\n\n## [0.0.1]\n"
        with self.assertRaises(ValueError):
            c.compile_changelog(dup, "0.0.2", "d", {})

    def test_heading_matches_release_workflow_lookup(self):
        # release.yml and package.yml look for a line that starts with `## [<version>]`.
        out = c.compile_changelog(OLD, "1.2.3", "2026-01-01", self.FR)
        self.assertTrue(any(line.startswith("## [1.2.3]") for line in out.splitlines()))


class FilesTests(unittest.TestCase):
    def test_check_fragments(self):
        errors = c.check_fragments(
            {"README.md": "", "1-a.added.md": "ok\n", "bad.md": "x\n", "2-b.added.md": "- b\n"}
        )
        self.assertEqual(len(errors), 2)

    def test_cli_dry_run_then_write(self):
        with tempfile.TemporaryDirectory() as d:
            frag = os.path.join(d, "changelog.d")
            os.mkdir(frag)
            with open(os.path.join(frag, "1-a.added.md"), "w") as f:
                f.write("Thing.\n")
            with open(os.path.join(frag, "README.md"), "w") as f:
                f.write("doc\n")
            log = os.path.join(d, "CHANGELOG.md")
            with open(log, "w") as f:
                f.write(OLD)
            base = ["--dir", frag]
            comp = ["compile", "--version", "0.3.0", "--date", "2026-01-01", "--changelog", log]
            self.assertEqual(c.main(base + ["check"]), 0)
            self.assertEqual(c.main(base + comp), 0)
            self.assertTrue(os.path.exists(os.path.join(frag, "1-a.added.md")))
            with open(log) as f:
                self.assertEqual(f.read(), OLD)
            self.assertEqual(c.main(base + comp + ["--write"]), 0)
            self.assertFalse(os.path.exists(os.path.join(frag, "1-a.added.md")))
            self.assertTrue(os.path.exists(os.path.join(frag, "README.md")))
            with open(log) as f:
                self.assertIn("## [0.3.0] - 2026-01-01", f.read())

    def test_check_fails_on_dotfile_and_subdirectory(self):
        for name in [".x", "sub"]:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as d:
                path = os.path.join(d, name)
                if name == "sub":
                    os.mkdir(path)
                else:
                    with open(path, "w") as f:
                        f.write("x\n")
                self.assertEqual(c.main(["--dir", d, "check"]), 1)

    def test_check_fails_on_bad_fragment(self):
        with tempfile.TemporaryDirectory() as d:
            with open(os.path.join(d, "oops.md"), "w") as f:
                f.write("x\n")
            self.assertEqual(c.main(["--dir", d, "check"]), 1)


if __name__ == "__main__":
    unittest.main()
