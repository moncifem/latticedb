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
            "api": "crates/lattice-api/src/exec_host.rs",
            "layers": "layers.toml",
            "workflows": ".github/workflows/pr.yml",
            "protection": "tools/branch-protection.json",
            "rfc-adr": "docs/adr/0019-managed-indexes.md",
            "dependencies": "Cargo.toml",
            "server": "crates/latticed/src/sql.rs",
            "sql-host": "crates/lattice-exec/src/session.rs",
            "engines": "crates/lattice-engines/src/limits.rs",
            "sql": "crates/lattice-sql/src/parser.rs",
            "sdk": "crates/lattice-sdk/src/outbox.rs",
            "foundations": "crates/lattice-abi/src/lib.rs",
            "fuzz": "fuzz/fuzz_targets/pgwire.rs",
            "tla": "tla/Commit.tla",
            "toolchain": "clippy.toml",
            "security-docs": "docs/threat-model.md",
        }
        for area, path in cases.items():
            with self.subTest(area=area):
                self.assertIn(area, g.classify_risk([path]))

    def test_scram_and_nested_paths(self):
        self.assertEqual(g.classify_risk(["crates/lattice-api/src/scram.rs"]), {"auth", "api"})
        self.assertEqual(g.classify_risk(["crates/lattice-api/tests/scram_vectors.rs"]), {"auth", "api"})
        self.assertEqual(g.classify_risk(["docs/rfc/0005-x.md"]), {"rfc-adr"})
        self.assertEqual(g.classify_risk([".github/CODEOWNERS"]), {"workflows"})
        self.assertEqual(g.classify_risk(["tools/pr_governance.py"]), {"protection"})

    def test_dependency_files(self):
        for path in ["Cargo.toml", "Cargo.lock", "deny.toml"]:
            with self.subTest(path=path):
                self.assertEqual(g.classify_risk([path]), {"dependencies"})
        self.assertEqual(g.classify_risk(["Cargo.lock", "docs/x.md"]), {"dependencies"})

    def test_crate_manifests_are_not_dependency_risk(self):
        # Only the root files are the `dependencies` area; crate manifests follow their crate's rule.
        for path in ["fuzz/Cargo.toml", "xtask/Cargo.toml", "my-deny.toml", "docs/Cargo.lock"]:
            with self.subTest(path=path):
                self.assertEqual(g.classify_risk([path]), set())
        self.assertEqual(g.classify_risk(["crates/lattice-core/Cargo.toml"]), {"kernel"})
        self.assertNotIn("dependencies", g.classify_risk(["crates/lattice-seams/Cargo.toml"]))

    def test_sql_host_and_server_paths_trigger_the_gate(self):
        # PR 412 (latticed SQL wiring) merged without the label because none of these were listed.
        expected = {
            "crates/latticed/src/main.rs": "server",
            "crates/latticed/src/lib.rs": "server",
            "crates/latticed/src/sql.rs": "server",
            "crates/latticed/src/transport.rs": "server",
            "crates/latticed/tests/sql_binary.rs": "server",
            "crates/latticed/Cargo.toml": "server",
            "crates/lattice-exec/src/exec.rs": "sql-host",
            "crates/lattice-exec/src/schema.rs": "sql-host",
            "crates/lattice-exec/tests/planted.rs": "sql-host",
            "crates/lattice-exec/Cargo.toml": "sql-host",
            "crates/lattice-api/src/exec_host.rs": "api",
            "crates/lattice-api/src/exec_handler.rs": "api",
            "crates/lattice-api/src/principal.rs": "api",
            "crates/lattice-api/src/session.rs": "api",
            "crates/lattice-api/src/subscribe.rs": "api",
            "crates/lattice-api/src/pgwire.rs": "api",
            "crates/lattice-api/src/cancel.rs": "api",
            "crates/lattice-proto/src/sync.rs": "api",
            "crates/lattice-proto/src/sync/proofs.rs": "api",
            "crates/lattice-proto/src/websocket.rs": "api",
            "crates/lattice-api/src/conformance.rs": "api",
            "crates/lattice-api/src/lib.rs": "api",
            "crates/lattice-api/Cargo.toml": "api",
            "crates/lattice-api/tests/cancel_edges.rs": "api",
            "crates/lattice-engines/src/store.rs": "engines",
            "crates/lattice-engines/src/index.rs": "engines",
            "crates/lattice-engines/src/limits.rs": "engines",
            "crates/lattice-engines/src/store_policy_tests.rs": "engines",
            "crates/lattice-engines/src/rowops.rs": "engines",
            "crates/lattice-engines/src/catalog.rs": "engines",
            "crates/lattice-engines/src/typed.rs": "engines",
            "crates/lattice-engines/src/mvcc.rs": "engines",
            "crates/lattice-engines/src/feed_emit.rs": "engines",
            "crates/lattice-engines/src/lib.rs": "engines",
            "docs/security-test-plan.md": "security-docs",
        }
        for path, area in expected.items():
            with self.subTest(path=path):
                self.assertIn(area, g.classify_risk([path]))
        for path in ["crates/lattice-api/src/auth.rs", "crates/lattice-api/src/scram.rs"]:
            with self.subTest(path=path):
                self.assertIn("auth", g.classify_risk([path]))

    def test_other_security_wire_and_format_paths_are_risky(self):
        for path in [
            "crates/lattice-sql/src/parser.rs", "crates/lattice-sql/src/binder.rs",
            "crates/lattice-sql/src/plan.rs", "crates/lattice-lsl/src/format.rs",
            "crates/lattice-sdk/src/contract.rs", "crates/lattice-sdk/src/contract/x.rs",
            "crates/lattice-sdk/src/outbox.rs", "crates/lattice-sdk/src/mcp.rs",
            "crates/lattice-abi/src/lib.rs", "crates/lattice-seams/src/lib.rs",
            "crates/lattice-sys/src/lib.rs", "crates/lattice-identity/src/lib.rs",
            "fuzz/fuzz_targets/sync_frame.rs", "tools/allow-banned.toml", "tools/quarantine.toml",
            "tools/main_guard.py", "tools/test_main_guard.py", "tools/rebase_pr.py",
            "tools/test_rebase_pr.py", "tools/check_expected_tests.py",
            "tools/test_check_expected_tests.py", "clippy.toml", ".cargo/config.toml",
            "rust-toolchain.toml", "xtask/src/checks.rs", "tla/Fence.tla", "tla/negative/x.tla",
            "docs/review-policy.md", "docs/merging.md",
        ]:
            with self.subTest(path=path):
                self.assertTrue(g.classify_risk([path]))

    def test_files_that_define_or_weaken_the_required_gate_are_risky(self):
        # The audit found these classified "none", so a change to the gate itself needed no review.
        expected = {
            "xtask/src/main.rs": "toolchain",
            "xtask/src/conventions.rs": "toolchain",
            "xtask/src/lex.rs": "toolchain",
            "_typos.toml": "toolchain",
            ".gitleaks.toml": "toolchain",
            ".cargo/mutants.toml": "toolchain",
            "api-snapshots/lattice-core.txt": "toolchain",
            "api-snapshots/new-crate.txt": "toolchain",
            "Dockerfile": "packaging",
            "crates/lattice-testkit/src/slt.rs": "verification",
            "crates/lattice-testkit/tests/scenario.rs": "verification",
            "crates/lattice-sim/src/lib.rs": "verification",
            "crates/lattice-sim/Cargo.toml": "verification",
            "tools/check_expected_tests.py": "verification",
            "tools/check_fuzz_matrices.py": "verification",
            "tools/check_public_api.sh": "verification",
            "tools/check_something_new.py": "verification",
            "tools/test_check_fuzz_matrices.py": "verification",
            "tools/expected-tests/wal-admission.txt": "verification",
            "tools/mutation_campaign.py": "verification",
            "tools/mutation_results.py": "verification",
            "tools/focused_mutation.py": "verification",
            "tools/test_focused_mutation.py": "verification",
            "tools/scorecard.py": "verification",
            "tools/scorecard_metrics.py": "verification",
            "tools/scorecard_replay.py": "verification",
            "tools/test_scorecard.py": "verification",
            "tools/slt_reference.py": "verification",
            "tools/test_slt_reference.py": "verification",
            "tools/review_rules.py": "protection",
            "tools/test_review_rules.py": "protection",
            "tools/fixtures/pr288-check-history.json": "protection",
        }
        for path, area in expected.items():
            with self.subTest(path=path):
                self.assertEqual(g.classify_risk([path]), {area})

    def test_neighbours_of_the_new_gate_paths_stay_unlisted(self):
        for path in [
            "xtask/Cargo.toml", "xtask/src/backlog.rs", "xtask/src/gate.rs", "_typos.toml.bak",
            "docs/Dockerfile", "Dockerfile.dev", "crates/lattice-sim-extras/src/x.rs",
            "crates/lattice-testkit2/src/x.rs", "tools/docs_wiki.py", "tools/test_docs_wiki.py",
            "tools/checks.py", "api-snapshots-notes.md", "docs/api-snapshots/x.txt",
        ]:
            with self.subTest(path=path):
                self.assertEqual(g.classify_risk([path]), set())

    def test_directory_modules_and_new_files_in_the_api_crate_do_not_escape(self):
        # `**/auth*` alone does not match auth/mod.rs, and a new policy.rs or authz.rs would be
        # missed by a file list. The whole crate is covered, with a short exempt list.
        for path in [
            "crates/lattice-api/src/auth/mod.rs", "crates/lattice-api/src/auth/accounts.rs",
            "crates/lattice-api/src/scram/mod.rs", "crates/lattice-api/src/policy.rs",
            "crates/lattice-api/src/authz.rs", "crates/lattice-api/src/exec_host/mod.rs",
            "crates/lattice-api/src/exec_host/policy.rs", "crates/lattice-api/src/new_thing.rs",
        ]:
            with self.subTest(path=path):
                self.assertIn("api", g.classify_risk([path]))
        self.assertIn("auth", g.classify_risk(["crates/lattice-api/src/auth/mod.rs"]))
        self.assertIn("auth", g.classify_risk(["crates/lattice-api/src/scram/mod.rs"]))

    def test_api_exempt_files_are_not_risky(self):
        for path in ["crates/lattice-api/README.md", "crates/lattice-api/src/backpressure.rs"]:
            with self.subTest(path=path):
                self.assertEqual(g.classify_risk([path]), set())
        # The exemption is exact: a lookalike or another crate's README is judged by its own rules.
        self.assertTrue(g.classify_risk(["crates/lattice-api/src/backpressure/mod.rs"]))
        self.assertTrue(g.classify_risk(["crates/lattice-api/src/backpressure_x.rs"]))
        self.assertTrue(g.classify_risk(["crates/latticed/README.md"]))
        # Mixed: an exempt file next to a risky one is still risky.
        self.assertEqual(
            g.classify_risk(["crates/lattice-api/README.md", "crates/lattice-api/src/session.rs"]), {"api"})

    def test_moving_a_file_out_of_latticed_keeps_the_risk(self):
        # A rename reports both paths (see pr_files); the old path inside latticed raises the gate.
        self.assertEqual(
            g.classify_risk(["crates/latticed/src/sql.rs", "crates/lattice-sql-host/src/sql.rs"]),
            {"server"})
        self.assertEqual(
            g.classify_risk(["crates/latticed/src/sql.rs", "crates/lattice-api/src/sql_host.rs"]),
            {"server", "api"})

    def test_merge_tools_are_protection(self):
        for path in ["tools/merge_queue.py", "tools/test_merge_queue.py", "tools/merge_pr.py",
                     "tools/test_merge_pr.py", "tools/merge-pr.sh", "tools/test_merge_pr.sh"]:
            with self.subTest(path=path):
                self.assertEqual(g.classify_risk([path]), {"protection"})

    def test_readmes_follow_their_directory_rule(self):
        # A whole-directory rule covers the README too, as it already does for lattice-core.
        for path in ["crates/latticed/README.md", "crates/lattice-exec/README.md",
                     "crates/lattice-core/README.md", "crates/lattice-engines/README.md"]:
            with self.subTest(path=path):
                self.assertTrue(g.classify_risk([path]))
        # Docs-only change with the label missing fails; with it, passes.
        risk = g.classify_risk(["crates/latticed/README.md"])
        self.assertFalse(g.required_label_ok([], risk))
        self.assertTrue(g.required_label_ok(["reviewed"], risk))

    def test_changelog_tool_is_protection(self):
        # The changelog job runs the pull request's own copy of these files.
        for path in ["tools/changelog.py", "tools/test_changelog.py"]:
            with self.subTest(path=path):
                self.assertEqual(g.classify_risk([path]), {"protection"})

    def test_not_risky(self):
        # Left to the merge queue's universal `reviewed` rule (docs/review-policy.md).
        for path in [
            "crates/lattice-api/src/backpressure.rs",
            "crates/lattice-api/README.md",
            "crates/lattice-exec-extras/src/x.rs",
            "crates/latticed-tools/src/x.rs",
            "crates/lattice-api-extras/src/x.rs",
            "crates/lattice-engines-extras/src/x.rs",
            "tools/foundation_comparison.py",
            "docs/threat-model-notes.md",
            "crates/lattice-core-extras/src/x.rs",
            "tools/docs_wiki.py",
            "docs/gates/phase-1.md",
            "README.md",
            "CHANGELOG.md",
            "fuzz/Cargo.toml",
            "fuzz/README.md",
            "layers.toml.bak",
            "docs/adr-notes.md",
            "my-formats/x.md",
            "crates/lattice-cli/src/main.rs",
            "crates/lattice-bench/src/lib.rs",
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
          "CHANGELOG.md"], {"engines"}),
        ("wal admission",
         ["crates/lattice-core/src/wal.rs", "formats/wal-admission-v2.md",
          "docs/rfc/0008-wal-admission.md", "docs/adr/0022-wal-admission.md"],
         {"kernel", "format", "rfc-adr"}),
        ("scram over tls", ["crates/lattice-api/src/scram.rs", "crates/latticed/src/main.rs"],
         {"auth", "api", "server"}),
        ("ci guard for formats", [".github/workflows/pr.yml", "xtask/src/main.rs"], {"workflows", "toolchain"}),
        ("gate docs only", ["docs/gates/phase-2.md", "README.md", "CHANGELOG.md"], set()),
        ("layer rule change", ["layers.toml", "crates/lattice-sdk/Cargo.toml"], {"layers", "sdk"}),
        ("protection update", ["tools/branch-protection.json", "docs/releasing.md"], {"protection"}),
        ("sql parser", ["crates/lattice-sql/src/parser.rs", "docs/sql-subset.md"], {"sql"}),
        ("api session work",
         ["crates/lattice-api/src/session.rs", "crates/lattice-api/src/pgwire.rs"], {"api"}),
        ("latticed sql wiring (PR 412)",
         ["crates/latticed/src/sql.rs", "crates/latticed/src/main.rs",
          "crates/lattice-api/src/exec_host.rs", "crates/lattice-exec/src/exec.rs",
          "crates/lattice-engines/src/limits.rs", "CHANGELOG.md"],
         {"server", "api", "sql-host", "engines"}),
        ("fuzz target",
         ["fuzz/Cargo.toml", "fuzz/fuzz_targets/feed_decode.rs", "fuzz/README.md"], {"fuzz"}),
        ("fuzz manifest only", ["fuzz/Cargo.toml", "fuzz/README.md"], set()),
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


class CodeownersMatchesClassifier(unittest.TestCase):
    """.github/CODEOWNERS and RISK_RULES list the same risky paths (docs/review-policy.md)."""

    # One concrete path per RISK_RULES glob. A new glob without an entry here fails the test.
    SAMPLES = {
        "formats/**": "formats/wal-v1.md",
        "crates/lattice-core/**": "crates/lattice-core/src/wal.rs",
        "crates/lattice-api/**/auth*": "crates/lattice-api/src/auth.rs",
        "crates/lattice-api/**/auth*/**": "crates/lattice-api/src/auth/mod.rs",
        "crates/lattice-api/**/scram*": "crates/lattice-api/src/scram.rs",
        "crates/lattice-api/**/scram*/**": "crates/lattice-api/src/scram/mod.rs",
        "crates/lattice-api/**": "crates/lattice-api/src/session.rs",
        "crates/lattice-proto/**": "crates/lattice-proto/src/sync.rs",
        "crates/latticed/**": "crates/latticed/src/sql.rs",
        "crates/lattice-exec/**": "crates/lattice-exec/src/exec.rs",
        "crates/lattice-engines/**": "crates/lattice-engines/src/store.rs",
        "crates/lattice-sql/**": "crates/lattice-sql/src/parser.rs",
        "crates/lattice-lsl/**": "crates/lattice-lsl/src/format.rs",
        "crates/lattice-sdk/**": "crates/lattice-sdk/src/outbox.rs",
        "crates/lattice-abi/**": "crates/lattice-abi/src/lib.rs",
        "crates/lattice-seams/**": "crates/lattice-seams/src/lib.rs",
        "crates/lattice-sys/**": "crates/lattice-sys/src/lib.rs",
        "crates/lattice-identity/**": "crates/lattice-identity/src/lib.rs",
        "fuzz/fuzz_targets/**": "fuzz/fuzz_targets/pgwire.rs",
        "tla/**": "tla/Commit.tla",
        "clippy.toml": "clippy.toml",
        "rust-toolchain.toml": "rust-toolchain.toml",
        ".cargo/config.toml": ".cargo/config.toml",
        "xtask/src/checks.rs": "xtask/src/checks.rs",
        "tools/allow-banned.toml": "tools/allow-banned.toml",
        "tools/quarantine.toml": "tools/quarantine.toml",
        "layers.toml": "layers.toml",
        ".github/**": ".github/workflows/pr.yml",
        "tools/branch-protection.json": "tools/branch-protection.json",
        "tools/pr_governance.py": "tools/pr_governance.py",
        "tools/test_pr_governance.py": "tools/test_pr_governance.py",
        "tools/changelog.py": "tools/changelog.py",
        "tools/test_changelog.py": "tools/test_changelog.py",
        "tools/merge_queue.py": "tools/merge_queue.py",
        "tools/test_merge_queue.py": "tools/test_merge_queue.py",
        "tools/merge_pr.py": "tools/merge_pr.py",
        "tools/test_merge_pr.py": "tools/test_merge_pr.py",
        "tools/merge-pr.sh": "tools/merge-pr.sh",
        "tools/test_merge_pr.sh": "tools/test_merge_pr.sh",
        "tools/main_guard.py": "tools/main_guard.py",
        "tools/test_main_guard.py": "tools/test_main_guard.py",
        "tools/rebase_pr.py": "tools/rebase_pr.py",
        "tools/test_rebase_pr.py": "tools/test_rebase_pr.py",
        "tools/review_rules.py": "tools/review_rules.py",
        "tools/test_review_rules.py": "tools/test_review_rules.py",
        "tools/fixtures/**": "tools/fixtures/pr288-check-history.json",
        "xtask/src/main.rs": "xtask/src/main.rs",
        "xtask/src/conventions.rs": "xtask/src/conventions.rs",
        "xtask/src/lex.rs": "xtask/src/lex.rs",
        "_typos.toml": "_typos.toml",
        ".gitleaks.toml": ".gitleaks.toml",
        ".cargo/mutants.toml": ".cargo/mutants.toml",
        "api-snapshots/**": "api-snapshots/lattice-abi.txt",
        "Dockerfile": "Dockerfile",
        "crates/lattice-testkit/**": "crates/lattice-testkit/src/slt.rs",
        "crates/lattice-sim/**": "crates/lattice-sim/src/lib.rs",
        "tools/check_*": "tools/check_public_api.sh",
        "tools/test_check_*": "tools/test_check_expected_tests.py",
        "tools/expected-tests/**": "tools/expected-tests/wal-admission.txt",
        "tools/mutation_*": "tools/mutation_campaign.py",
        "tools/focused_mutation.py": "tools/focused_mutation.py",
        "tools/test_focused_mutation.py": "tools/test_focused_mutation.py",
        "tools/scorecard*": "tools/scorecard_metrics.py",
        "tools/test_scorecard.py": "tools/test_scorecard.py",
        "tools/slt_reference.py": "tools/slt_reference.py",
        "tools/test_slt_reference.py": "tools/test_slt_reference.py",
        "docs/threat-model.md": "docs/threat-model.md",
        "docs/security-test-plan.md": "docs/security-test-plan.md",
        "docs/review-policy.md": "docs/review-policy.md",
        "docs/merging.md": "docs/merging.md",
        "docs/rfc/**": "docs/rfc/0001-x.md",
        "docs/adr/**": "docs/adr/0001-x.md",
        "Cargo.toml": "Cargo.toml",
        "Cargo.lock": "Cargo.lock",
        "deny.toml": "deny.toml",
    }

    @staticmethod
    def owner_patterns():
        path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".github", "CODEOWNERS")
        patterns = []
        with open(path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if line and not line.startswith("#"):
                    patterns.append(line.split()[0])
        return [p for p in patterns if p != "*"]

    @staticmethod
    def owned(pattern, path):
        anchored = pattern.lstrip("/")
        if anchored.endswith("/"):
            return path.startswith(anchored)
        return bool(g._glob_regex(anchored).match(path))

    def test_every_exempt_file_sits_in_its_area_and_is_not_risky(self):
        areas = dict(g.RISK_RULES)
        for area, files in g.RISK_EXEMPT.items():
            for path in files:
                with self.subTest(path=path):
                    self.assertTrue(g.matches(path, areas[area]), "an exemption outside its area is dead")
                    self.assertEqual(g.classify_risk([path]), set())

    def test_every_classifier_glob_has_a_sample(self):
        globs = {glob for _, globs in g.RISK_RULES for glob in globs}
        self.assertEqual(globs, set(self.SAMPLES))

    def test_samples_are_classified(self):
        for glob, path in self.SAMPLES.items():
            with self.subTest(glob=glob):
                self.assertTrue(g.matches(path, [glob]))
                self.assertTrue(g.classify_risk([path]))

    def test_every_classifier_glob_is_owned(self):
        patterns = self.owner_patterns()
        for glob, path in self.SAMPLES.items():
            with self.subTest(glob=glob):
                self.assertTrue(any(self.owned(p, path) for p in patterns), f"{path} has no CODEOWNERS entry")

    def test_every_codeowners_entry_is_risky(self):
        for pattern in self.owner_patterns():
            sample = pattern.lstrip("/")
            sample = sample + "x.md" if sample.endswith("/") else sample.replace("*", "x")
            with self.subTest(pattern=pattern):
                self.assertTrue(self.owned(pattern, sample))
                self.assertTrue(g.classify_risk([sample]), f"{pattern} is owned but not classified as risky")


if __name__ == "__main__":
    unittest.main()
