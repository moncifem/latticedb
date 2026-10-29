import argparse
import json
import subprocess
from unittest.mock import patch

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import pr_governance as g  # noqa: E402


class ClassifyRisk(unittest.TestCase):
    def test_each_area(self):
        cases = {
            "format": "formats/wal-v1.md",
            "kernel": "crates/lattice-core/src/wal.rs",
            "auth": "crates/lattice-api/src/auth.rs",
            "layers": "layers.toml",
            "workflows": ".github/workflows/pr.yml",
            "protection": "tools/branch-protection.json",
            "rfc-adr": "docs/adr/0019-managed-indexes.md",
        }
        for area, path in cases.items():
            with self.subTest(area=area):
                self.assertEqual(g.classify_risk([path]), {area})

    def test_scram_and_nested_paths(self):
        self.assertEqual(g.classify_risk(["crates/lattice-api/src/scram.rs"]), {"auth"})
        self.assertEqual(g.classify_risk(["crates/lattice-api/tests/scram_vectors.rs"]), {"auth"})
        self.assertEqual(g.classify_risk(["docs/rfc/0005-x.md"]), {"rfc-adr"})
        self.assertEqual(g.classify_risk([".github/CODEOWNERS"]), {"workflows"})
        self.assertEqual(g.classify_risk(["tools/pr_governance.py"]), {"protection"})

    def test_not_risky(self):
        for path in [
            "crates/lattice-api/src/session.rs",
            "crates/lattice-api/src/pgwire.rs",
            "crates/lattice-engines/src/lib.rs",
            "crates/lattice-core-extras/src/x.rs",
            "tools/scorecard.py",
            "docs/gates/phase-1.md",
            "README.md",
            "CHANGELOG.md",
            "fuzz/Cargo.toml",
            "layers.toml.bak",
            "docs/adr-notes.md",
            "my-formats/x.md",
        ]:
            with self.subTest(path=path):
                self.assertEqual(g.classify_risk([path]), set())

    def test_multiple_areas(self):
        paths = ["formats/a.md", "docs/rfc/1.md", "docs/adr/2.md", "src/x.rs"]
        self.assertEqual(g.classify_risk(paths), {"format", "rfc-adr"})
        self.assertEqual(g.classify_risk([]), set())


class RequiredLabel(unittest.TestCase):
    def test_label_rules(self):
        self.assertTrue(g.required_label_ok([], set()))
        self.assertTrue(g.required_label_ok(["type: ci"], set()))
        self.assertFalse(g.required_label_ok([], {"kernel"}))
        self.assertFalse(g.required_label_ok(["type: ci", "overlap"], {"kernel"}))
        self.assertTrue(g.required_label_ok(["reviewed"], {"kernel", "format"}))


class Overlap(unittest.TestCase):
    def test_overlap_and_exempt(self):
        mine = ["a.rs", "CHANGELOG.md", "docs/gates/phase-1.md", "fuzz/README.md",
                "fuzz/Cargo.toml", "fuzz/fuzz_targets/x.rs", "b.rs"]
        other = ["CHANGELOG.md", "docs/gates/phase-1.md", "fuzz/README.md", "fuzz/Cargo.toml",
                 "fuzz/fuzz_targets/x.rs", "a.rs", "c.rs"]
        self.assertEqual(g.overlap(mine, other, g.EXEMPT_GLOBS), ["a.rs", "fuzz/fuzz_targets/x.rs"])

    def test_no_overlap(self):
        self.assertEqual(g.overlap(["a"], ["b"]), [])
        self.assertEqual(g.overlap([], ["b"]), [])

    def test_by_pr(self):
        res = g.overlaps_by_pr(
            ["a", "b", "CHANGELOG.md"], {7: ["a", "CHANGELOG.md"], 8: ["z"], 9: ["b", "a"]}
        )
        self.assertEqual(res, {7: ["a"], 9: ["a", "b"]})

    def test_comment_has_marker_once(self):
        body = g.render_comment({5: ["a.rs"], 3: ["b.rs"]})
        self.assertEqual(body.count(g.COMMENT_MARKER), 1)
        self.assertLess(body.index("#3"), body.index("#5"))


class PastPrs(unittest.TestCase):
    """Realistic path sets from earlier work and the risk areas they must raise."""

    CASES = [
        ("managed index engine",
         ["crates/lattice-engines/src/index.rs", "crates/lattice-engines/tests/managed.rs",
          "CHANGELOG.md"], set()),
        ("wal admission",
         ["crates/lattice-core/src/wal.rs", "formats/wal-admission-v2.md",
          "docs/rfc/0008-wal-admission.md", "docs/adr/0022-wal-admission.md"],
         {"kernel", "format", "rfc-adr"}),
        ("scram over tls", ["crates/lattice-api/src/scram.rs", "crates/latticed/src/main.rs"], {"auth"}),
        ("ci guard for formats", [".github/workflows/pr.yml", "xtask/src/main.rs"], {"workflows"}),
        ("gate docs only", ["docs/gates/phase-2.md", "README.md", "CHANGELOG.md"], set()),
        ("layer rule change", ["layers.toml", "crates/lattice-sdk/Cargo.toml"], {"layers"}),
        ("protection update", ["tools/branch-protection.json", "docs/releasing.md"], {"protection"}),
        ("sql parser", ["crates/lattice-sql/src/parser.rs", "docs/sql-subset.md"], set()),
        ("api session work",
         ["crates/lattice-api/src/session.rs", "crates/lattice-api/src/pgwire.rs"], set()),
        ("fuzz target",
         ["fuzz/Cargo.toml", "fuzz/fuzz_targets/feed_decode.rs", "fuzz/README.md"], set()),
    ]

    def test_table(self):
        for name, paths, expected in self.CASES:
            with self.subTest(pr=name):
                areas = g.classify_risk(paths)
                self.assertEqual(areas, expected)
                self.assertEqual(g.required_label_ok([], areas), not expected)
                self.assertTrue(g.required_label_ok(["reviewed"], areas))


class ApiBoundaries(unittest.TestCase):
    # Raw paginated GitHub PR-files response shape, including both directions across risk paths.
    FILES = [[
        {"status": "renamed", "filename": "docs/former-wal.md",
         "previous_filename": "formats/wal-v1.md"},
        {"status": "renamed", "filename": ".github/workflows/new.yml",
         "previous_filename": "tools/old.yml"},
        {"status": "modified", "filename": "README.md"},
    ], [{"status": "renamed", "filename": "docs/former-core.rs",
         "previous_filename": "crates/lattice-core/src/log.rs"}]]

    def test_renames_in_and_out_keep_both_risk_and_claim_paths(self):
        with patch.object(g, "_gh", return_value=json.dumps(self.FILES)) as gh:
            paths = g.pr_files("owner/repo", 7)
        self.assertIn("--slurp", gh.call_args.args[0])
        self.assertEqual(g.classify_risk(paths), {"format", "kernel", "workflows"})
        self.assertFalse(g.required_label_ok([], g.classify_risk(paths)))
        self.assertEqual(g.overlap(paths, ["formats/wal-v1.md", "tools/old.yml"]),
                         ["formats/wal-v1.md", "tools/old.yml"])
        self.assertIn(".github/workflows/new.yml", paths)

    def test_path_with_newline_is_not_split_into_fake_paths(self):
        with patch.object(g, "_gh", return_value=json.dumps([[{"filename": "docs/a\nformats/b"}]])):
            self.assertEqual(g.pr_files("owner/repo", 7), ["docs/a\nformats/b"])

    def test_comment_lookup_requires_the_workflow_bot(self):
        comments = [[
            {"id": 1, "body": g.COMMENT_MARKER, "user": {"login": "contributor", "type": "User"}},
            {"id": 2, "body": g.COMMENT_MARKER, "user": {"login": "other[bot]", "type": "Bot"}},
            {"id": 3, "body": g.COMMENT_MARKER, "user": {"login": "github-actions[bot]", "type": "Bot"}},
        ]]
        with patch.object(g, "_gh", side_effect=[json.dumps(comments), ""]) as gh:
            g._upsert_comment("owner/repo", 7, "body", create=True)
        self.assertIn("repos/owner/repo/issues/comments/3", gh.call_args_list[1].args[0])

    def test_no_overlap_does_not_delete_a_missing_label(self):
        with patch.object(g, "pr_files", return_value=[]), patch.object(g, "_gh", side_effect=["", "[[]]"]) as gh, patch.object(g, "_upsert_comment"):
            self.assertEqual(g.cmd_claims(argparse.Namespace(repo="owner/repo", pr=7, dry_run=False)), 0)
        self.assertFalse(any("DELETE" in call.args[0] for call in gh.call_args_list))

    def test_label_delete_api_errors_are_not_hidden(self):
        for status in [403, 404, 429, 500]:
            with self.subTest(status=status):
                failure = subprocess.CalledProcessError(1, ["gh"], stderr=f"HTTP {status}")
                with patch.object(g, "pr_files", return_value=[]), patch.object(g, "_gh", side_effect=["", '[[{"name":"overlap"}]]', failure]), patch.object(g, "_upsert_comment") as comment:
                    with self.assertRaises(subprocess.CalledProcessError):
                        g.cmd_claims(argparse.Namespace(repo="owner/repo", pr=7, dry_run=False))
                comment.assert_not_called()


if __name__ == "__main__":
    unittest.main()
