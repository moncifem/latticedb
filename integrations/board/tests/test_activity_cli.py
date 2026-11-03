"""`board activity`, `activity-list` and `activity-purge` against a fake psql and a fake clock.

The fake never runs SQL, so these tests pin what the helper decides: whether to write at all, which values
reach psql and how (only as -v variables), what is kept between writes, what is retried and what is lost.
Whether Lattice runs the script is tested against a real latticed by tests/activity_devstack.py. Every
expected value below was worked out by hand from the rules in CONVENTIONS.md ("Agent activity"), not by
running the helper and copying what it printed.

Run: python3 -m unittest discover -s integrations/board/tests -p "test_activity_cli.py"
"""

import json
import os
import re
import signal
import stat
import subprocess
import unittest

import actbase
from actbase import ActCli, T0, WRITE, RING_DELETE, vars_of

FILTER_ONE_LINE = r"[\x00-\x1f\x7f]"


class Gate(ActCli):
    def test_it_is_off_unless_board_activity_is_1(self):
        for value in (None, "", "0", "yes", "true", "2"):
            r = self.act("Edit", activity=value)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("BOARD_ACTIVITY=1", r.stderr)
        self.assertEqual(self.calls(), [])
        self.assertFalse(os.path.exists(self.state), "an off helper must not even make its state directory")

    def test_it_needs_an_author(self):
        r = self.act("Edit", author=None)
        self.assertEqual(r.returncode, 1)
        self.assertIn("BOARD_AUTHOR", r.stderr)
        self.assertEqual(self.calls(), [])

    def test_the_author_may_use_only_the_safe_characters(self):
        r = self.act("Edit", author="al ice")
        self.assertEqual(r.returncode, 1)
        self.assertEqual(self.calls(), [])


class Validation(ActCli):
    def assertRefused(self, *args, **kw):
        r = self.board("activity", *args, **kw)
        self.assertEqual(r.returncode, 1, "%s -> %s" % (args, r.stdout))
        self.assertEqual(self.calls(), [], "a refused call must not reach the database: %s" % (args,))
        self.assertEqual(self.writes(), [])

    def test_bad_tools(self):
        for tool in ("", "a b", "x" * 65, "a/b", "a;b", "a'b", "a\tb", "$(x)", "é", "-"):
            if tool == "-":
                continue  # a lone dash is a legal name
            self.assertRefused("--tool", tool)
        self.assertRefused()  # no tool at all

    def test_bad_outcomes_states_sessions_tasks(self):
        self.assertRefused("--tool", "Edit", "--outcome", "fatal")
        self.assertRefused("--tool", "Edit", "--outcome", "OK")
        self.assertRefused("--tool", "Edit", "--state", "sleeping")
        self.assertRefused("--tool", "Edit", "--session", "a b")
        self.assertRefused("--tool", "Edit", "--session", "../x")
        self.assertRefused("--tool", "Edit", "--session", "s" * 65)
        self.assertRefused("--tool", "Edit", "--task", "a b")
        self.assertRefused("--tool", "Edit", "--task", "t" * 65)

    def test_bad_environment_settings(self):
        for v in ("0", "abc", "3601", "-1", "1.5"):
            self.assertRefused("--tool", "Edit", extra={"BOARD_ACTIVITY_INTERVAL": v})
        for v in ("0", "abc", "10001"):
            self.assertRefused("--tool", "Edit", extra={"BOARD_ACTIVITY_KEEP": v})

    def test_unknown_options_and_stray_words(self):
        self.assertRefused("--tool", "Edit", "--fence", "5")
        self.assertRefused("--tool", "Edit", "extra")
        self.assertRefused("--tool")  # an option without a value


class Sanitiser(ActCli):
    """The label is cleaned by the helper whatever the caller sends. Expected values derived by hand from the
    rule: control characters out; backslash to slash; user part of a URL out; everything from the first ? or #
    out; each byte outside printable ASCII to _; 64 bytes; no space at either end."""

    CASES = [
        ("src/main.rs", "src/main.rs"),
        ("a\x1b[31mb", "a[31mb"),
        ("tab\there", "tabhere"),
        ("line1\nline2", "line1line2"),
        ("a\x7fb", "ab"),
        ("https://user:pw@example.com/x?token=abc#f", "https://example.com/x"),
        ("http://host/p?x=1", "http://host/p"),
        ("https://h/p?e=a@b", "https://h/p"),
        ("C:\\Users\\me\\f.txt", "C:/Users/me/f.txt"),
        ("\u00e9", "__"),
        ("\u00e9a", "__a"),
        ("x" * 100, "x" * 64),
        ("  padded  ", "padded"),
        ("a  b", "a  b"),
        ("name?query", "name"),
        ("?only", ""),
        ("#only", ""),
        ("../../etc/passwd", "../../etc/passwd"),
        ("'; DROP TABLE x; --", "'; DROP TABLE x; --"),
        ("", ""),
    ]

    def test_every_case(self):
        for given, expected in self.CASES:
            with self.subTest(given=given):
                self.calls_clear()
                r = self.act("Edit", "--target", given, "--session", "s%d" % abs(hash(given) % 10**6), "--flush")
                self.assertEqual(r.returncode, 0, r.stderr)
                sent = vars_of(self.writes()[-1])["target"]
                self.assertEqual(sent, expected)

    def calls_clear(self):
        if os.path.exists(self.log):
            os.unlink(self.log)
        if os.path.exists(self.log + ".counts"):
            os.unlink(self.log + ".counts")

    def test_a_target_longer_than_the_limit_in_multibyte_characters_is_cut_in_bytes(self):
        r = self.act("Edit", "--target", "\u00e9" * 40, "--flush")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(vars_of(self.writes()[-1])["target"], "_" * 64)


class FirstWrite(ActCli):
    def test_the_first_call_writes_at_once_with_every_value_as_a_variable(self):
        r = self.act("ZZtoolZZ", "--target", "ZZtargetZZ", "--task", "tZZtask", "--session", "ZZsession",
                     "--outcome", "error", "--state", "idle")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("activity written (1 calls)", r.stdout)
        ws = self.writes()
        self.assertEqual(len(ws), 1)
        v = vars_of(ws[0])
        self.assertEqual((v["author"], v["key"], v["session"], v["tool"], v["target"], v["task"], v["outcome"], v["state"]),
                         ("alice", "alice/ZZsession", "ZZsession", "ZZtoolZZ", "ZZtargetZZ", "tZZtask", "error", "idle"))
        self.assertEqual((v["calls"], v["mix"], v["known"], v["keep"], v["over"]), ("1", "ZZtoolZZ=1", "f", "200", "250"))
        sql = ws[0]["sql"]
        for sentinel in ("ZZtoolZZ", "ZZtargetZZ", "tZZtask", "ZZsession", "alice"):
            self.assertNotIn(sentinel, sql, "a value was written into the SQL text instead of a variable")

    def test_the_script_has_the_shape_the_server_needs(self):
        self.act("Edit")
        sql = self.writes()[0]["sql"]
        self.assertTrue(sql.lstrip().startswith("BEGIN;"))
        self.assertIn("COMMIT;", sql)
        self.assertIn("UPDATE counters SET n = n + 1 WHERE name = 'activity'", sql)
        # A read that can return no row must never be \gset: psql treats that as an error.
        for line in sql.splitlines():
            if "FROM agent_activity" in line:
                self.assertNotIn("gset", line, line)
        self.assertNotRegex(sql, r"count\(|COUNT\(|ON CONFLICT")  # the alpha has no aggregates, no upsert
        # everything it writes goes through the two activity tables
        tables = set(re.findall(r"(?:INSERT INTO|UPDATE|DELETE FROM) (\w+)", sql))
        self.assertEqual(tables, {"counters", "agent_activity", "agent_activity_log"})

    def test_the_session_defaults_to_main_or_board_session(self):
        self.act("Edit")
        self.assertEqual(vars_of(self.writes()[-1])["session"], "main")
        self.act("Edit", now=T0 + 100, extra={"BOARD_SESSION": "work-1"})
        self.assertEqual(vars_of(self.writes()[-1])["session"], "work-1")

    def test_state_files_are_private(self):
        self.act("Edit")
        for p in (self.state, self.activity_dir()):
            mode = stat.S_IMODE(os.stat(p).st_mode)
            self.assertEqual(mode, 0o700, p)
        files = os.listdir(self.activity_dir())
        self.assertIn("alice@main.last", files)
        self.assertNotIn("alice@main.pending", files, "the pending file is gone once its calls are written")


class RateLimit(ActCli):
    def test_a_burst_makes_one_write_not_one_per_call(self):
        script = ('i=0; while [ "$i" -lt 120 ]; do sh "$0" activity --tool Edit --target f.rs --task tT1 >/dev/null 2>&1; '
                  'i=$((i + 1)); done')
        self.board("head", rules=self.rules)  # writes the rules file; the call itself is not under test
        os.unlink(self.log)
        r = subprocess.run(["sh", "-c", script, actbase.BOARD], env=self.env(), capture_output=True, text=True, timeout=120)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.writes()), 1, "120 calls inside one interval must cause one write")
        self.assertEqual(len(self.calls()), 1, "and no other connection")
        self.assertEqual(vars_of(self.writes()[0])["calls"], "1")
        # the other 119 are held back, not lost
        pending = os.path.join(self.activity_dir(), "alice@main.pending")
        with open(pending) as f:
            self.assertEqual(len(f.read().splitlines()), 119)
        r = self.act("Edit", "--target", "g.rs", now=T0 + 5)
        self.assertEqual(r.returncode, 0, r.stderr)
        v = vars_of(self.writes()[-1])
        self.assertEqual((v["calls"], v["mix"], v["target"]), ("120", "Edit=120", "g.rs"))
        self.assertEqual(len(self.writes()), 2)

    def test_the_interval_is_the_setting(self):
        self.act("Edit", extra={"BOARD_ACTIVITY_INTERVAL": "10"})
        for t, expect in ((T0 + 9, 1), (T0 + 10, 2), (T0 + 19, 2), (T0 + 20, 3)):
            self.act("Edit", now=t, extra={"BOARD_ACTIVITY_INTERVAL": "10"})
            self.assertEqual(len(self.writes()), expect, "at +%d" % (t - T0))

    def test_the_default_interval_is_five_seconds(self):
        self.act("Edit")
        self.act("Edit", now=T0 + 4)
        self.assertEqual(len(self.writes()), 1)
        self.act("Edit", now=T0 + 5)
        self.assertEqual(len(self.writes()), 2)

    def test_a_clock_that_goes_back_does_not_freeze_the_writes(self):
        self.act("Edit", now=T0 + 1000)
        self.act("Edit", now=T0)
        self.assertEqual(len(self.writes()), 2)

    def test_flush_writes_at_once_and_still_carries_the_held_calls(self):
        self.act("Edit")
        self.act("Read", now=T0 + 1)
        self.act("Bash", now=T0 + 2)
        self.assertEqual(len(self.writes()), 1)
        r = self.act("stop", "--state", "idle", "--flush", now=T0 + 2)
        self.assertEqual(r.returncode, 0, r.stderr)
        v = vars_of(self.writes()[-1])
        self.assertEqual((v["calls"], v["state"], v["tool"], v["mix"]), ("2", "idle", "stop", "Bash=1,Read=1"))

    def test_two_writers_at_once_do_not_both_write_the_same_calls(self):
        # Eight processes call in the same second on a fresh state: the calls are written once between them.
        script = 'for i in 1 2 3 4 5 6 7 8; do sh "$0" activity --tool Edit >/dev/null 2>&1 & done; wait'
        self.board("head")
        os.unlink(self.log)
        subprocess.run(["sh", "-c", script, actbase.BOARD], env=self.env(), capture_output=True, text=True, timeout=60)
        total = sum(int(vars_of(w)["calls"]) for w in self.writes())
        pending = os.path.join(self.activity_dir(), "alice@main.pending")
        held = 0
        if os.path.exists(pending):
            with open(pending) as f:
                held = len(f.read().splitlines())
        self.assertEqual(total + held, 8, "no call may be lost or written twice")
        self.assertLessEqual(len(self.writes()), 2)


class Summary(ActCli):
    def test_the_row_is_the_newest_call_and_the_mix_counts_all(self):
        for i, (tool, target, task) in enumerate([("Edit", "a.rs", ""), ("Edit", "b.rs", "tOne"), ("Bash", "ls", ""),
                                                  ("Read", "c.rs", ""), ("Edit", "d.rs", "")]):
            self.act(tool, "--target", target, *(["--task", task] if task else []), now=T0 + i * 0)
        self.act("Bash", "--target", "make [build]", "--outcome", "error", "--flush")
        v = vars_of(self.writes()[-1])
        self.assertEqual(v["calls"], "6" if len(self.writes()) == 1 else v["calls"])
        # first call wrote alone (Edit a.rs); the other five were held and the flush carries them
        self.assertEqual(vars_of(self.writes()[0])["calls"], "1")
        self.assertEqual((v["calls"], v["tool"], v["target"], v["outcome"], v["task"]), ("5", "Bash", "make [build]", "error", "tOne"))
        self.assertEqual(v["mix"], "Bash=2,Edit=2,Read=1")  # highest count first; the tie by name

    def test_lifecycle_marks_are_not_counted_as_calls(self):
        for tool in ("session-start", "stop", "prompt", "session-end"):
            self.calls_reset()
            r = self.act(tool, "--flush", "--session", "s-" + tool)
            self.assertEqual(r.returncode, 0, r.stderr)
            v = vars_of(self.writes()[-1])
            self.assertEqual((v["calls"], v["mix"]), ("0", ""), tool)

    def calls_reset(self):
        for p in (self.log, self.log + ".counts"):
            if os.path.exists(p):
                os.unlink(p)

    def test_the_mix_stays_well_formed_within_200_characters(self):
        self.act("Edit")
        for i in range(40):
            self.act("Tool%02d_%s" % (i, "x" * 14), now=T0 + 1)
        self.act("Edit", "--flush", now=T0 + 1)
        mix = vars_of(self.writes()[-1])["mix"]
        self.assertLessEqual(len(mix), 200)
        self.assertTrue(all(re.fullmatch(r"[A-Za-z0-9_.-]+=[0-9]+", item) for item in mix.split(",")), mix)
        self.assertTrue(mix.startswith("Edit=1,"))  # the highest count first, then by name

    def test_an_incomplete_pending_line_is_ignored(self):
        self.act("Edit")
        pending = os.path.join(self.activity_dir(), "alice@main.pending")
        with open(pending, "w") as f:
            f.write("half a line without tabs\nBash\t\tok\t\tworking\n")
        self.act("Read", "--flush", now=T0 + 1)
        v = vars_of(self.writes()[-1])
        self.assertEqual((v["calls"], v["mix"]), ("2", "Bash=1,Read=1"))


class Failures(ActCli):
    FAIL = [{"match": WRITE, "err": "psql: error: connection to server at \"127.0.0.1\", port 1 failed: Connection refused",
             "rc": 2}]

    def test_a_failed_write_loses_nothing(self):
        self.rules = self.FAIL
        r = self.act("Edit", extra={"BOARD_ACTIVITY_CONNECT_TRIES": "1"})
        self.assertEqual(r.returncode, 1)
        pending = os.path.join(self.activity_dir(), "alice@main.pending")
        with open(pending) as f:
            self.assertEqual(len(f.read().splitlines()), 1)
        self.assertFalse([n for n in os.listdir(self.activity_dir()) if ".work." in n], "the work file is given back")
        self.rules = list(self.ok_rows)
        self.act("Read", now=T0 + 60)
        v = vars_of(self.writes()[-1])
        self.assertEqual((v["calls"], v["mix"]), ("2", "Edit=1,Read=1"))

    def test_the_wait_doubles_with_every_failure_up_to_300_seconds(self):
        self.rules = self.FAIL
        env = {"BOARD_ACTIVITY_CONNECT_TRIES": "1"}
        t = T0
        attempts = []
        self.act("Edit", extra=env, now=t)
        attempts.append(t)
        # failures so far: 1 -> wait 10, 2 -> 20, 3 -> 40 ... up to 300
        expected_waits = [10, 20, 40, 80, 160, 300, 300]
        for wait in expected_waits:
            before = len(self.writes())
            self.act("Edit", extra=env, now=t + wait - 1)
            self.assertEqual(len(self.writes()), before, "no attempt before the wait of %d s is over" % wait)
            t += wait
            self.act("Edit", extra=env, now=t)
            self.assertEqual(len(self.writes()), before + 1, "an attempt when the wait of %d s is over" % wait)

    def test_a_database_that_is_away_costs_one_connection_not_one_per_call(self):
        self.rules = self.FAIL
        env = {"BOARD_ACTIVITY_CONNECT_TRIES": "1"}
        for _ in range(25):
            self.act("Edit", extra=env, now=T0 + 3)
        self.assertEqual(len(self.calls()), 1)

    def test_a_success_clears_the_backoff(self):
        self.rules = self.FAIL
        env = {"BOARD_ACTIVITY_CONNECT_TRIES": "1"}
        self.act("Edit", extra=env, now=T0)
        self.act("Edit", extra=env, now=T0 + 10)  # second failure: wait is 20 now
        self.rules = list(self.ok_rows)
        self.act("Edit", extra=env, now=T0 + 30)  # succeeds
        n = len(self.writes())
        self.act("Edit", extra=env, now=T0 + 34)
        self.assertEqual(len(self.writes()), n, "deferred")
        self.act("Edit", extra=env, now=T0 + 35)
        self.assertEqual(len(self.writes()), n + 1, "back to the normal interval of 5 s")

    def test_a_missing_table_says_to_run_init(self):
        self.rules = [{"match": WRITE, "err": 'ERROR:  42P01: relation "agent_activity" does not exist', "rc": 1}]
        r = self.act("Edit")
        self.assertEqual(r.returncode, 1)
        self.assertIn("board init", r.stderr)
        pending = os.path.join(self.activity_dir(), "alice@main.pending")
        self.assertTrue(os.path.exists(pending))

    def test_write_conflicts_are_retried_three_times(self):
        self.rules = [{"match": WRITE, "err": "ERROR:  40001: could not serialize access", "rc": 1}]
        r = self.act("Edit")
        self.assertEqual(r.returncode, 1)
        self.assertEqual(len(self.writes()), 3)
        self.rules = [{"match": WRITE, "seq": [{"err": "ERROR:  40001: x", "rc": 1}, {"out": "row\talice/main"}]}]
        r = self.act("Edit", now=T0 + 300)
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_a_connect_failure_is_tried_once_by_default_and_as_often_as_asked(self):
        # One try with a 2 s connect timeout fits the hook's 3 s budget; three would not (about 6 s).
        self.rules = [{"match": WRITE, "err": 'psql: error: connection to server at "x", port 1 failed: Connection refused', "rc": 2}]
        self.act("Edit", extra={"BOARD_ACTIVITY_CONNECT_TRIES": "2"})
        self.assertEqual(len(self.writes()), 2)
        self.calls_reset()
        self.act("Edit", now=T0 + 1000)
        self.assertEqual(len(self.writes()), 1, "the default is one try")

    def calls_reset(self):
        for p in (self.log, self.log + ".counts"):
            if os.path.exists(p):
                os.unlink(p)

    def test_a_writer_killed_midway_leaves_calls_that_the_next_one_adopts(self):
        self.act("Edit")
        d = self.activity_dir()
        with open(os.path.join(d, "alice@main.work.99999999"), "w") as f:
            f.write("Bash\tls\tok\t\tworking\nBash\tls\tok\t\tworking\n")
        self.act("Read", now=T0 + 5)
        v = vars_of(self.writes()[-1])
        self.assertEqual((v["calls"], v["mix"]), ("3", "Bash=2,Read=1"))
        self.assertFalse([n for n in os.listdir(d) if ".work." in n])

    def test_a_work_file_of_a_live_process_is_left_alone(self):
        self.act("Edit")
        d = self.activity_dir()
        mine = os.path.join(d, "alice@main.work.%d" % os.getpid())
        with open(mine, "w") as f:
            f.write("Bash\tls\tok\t\tworking\n")
        self.act("Read", now=T0 + 5)
        self.assertEqual(vars_of(self.writes()[-1])["calls"], "1")
        self.assertTrue(os.path.exists(mine))


class Backoff(ActCli):
    """The failure is counted BEFORE the attempt, so a writer that the hook kills after its 3 s budget still
    leaves the backoff behind; a flush honours the wait once a write has failed; the stamp is whole or absent."""

    BLACKHOLE = '#!/bin/sh\necho x >>"$FAKE_LOG.blackhole"\nexec sleep 30\n'

    def stamp(self):
        with open(os.path.join(self.activity_dir(), "alice@main.last")) as f:
            return f.read().split()

    def hang_and_kill(self, now, seconds=1.5, *args):
        """Run the helper against a psql that never answers (a host that drops packets) and kill its whole process
        group after `seconds`, as the hook does."""
        hole = os.path.join(self.tmp, "psql-hole")
        with open(hole, "w") as f:
            f.write(self.BLACKHOLE)
        os.chmod(hole, 0o755)
        with open(self.rules_path, "w") as f:
            json.dump(self.rules, f)
        env = self.env(now)
        env["BOARD_PSQL"] = hole
        p = subprocess.Popen(["sh", actbase.BOARD, "activity", "--tool", "Edit", *args], env=env, start_new_session=True,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            p.wait(timeout=seconds)
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)
            p.wait()
        return p

    def test_a_writer_killed_while_the_database_does_not_answer_has_left_the_backoff(self):
        self.hang_and_kill(T0)
        self.assertEqual(self.stamp(), [str(T0), "1", "0"], "fails+1 is stored before the attempt")
        # the next calls inside the doubled wait (10 s) make no connection at all
        for t in (T0 + 1, T0 + 5, T0 + 9):
            self.act("Edit", now=t)
        self.assertEqual(self.calls(), [], "no psql call while the backoff runs")
        self.act("Edit", now=T0 + 10)
        self.assertEqual(len(self.writes()), 1, "and one attempt when it is over")
        # the killed writer's calls were not lost: adopted by the next one (the pid of the killed one is gone)
        self.assertEqual(vars_of(self.writes()[0])["calls"], "5")

    def test_each_kill_doubles_the_wait(self):
        self.hang_and_kill(T0)
        self.hang_and_kill(T0 + 10)
        self.assertEqual(self.stamp(), [str(T0 + 10), "2", "0"])
        self.act("Edit", now=T0 + 29)
        self.assertEqual(self.calls(), [], "the wait after two failures is 20 s")

    def test_a_flush_waits_too_once_a_write_has_failed_and_goes_through_before(self):
        self.hang_and_kill(T0)
        self.act("stop", "--state", "idle", "--flush", now=T0 + 2)
        self.assertEqual(self.calls(), [], "a flush does not bypass the backoff")
        self.act("stop", "--state", "idle", "--flush", now=T0 + 10)
        self.assertEqual(len(self.writes()), 1)
        # without a failure a flush still writes at once
        self.calls_reset()
        self.act("Edit", now=T0 + 100)
        self.act("stop", "--state", "idle", "--flush", now=T0 + 101)
        self.assertEqual(len(self.writes()), 2)

    def calls_reset(self):
        for p in (self.log, self.log + ".counts"):
            if os.path.exists(p):
                os.unlink(p)

    def test_the_stamp_is_whole_and_leaves_no_temporary_file(self):
        self.act("Edit")
        self.assertEqual(self.stamp(), [str(T0), "0", "1"])
        self.rules = [{"match": WRITE, "err": 'psql: error: connection to server at "x", port 1 failed: Connection refused', "rc": 2}]
        self.act("Edit", now=T0 + 5, extra={"BOARD_ACTIVITY_CONNECT_TRIES": "1"})
        self.assertEqual(self.stamp(), [str(T0 + 5), "1", "1"])
        self.assertEqual([n for n in os.listdir(self.activity_dir()) if ".tmp." in n], [])

    def test_the_stamp_is_written_before_the_pending_file_is_taken(self):
        with open(actbase.BOARD) as f:
            src = f.read()
        first = src.index('act_stamp_write "$act_stamp" "$act_now_s $((act_fails + 1)) $act_known"')
        take = src.index('mv "$act_pending" "$act_work"')
        self.assertLess(first, take)
        self.assertIn('mv "$asw_tmp" "$1"', src)  # moved into place, not appended or truncated in place


class Names(ActCli):
    """Author and session may both contain dots: the files of a.b + c and a + b.c must not be the same."""

    def test_authors_and_sessions_that_join_to_the_same_text_have_their_own_files(self):
        self.act("Edit", "--session", "b.c", author="a")
        self.act("Edit", "--session", "c", author="a.b")
        files = sorted(os.listdir(self.activity_dir()))
        self.assertIn("a@b.c.last", files)
        self.assertIn("a.b@c.last", files)
        self.assertEqual(len(self.writes()), 2, "each pair wrote at once: they do not share a stamp")

    def test_a_purge_of_one_author_leaves_the_files_of_the_other(self):
        self.act("Edit", "--session", "c", author="a.b")
        self.act("Edit", "--session", "b.c", author="a")
        self.assertEqual(self.board("activity-purge", author="a").returncode, 0)
        files = os.listdir(self.activity_dir())
        self.assertEqual([f for f in files if f.startswith("a@")], [])
        self.assertIn("a.b@c.last", files, "a.b's counters are not a's")


class SessionRow(ActCli):
    def test_the_helper_inserts_first_then_updates_and_inserts_again_when_the_row_is_gone(self):
        self.act("Edit")
        self.assertEqual(vars_of(self.writes()[-1])["known"], "f")
        self.act("Edit", now=T0 + 5)
        self.assertEqual(vars_of(self.writes()[-1])["known"], "t")
        # the next answer has no "row" line: the row was purged elsewhere
        self.rules = [{"match": WRITE, "out": ""}]
        self.act("Edit", now=T0 + 10)
        self.assertEqual(vars_of(self.writes()[-1])["known"], "t")
        self.act("Edit", now=T0 + 15)
        self.assertEqual(vars_of(self.writes()[-1])["known"], "f", "the lost row is inserted again")

    def test_an_insert_that_finds_the_row_is_repeated_as_an_update(self):
        self.rules = [{"match": WRITE, "seq": [{"err": "ERROR:  23505: duplicate key value", "rc": 1},
                                               {"out": "row\talice/main"}]}]
        r = self.act("Edit")
        self.assertEqual(r.returncode, 0, r.stderr)
        ws = self.writes()
        self.assertEqual([vars_of(w)["known"] for w in ws], ["f", "t"])
        self.assertEqual(len(self.matching(r"INSERT INTO agent_activity_log")), 2)
        self.act("Edit", now=T0 + 5)
        self.assertEqual(vars_of(self.writes()[-1])["known"], "t")

    def test_the_script_updates_or_inserts_the_row_by_the_known_flag(self):
        self.act("Edit")
        sql = self.writes()[0]["sql"]
        self.assertRegex(sql, r"(?s)\\if :known\s+UPDATE agent_activity SET .*tool_calls = tool_calls \+ :calls.*\\else\s+INSERT INTO agent_activity \(", )
        self.assertRegex(sql, r"WHERE id = :'key';\s*\\else")


class Ring(ActCli):
    def test_the_oldest_rows_go_when_the_ring_is_over(self):
        self.rules = [{"match": WRITE, "out": "ring-over\t900\nring-cut\t700\nrow\talice/main"},
                      {"match": RING_DELETE, "out": ""}]
        r = self.act("Edit", extra={"BOARD_ACTIVITY_KEEP": "7"})
        self.assertEqual(r.returncode, 0, r.stderr)
        v = vars_of(self.writes()[0])
        self.assertEqual((v["keep"], v["over"]), ("7", "57"))
        deletes = self.matching(RING_DELETE)
        deletes = [c for c in deletes if "INSERT INTO" not in c["sql"]]
        self.assertEqual(len(deletes), 1)
        dv = vars_of(deletes[0])
        self.assertEqual((dv["author"], dv["cut"]), ("alice", "700"))
        self.assertIn("id <= :cut", deletes[0]["sql"])

    def test_no_delete_while_the_ring_is_not_over(self):
        self.rules = [{"match": WRITE, "out": "ring-cut\t700\nrow\talice/main"}, {"match": RING_DELETE, "out": ""}]
        self.act("Edit")
        deletes = [c for c in self.matching(RING_DELETE) if "INSERT INTO" not in c["sql"]]
        self.assertEqual(deletes, [])

    def test_a_cut_that_is_not_a_number_is_not_used(self):
        self.rules = [{"match": WRITE, "out": "ring-over\t900\nring-cut\t7; DROP\nrow\talice/main"}]
        self.act("Edit")
        self.assertEqual([c for c in self.matching(RING_DELETE) if "INSERT INTO" not in c["sql"]], [])

    def test_a_failed_prune_does_not_fail_the_write(self):
        self.rules = [{"match": WRITE, "out": "ring-over\t900\nring-cut\t700\nrow\talice/main"},
                      {"match": r"^DELETE FROM agent_activity_log", "err": "ERROR: boom", "rc": 1}]
        r = self.act("Edit")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_the_script_also_expires_old_session_rows_of_the_agent(self):
        self.act("Edit")
        sql = self.writes()[0]["sql"]
        self.assertRegex(sql, r"DELETE FROM agent_activity WHERE agent = :'author' AND last_seen < :dbnow - 604800;")

    def test_the_ring_selects_are_read_only_and_never_gset(self):
        self.act("Edit")
        sql = self.writes()[0]["sql"]
        self.assertRegex(sql, r"SELECT 'ring-over', id FROM agent_activity_log WHERE agent = :'author' ORDER BY id DESC LIMIT 1 OFFSET :over;")
        self.assertRegex(sql, r"SELECT 'ring-cut', id FROM agent_activity_log WHERE agent = :'author' ORDER BY id DESC LIMIT 1 OFFSET :keep;")


class Unusable(ActCli):
    def fallback_dir(self):
        return os.path.join(self.tmp, "lattice-board-activity-%d" % os.getuid())

    def test_an_unusable_state_directory_limits_writes_to_one_per_30_seconds(self):
        with open(self.state, "w") as f:  # a regular file where the directory should be
            f.write("x")
        self.act("Edit", now=T0)
        self.assertEqual(len(self.writes()), 1)
        self.act("Edit", now=T0 + 10)
        self.act("Edit", now=T0 + 29)
        self.assertEqual(len(self.writes()), 1)
        self.act("Edit", now=T0 + 30)
        self.assertEqual(len(self.writes()), 2)
        self.assertEqual(stat.S_IMODE(os.stat(self.fallback_dir()).st_mode), 0o700)

    def test_a_symlinked_state_directory_is_not_followed(self):
        target = os.path.join(self.tmp, "elsewhere")
        os.mkdir(target, 0o700)
        os.symlink(target, self.state)
        self.act("Edit", now=T0)
        self.act("Edit", now=T0 + 10)
        self.assertEqual(len(self.writes()), 1, "the fallback limit of 30 s applies")
        self.assertEqual(os.listdir(target), [], "nothing was written through the link")

    def test_an_activity_subdirectory_that_is_a_symlink_is_not_followed(self):
        os.mkdir(self.state, 0o700)
        target = os.path.join(self.tmp, "elsewhere")
        os.mkdir(target, 0o700)
        os.symlink(target, os.path.join(self.state, "activity"))
        self.act("Edit", now=T0)
        self.assertEqual(os.listdir(target), [], "nothing may be written through the link")
        self.act("Edit", now=T0 + 10)
        self.assertEqual(len(self.writes()), 1, "the fallback limit of 30 s applies")

    def test_a_state_directory_with_loose_permissions_is_not_used(self):
        os.mkdir(self.state, 0o755)
        os.chmod(self.state, 0o755)
        self.act("Edit", now=T0)
        self.act("Edit", now=T0 + 10)
        self.assertEqual(len(self.writes()), 1)
        self.assertFalse(os.path.exists(os.path.join(self.state, "activity")))

    def test_when_nothing_private_exists_nothing_is_written(self):
        with open(self.state, "w") as f:
            f.write("x")
        blocker = os.path.join(self.tmp, "blocker")
        with open(blocker, "w") as f:
            f.write("x")
        # TMPDIR itself must exist for the helper's own temporary files, so the fallback name is the obstacle
        os.symlink(blocker, self.fallback_dir())
        r = self.act("Edit", now=T0)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("nothing recorded", r.stderr)
        self.assertEqual(self.calls(), [])

    def test_a_symlinked_pending_file_is_refused(self):
        self.act("Edit")
        d = self.activity_dir()
        victim = os.path.join(self.tmp, "victim")
        with open(victim, "w") as f:
            f.write("keep")
        os.symlink(victim, os.path.join(d, "alice@main.pending"))
        r = self.act("Edit", now=T0 + 10)
        self.assertEqual(r.returncode, 1)
        self.assertIn("symbolic link", r.stderr)
        with open(victim) as f:
            self.assertEqual(f.read(), "keep")


class ListAndPurge(ActCli):
    def test_activity_list_prints_tagged_lines_and_needs_no_author(self):
        rules = [{"match": r"AS bigint", "out": "1000"},
                 {"match": r"FROM agent_activity ORDER BY|FROM agent_activity\s+ORDER", "out": "alice/s1\talice\ts1\t\tEdit\tf.rs\tok\tworking\t3\t990\t900"},
                 {"match": r"FROM agent_activity_log", "out": "5\talice\ts1\t\tEdit\tf.rs\tok\t3\tEdit=3\t990"}]
        r = self.board("activity-list", author=None, activity=None, rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        lines = r.stdout.splitlines()
        self.assertEqual(lines[0], "now\t1000")
        self.assertEqual(lines[1], "session\talice/s1\talice\ts1\t\tEdit\tf.rs\tok\tworking\t3\t990\t900")
        self.assertEqual(lines[2], "log\t5\talice\ts1\t\tEdit\tf.rs\tok\t3\tEdit=3\t990")
        log_call = self.matching(r"FROM agent_activity_log")[0]
        v = vars_of(log_call)
        self.assertEqual((v["after"], v["lim"]), ("0", "100"))
        self.assertIn("id > :after", log_call["sql"])

    def test_activity_list_filters_and_validates(self):
        rules = [{"match": r"AS bigint", "out": "1000"}]
        r = self.board("activity-list", "--agent", "bob", "--after", "7", "--limit", "5", author=None, activity=None, rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        v = vars_of(self.matching(r"FROM agent_activity_log")[0])
        self.assertEqual((v["agent"], v["after"], v["lim"]), ("bob", "7", "5"))
        for bad in (["--agent", "a b"], ["--after", "-1"], ["--limit", "0"], ["--limit", "1001"], ["--after", "x"], ["junk"]):
            self.assertEqual(self.board("activity-list", *bad, author=None, activity=None).returncode, 1, bad)

    def test_activity_list_on_an_old_board_says_to_run_init(self):
        rules = [{"match": r"AS bigint", "out": "1000"},
                 {"match": r"FROM agent_activity", "err": 'ERROR:  42P01: relation "agent_activity" does not exist', "rc": 1}]
        r = self.board("activity-list", author=None, activity=None, rules=rules)
        self.assertEqual(r.returncode, 1)
        self.assertIn("board init", r.stderr)

    def test_purge_deletes_the_authors_rows_and_local_counters(self):
        self.act("Edit")
        self.act("Edit", now=T0 + 1)  # held
        rules = [{"match": r"SELECT id FROM agent_activity_log", "out": "1\n2\n3"}]
        r = self.board("activity-purge", rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("3 log rows deleted", r.stdout)
        for table in ("agent_activity_log", "agent_activity"):
            deletes = self.matching(r"DELETE FROM %s WHERE agent = :'agent'" % table)
            self.assertEqual(len(deletes), 1, table)
            self.assertEqual(vars_of(deletes[0])["agent"], "alice")
        self.assertEqual(os.listdir(self.activity_dir()), [], "the local counters of the agent are removed")

    def test_purge_of_another_agent_or_everybody_needs_yes(self):
        self.assertEqual(self.board("activity-purge", "--agent", "bob").returncode, 1)
        self.assertEqual(self.board("activity-purge", "--all").returncode, 1)
        self.assertEqual(self.matching(r"DELETE"), [])
        r = self.board("activity-purge", "--agent", "bob", "--yes")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(vars_of(self.matching(r"DELETE FROM agent_activity_log")[0])["agent"], "bob")
        r = self.board("activity-purge", "--all", "--yes")
        self.assertEqual(r.returncode, 0, r.stderr)
        everything = [c for c in self.matching(r"DELETE FROM agent_activity_log") if ":'agent'" not in c["sql"]]
        self.assertEqual(len(everything), 1)

    def test_purge_works_without_the_switch_and_needs_an_author(self):
        self.assertEqual(self.board("activity-purge", activity=None).returncode, 0)
        self.assertEqual(self.board("activity-purge", author=None).returncode, 1)

    def test_all_and_agent_exclude_each_other(self):
        for args in (["--all", "--agent", "bob", "--yes"], ["--agent", "bob", "--all", "--yes"]):
            r = self.board("activity-purge", *args)
            self.assertEqual(r.returncode, 1, args)
            self.assertIn("exclude each other", r.stderr)
        self.assertEqual(self.matching(r"DELETE"), [], "nothing was deleted")

    def test_purge_refuses_bad_arguments(self):
        for bad in (["--agent", "a b"], ["--agent"], ["junk"], ["--agent", "x y", "--yes"]):
            self.assertEqual(self.board("activity-purge", *bad).returncode, 1, bad)


class Schema(ActCli):
    def read_schema(self):
        with open(os.path.join(actbase.BOARD_DIR, "schema.sql")) as f:
            return f.read()

    def columns(self, table):
        m = re.search(r"CREATE TABLE IF NOT EXISTS %s \((.*?)\n\);" % table, self.read_schema(), re.S)
        self.assertIsNotNone(m, table)
        return [line.split()[0] for line in m.group(1).splitlines() if line.strip() and not line.strip().startswith("--")]

    def test_the_tables_exist_without_alter(self):
        s = self.read_schema()
        for t in ("agent_activity", "agent_activity_log"):
            self.assertIn("CREATE TABLE IF NOT EXISTS %s (" % t, s)
        code = "\n".join(l for l in s.splitlines() if not l.lstrip().startswith("--"))
        self.assertNotIn("ALTER", code.upper())

    def test_every_column_the_script_writes_exists_and_every_column_is_written(self):
        self.act("Edit")
        sql = self.writes()[0]["sql"]
        ins = re.search(r"INSERT INTO agent_activity \(([^)]*)\)", sql).group(1)
        self.assertEqual([c.strip() for c in ins.split(",")], self.columns("agent_activity")[:len(self.columns("agent_activity"))])
        log = re.search(r"INSERT INTO agent_activity_log \(([^)]*)\)", sql).group(1)
        self.assertEqual([c.strip() for c in log.split(",")], self.columns("agent_activity_log"))

    def test_the_script_is_a_quoted_heredoc_so_the_shell_expands_nothing_into_it(self):
        with open(actbase.BOARD) as f:
            src = f.read()
        body = src[src.index("act_sql() {"):src.index("# act_prune:")]
        self.assertIn("<<'SQL'", body)
        script = body[body.index("<<'SQL'"):body.rindex("SQL\n}")]
        self.assertNotIn("$", script, "a shell expansion inside the script would put a value into the SQL text")
        self.assertNotRegex(script, r"\|\|\s*'", "no string concatenation of a value into a statement")

    def test_init_seeds_the_activity_counter(self):
        rules = [{"match": r"SELECT n FROM counters", "out": ""}]
        r = self.board("init", activity=None, rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        inserts = self.matching(r"INSERT INTO counters")
        self.assertEqual(sorted(vars_of(c)["name"] for c in inserts), ["activity", "events"])

    def test_init_does_not_reseed_a_counter_that_exists(self):
        rules = [{"match": r"SELECT n FROM counters", "out": "5"}]
        self.board("init", activity=None, rules=rules)
        self.assertEqual(self.matching(r"INSERT INTO counters"), [])

    def test_the_usage_text_lists_the_commands(self):
        r = self.board("help", activity=None)
        for word in ("activity --tool", "activity-list", "activity-purge", "BOARD_ACTIVITY"):
            self.assertIn(word, r.stdout)


if __name__ == "__main__":
    unittest.main()
