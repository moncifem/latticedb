import os
import stat
import tempfile
import time
import unittest

import helpers  # noqa: F401

from console import source
from console.source import (ConfigError, PsqlSource, SourceError, build_select, check_url,
                            connection_url, first_line, parse_rows, run_bounded, scrub)

FAKE_PSQL = r"""#!/bin/sh
# A stand-in for psql. It records its argv, stdin and PGPASSWORD, then behaves as FAKE_MODE says.
printf '%s\n' "$@" > "$FAKE_DIR/args"
cat > "$FAKE_DIR/stdin"
printf '%s' "${PGPASSWORD:-}" > "$FAKE_DIR/pgpassword"
case "$FAKE_MODE" in
  out) cat "$FAKE_DIR/out" ;;
  fail) printf 'psql: error: connection failed for postgresql://dev:hunter2@127.0.0.1/x password=hunter2\nsecond line\n' >&2; exit 2 ;;
  huge) head -c 5000000 /dev/zero ;;
  hang) sleep 30 ;;
  quiet-fail) exit 3 ;;
esac
"""


def nul(*values):
    return ("\0".join(values) + "\0").encode("utf-8")


EVENT_ROW = ("7", "t1", "ann", "note", "hello", "1700000000")
TASK_ROW = ("t1", "Title", "claimed", "ann", "2", "", "maint", "body", "10", "20", "5", "0", "3")


class UrlTests(unittest.TestCase):
    def test_plain_urls_are_accepted(self):
        for url in ("", "postgresql://dev@127.0.0.1:5432/lattice", "postgres://127.0.0.1/x",
                    "host=127.0.0.1 port=5432 user=dev dbname=lattice"):
            self.assertEqual(check_url(url), url)

    def test_passwords_are_refused(self):
        for url in ("postgresql://dev:s3cret@127.0.0.1:5432/lattice",
                    "postgresql://dev:@127.0.0.1/x",
                    "postgresql://dev@127.0.0.1/x?password=x",
                    "postgresql://dev@127.0.0.1/x?PASSWORD=x",
                    "host=127.0.0.1 password=x",
                    "host=127.0.0.1 Password = 'a b'",
                    "postgresql://dev:p%41ss@127.0.0.1/x"):
            with self.assertRaises(ConfigError, msg=url):
                check_url(url)

    def test_error_does_not_echo_the_password(self):
        try:
            check_url("postgresql://dev:s3cret@127.0.0.1/x")
        except ConfigError as e:
            self.assertNotIn("s3cret", str(e))

    def test_control_characters_are_refused(self):
        with self.assertRaises(ConfigError):
            check_url("postgresql://dev@127.0.0.1/x\n-c")

    def test_cli_url_wins_over_environment(self):
        env = {"LATTICE_BOARD_URL": "postgresql://a@h/1"}
        self.assertEqual(connection_url("postgresql://b@h/2", env), "postgresql://b@h/2")
        self.assertEqual(connection_url(None, env), "postgresql://a@h/1")
        self.assertEqual(connection_url(None, {}), "")

    def test_environment_url_with_password_is_refused(self):
        with self.assertRaises(ConfigError):
            connection_url(None, {"LATTICE_BOARD_URL": "postgresql://a:b@h/1"})


class ScrubTests(unittest.TestCase):
    def test_url_credentials(self):
        self.assertEqual(scrub("could not connect to postgresql://dev:hunter2@host:5432/db"),
                         "could not connect to postgresql://dev:***@host:5432/db")

    def test_key_value_password(self):
        self.assertNotIn("hunter2", scrub("FATAL: password=hunter2 rejected"))
        self.assertNotIn("hunter2", scrub("password = 'hunter2'"))
        self.assertNotIn("a b c", scrub('PASSWORD="a b c"'))

    def test_known_secret_is_removed_anywhere(self):
        self.assertNotIn("zebra99", scrub("it said zebra99 twice zebra99", ["zebra99"]))

    def test_short_or_empty_secrets_are_ignored(self):
        self.assertEqual(scrub("ab", ["ab", ""]), "ab")

    def test_first_line(self):
        self.assertEqual(first_line("\n  \nreal line\nnext"), "real line")
        self.assertEqual(first_line(""), "")
        self.assertEqual(len(first_line("x" * 1000)), 200)


class SelectTests(unittest.TestCase):
    def test_templates(self):
        self.assertEqual(
            build_select(source.EVENT_COLUMNS, "events", "id", False, 50, 12),
            "SELECT id, task_id, author, kind, body, created_at FROM events WHERE id > 12 ORDER BY id LIMIT 50;")
        self.assertEqual(
            build_select(source.DECISION_COLUMNS, "decisions", "id", True, 5),
            "SELECT id, task_id, author, body, created_at FROM decisions ORDER BY id DESC LIMIT 5;")
        self.assertIn("FROM tasks ORDER BY created_seq DESC LIMIT 9;",
                      build_select(source.TASK_COLUMNS, "tasks", "created_seq", True, 9))

    def test_bad_integers_are_refused(self):
        for after in (-1, 2**60, True, "5", 1.5):
            with self.assertRaises(ValueError, msg=repr(after)):
                build_select(source.EVENT_COLUMNS, "events", "id", False, 5, after)
        for limit in (0, -3, 10**7, "5", None):
            with self.assertRaises(ValueError, msg=repr(limit)):
                build_select(source.EVENT_COLUMNS, "events", "id", False, limit)

    def test_unknown_table_is_refused(self):
        with self.assertRaises(ValueError):
            build_select(("id",), "pg_shadow", "id", False, 1)

    def test_only_plain_selects_pass_the_guard(self):
        ok = "SELECT id FROM events ORDER BY id LIMIT 1;"
        self.assertTrue(source._SQL_ALLOWED.fullmatch(ok))
        for bad in ("DELETE FROM events;", "UPDATE tasks SET state = 'done';",
                    "SELECT id FROM events ORDER BY id LIMIT 1; DROP TABLE tasks;",
                    "SELECT id FROM events; SELECT 1;", "INSERT INTO events VALUES (1);",
                    "SELECT id FROM events ORDER BY id LIMIT 1 -- x", "select id from events",
                    "SELECT id FROM events WHERE id > 1 OR 1=1 ORDER BY id LIMIT 1;",
                    "SELECT id FROM events WHERE id > '1' ORDER BY id LIMIT 1;",
                    "COPY events TO '/tmp/x';"):
            self.assertFalse(source._SQL_ALLOWED.fullmatch(bad), bad)


class ParseTests(unittest.TestCase):
    def test_rows(self):
        rows = parse_rows(nul(*EVENT_ROW, "8", "t2", "bob", "claim", "", "1700000005"),
                          source.EVENT_COLUMNS, source.EVENT_INTS)
        self.assertEqual(len(rows), 2)
        self.assertEqual(rows[0], {"id": 7, "task_id": "t1", "author": "ann", "kind": "note",
                                   "body": "hello", "created_at": 1700000000})
        self.assertEqual(rows[1]["body"], "")

    def test_empty_output(self):
        self.assertEqual(parse_rows(b"", source.EVENT_COLUMNS, source.EVENT_INTS), [])

    def test_null_columns(self):
        rows = parse_rows(nul(*TASK_ROW), source.TASK_COLUMNS, source.TASK_INTS, source.TASK_NULLS)
        self.assertIsNone(rows[0]["parent_id"])
        self.assertEqual(rows[0]["owner"], "ann")
        self.assertEqual(rows[0]["priority"], 2)

    def test_text_with_newlines_and_tabs_stays_in_its_column(self):
        # NUL cannot occur in a text value, so hostile text cannot shift columns.
        hostile = "line1\nline2\tTAB\x1e\x1f"
        rows = parse_rows(nul("9", "t", "a", "note", hostile, "5"), source.EVENT_COLUMNS, source.EVENT_INTS)
        self.assertEqual(rows[0]["body"], hostile)
        self.assertEqual(rows[0]["created_at"], 5)

    def test_utf8(self):
        rows = parse_rows(nul("1", "t", "a", "note", "café ✓", "5"), source.EVENT_COLUMNS, source.EVENT_INTS)
        self.assertEqual(rows[0]["body"], "café ✓")

    def test_partial_rows_are_an_error(self):
        with self.assertRaises(SourceError):
            parse_rows(nul("1", "t", "a"), source.EVENT_COLUMNS, source.EVENT_INTS)

    def test_non_numeric_id_is_an_error(self):
        with self.assertRaises(SourceError):
            parse_rows(nul("x", "t", "a", "note", "b", "5"), source.EVENT_COLUMNS, source.EVENT_INTS)

    def test_trailing_newline_is_tolerated(self):
        rows = parse_rows(nul(*EVENT_ROW) + b"\n", source.EVENT_COLUMNS, source.EVENT_INTS)
        self.assertEqual(len(rows), 1)


class RunBoundedTests(unittest.TestCase):
    def test_collects_output_and_exit_code(self):
        rc, out, err = run_bounded(["/bin/sh", "-c", "cat; echo oops >&2; exit 3"], "hi", 5, 1000)
        self.assertEqual((rc, out, err), (3, b"hi", b"oops\n"))

    def test_missing_program(self):
        with self.assertRaises(SourceError) as cm:
            run_bounded(["/nonexistent/psql"], "", 5, 1000)
        self.assertIn("not found", str(cm.exception))

    def test_timeout_kills_the_child(self):
        t0 = time.monotonic()
        with self.assertRaises(SourceError) as cm:
            run_bounded(["/bin/sh", "-c", "sleep 30"], "", 0.4, 1000)
        self.assertLess(time.monotonic() - t0, 5)
        self.assertIn("did not answer", str(cm.exception))

    def test_output_cap(self):
        t0 = time.monotonic()
        with self.assertRaises(SourceError) as cm:
            run_bounded(["/bin/sh", "-c", "head -c 3000000 /dev/zero"], "", 5, 100000)
        self.assertIn("larger than 100000", str(cm.exception))
        self.assertLess(time.monotonic() - t0, 5)

    def test_endless_output_is_cut(self):
        with self.assertRaises(SourceError):
            run_bounded(["/bin/sh", "-c", "yes"], "", 5, 50000)

    def test_stderr_is_capped_not_fatal(self):
        rc, out, err = run_bounded(["/bin/sh", "-c", "head -c 300000 /dev/zero >&2"], "", 5, 1000)
        self.assertEqual(rc, 0)
        self.assertLessEqual(len(err), 65536 + 65536)

    def test_no_shell_is_involved(self):
        # an argument that looks like shell syntax is passed through as plain text
        rc, out, _ = run_bounded(["/bin/echo", "$(touch /nonexistent-dir/x); `id`"], "", 5, 1000)
        self.assertEqual(out.strip(), b"$(touch /nonexistent-dir/x); `id`")


class PsqlSourceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = self.tmp.name
        self.psql = os.path.join(self.dir, "psql")
        with open(self.psql, "w") as f:
            f.write(FAKE_PSQL)
        os.chmod(self.psql, os.stat(self.psql).st_mode | stat.S_IXUSR)

    def src(self, mode="out", out=b"", url="postgresql://dev@127.0.0.1:5999/lattice", **kw):
        with open(os.path.join(self.dir, "out"), "wb") as f:
            f.write(out)
        env = {"FAKE_DIR": self.dir, "FAKE_MODE": mode, "PATH": os.environ.get("PATH", ""),
               "PGPASSWORD": "pw-from-env-9"}
        return PsqlSource(url, psql=self.psql, env=env, **kw)

    def read(self, name):
        with open(os.path.join(self.dir, name)) as f:
            return f.read()

    def test_events_after_parses_and_sends_the_expected_sql(self):
        s = self.src(out=nul(*EVENT_ROW))
        rows = s.events_after(6, 100)
        self.assertEqual(rows[0]["id"], 7)
        self.assertEqual(self.read("stdin").strip(),
                         "SELECT id, task_id, author, kind, body, created_at FROM events "
                         "WHERE id > 6 ORDER BY id LIMIT 100;")

    def test_argument_list_shape(self):
        s = self.src(out=nul(*EVENT_ROW))
        s.events_after(0, 10)
        args = self.read("args").splitlines()
        self.assertEqual(args[:4], ["-X", "-q", "-A", "-t"])
        self.assertIn("-z", args)
        self.assertIn("-0", args)
        self.assertEqual(args[args.index("-d") + 1], "postgresql://dev@127.0.0.1:5999/lattice")
        self.assertNotIn("-c", args)  # SQL goes through stdin, never through an argument
        self.assertNotIn("-f", args)

    def test_no_url_means_no_dash_d(self):
        s = self.src(out=nul(*EVENT_ROW), url="")
        s.events_after(0, 10)
        self.assertNotIn("-d", self.read("args").splitlines())

    def test_password_reaches_psql_only_through_the_environment(self):
        s = self.src(out=nul(*EVENT_ROW))
        s.events_after(0, 10)
        self.assertEqual(self.read("pgpassword"), "pw-from-env-9")
        self.assertNotIn("pw-from-env-9", self.read("args"))
        self.assertNotIn("pw-from-env-9", self.read("stdin"))

    def test_a_url_with_a_password_cannot_even_be_configured(self):
        with self.assertRaises(ConfigError):
            PsqlSource("postgresql://dev:pw@127.0.0.1/x", psql=self.psql)

    def test_nonzero_exit_gives_a_scrubbed_short_error(self):
        s = self.src(mode="fail")
        with self.assertRaises(SourceError) as cm:
            s.events_after(0, 10)
        msg = str(cm.exception)
        self.assertNotIn("hunter2", msg)
        self.assertNotIn("pw-from-env-9", msg)
        self.assertNotIn("second line", msg)  # only the first line is ever shown
        self.assertIn("psql: error", msg)
        self.assertIn("latticed", msg)

    def test_exit_without_stderr_still_has_a_hint(self):
        s = self.src(mode="quiet-fail")
        with self.assertRaises(SourceError) as cm:
            s.events_after(0, 10)
        self.assertIn("query failed", str(cm.exception))

    def test_timeout(self):
        s = self.src(mode="hang", timeout=0.5)
        t0 = time.monotonic()
        with self.assertRaises(SourceError):
            s.events_after(0, 10)
        self.assertLess(time.monotonic() - t0, 5)

    def test_huge_output_is_refused(self):
        s = self.src(mode="huge", max_bytes=100000)
        with self.assertRaises(SourceError) as cm:
            s.events_after(0, 10)
        self.assertIn("larger than", str(cm.exception))

    def test_garbage_output_is_an_error(self):
        s = self.src(out=nul("a", "b"))
        with self.assertRaises(SourceError):
            s.events_after(0, 10)

    def test_missing_psql(self):
        s = PsqlSource("", psql="/nonexistent/psql", env={})
        with self.assertRaises(SourceError):
            s.events_after(0, 10)

    def test_the_guard_runs_before_psql_starts(self):
        s = self.src(out=b"")
        with self.assertRaises(ValueError):
            s._rows("DELETE FROM tasks;", source.TASK_COLUMNS)
        self.assertFalse(os.path.exists(os.path.join(self.dir, "args")))

    def test_tasks_reports_truncation_and_restores_creation_order(self):
        two = nul(*TASK_ROW) + nul("t0", "Older", "open", "", "0", "", "maint", "", "1", "1", "0", "0", "1")
        s = self.src(out=two)
        rows, truncated = s.tasks(1)  # asks for limit + 1 = 2 rows, gets 2
        self.assertTrue(truncated)
        self.assertEqual([r["id"] for r in rows], ["t1"])
        self.assertIn("ORDER BY created_seq DESC LIMIT 2;", self.read("stdin"))
        rows, truncated = s.tasks(5)
        self.assertFalse(truncated)
        self.assertEqual([r["id"] for r in rows], ["t0", "t1"])  # oldest first again

    def test_events_latest_is_returned_oldest_first(self):
        out = nul("9", "t", "a", "note", "b", "5") + nul("8", "t", "a", "note", "b", "4")  # DESC from psql
        s = self.src(out=out)
        rows, truncated = s.events_latest(10)
        self.assertEqual([r["id"] for r in rows], [8, 9])
        self.assertFalse(truncated)

    def test_decisions(self):
        s = self.src(out=nul("4", "t", "a", "because", "7"))
        self.assertEqual(s.decisions_after(3, 10)[0]["body"], "because")
        self.assertEqual(len(s.decisions_latest(10)), 1)

    def test_board_psql_variable_names_the_program(self):
        old = os.environ.get("BOARD_PSQL")
        os.environ["BOARD_PSQL"] = "/custom/psql"
        try:
            self.assertEqual(PsqlSource("").psql, "/custom/psql")
        finally:
            if old is None:
                del os.environ["BOARD_PSQL"]
            else:
                os.environ["BOARD_PSQL"] = old


if __name__ == "__main__":
    unittest.main()
