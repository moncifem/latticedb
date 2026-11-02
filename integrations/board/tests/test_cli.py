"""Tests of the shell helper (integrations/board/board) against a fake psql.

The fake (fakepsql.py) answers each statement from a canned rule, so these tests exercise the
helper's own logic: argument checks, exit codes, fence files, which values reach psql and how,
the order of reads, the backoff, the dependency walk. They do NOT run the SQL: whether Lattice
accepts and executes it is tested by integrations/board/test.sh against a real latticed.

Run: python3 -m unittest discover -s integrations/board/tests
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
BOARD = os.path.join(os.path.dirname(HERE), "board")
FAKE = os.path.join(HERE, "fakepsql.py")

# Patterns that identify a statement of the helper.
TASK_INFO = r"SELECT state, coalesce\(owner"
LEASE_ROW = r"SELECT task_id FROM task_leases WHERE task_id ="
BLOCKED_BY = r"SELECT blocked_by FROM task_leases"
DONE_IDS = r"state = 'done' AND id IN"
DB_NOW = r"AS bigint\);"
CLAIM_TX = r"UPDATE tasks SET owner = :'author', state = 'claimed'"
HEARTBEAT_TX = r"AS until"
NOTE_TX = r"'note', :'text'"
DONE_TX = r"SET state = 'done'"
FAIL_TX = r"'fail', :'text'"
RELEASE_TX = r"'release', ''"
REAP_LIST = r"lease_until > 0 AND lease_until < :dbnow\s+ORDER BY"
REAP_TX = r"'expired'"
FENCED_TX = r"'fenced'"
ATTEMPTS = r"SELECT attempts FROM task_leases"
CANDIDATES = r"ORDER BY priority DESC, created_seq LIMIT 50 OFFSET"
CAND_LEASES = r"SELECT task_id, not_before, blocked_by"
CAP_QUERY = r"state = 'claimed' AND owner = :'author' ORDER BY id LIMIT"
APPROVE_TX = r"'approve', :'text'"
CANCEL_TX = r"SET state = 'cancelled'"
BLOCK_TX = r"SELECT \(blocked_by = :'old'\)"
EXISTS_IN = r"SELECT id FROM tasks WHERE id IN"


class Cli(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="board-cli-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.log = os.path.join(self.tmp, "calls.jsonl")
        self.rules_path = os.path.join(self.tmp, "rules.json")
        self.state = os.path.join(self.tmp, "state")
        self.psql = os.path.join(self.tmp, "psql")
        with open(self.psql, "w") as f:
            f.write('#!/bin/sh\nexec "%s" "%s" "$@"\n' % (sys.executable, FAKE))
        os.chmod(self.psql, 0o755)
        self.rules = []

    # -- plumbing
    def run_board(self, *args, author="alice", env=None, rules=None):
        if rules is not None:
            self.rules = rules
        with open(self.rules_path, "w") as f:
            json.dump(self.rules, f)
        e = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": self.tmp, "TMPDIR": self.tmp,
             "BOARD_PSQL": self.psql, "BOARD_STATE_DIR": self.state, "FAKE_LOG": self.log,
             "FAKE_RULES": self.rules_path}
        if author:
            e["BOARD_AUTHOR"] = author
        e.update(env or {})
        return subprocess.run(["sh", BOARD, *args], env=e, capture_output=True, text=True, timeout=60)

    def calls(self):
        if not os.path.exists(self.log):
            return []
        with open(self.log) as f:
            return [json.loads(line) for line in f if line.strip()]

    def calls_matching(self, pattern):
        import re
        return [c for c in self.calls() if re.search(pattern, c["sql"], re.S)]

    def vars_of(self, call):
        out = {}
        argv = call["argv"]
        for i, a in enumerate(argv):
            if a == "-v" and "=" in argv[i + 1]:
                k, v = argv[i + 1].split("=", 1)
                out[k] = v
        return out

    def fence_file(self, task, author="alice"):
        return os.path.join(self.state, "%s.%s" % (author, task))

    def put_fence(self, task, value, author="alice"):
        os.makedirs(self.state, mode=0o700, exist_ok=True)
        with open(self.fence_file(task, author), "w") as f:
            f.write("%s\n" % value)

    def info(self, state="claimed", owner="alice", seq=9):
        return {"match": TASK_INFO, "out": "%s\t%s\t%s" % (state, owner, seq)}


class Validation(Cli):
    def test_claim_lease_out_of_range(self):
        for bad in ("0", "-5", "abc", "604801", "1.5", ""):
            r = self.run_board("claim", "t1", "--lease", bad)
            self.assertEqual(r.returncode, 1, bad)
        self.assertEqual(self.calls(), [])

    def test_a_write_needs_an_author(self):
        for cmd in (["claim", "t1"], ["heartbeat", "t1"], ["fail", "t1"], ["release", "t1"],
                    ["reap"], ["approve", "t1"], ["cancel", "t1"], ["block", "t1", "t2"],
                    ["unblock", "t1", "t2"], ["next"]):
            r = self.run_board(*cmd, author=None)
            self.assertEqual(r.returncode, 1, cmd)
            self.assertIn("BOARD_AUTHOR", r.stderr)
        self.assertEqual(self.calls(), [])

    def test_task_ids_and_numbers_are_checked_before_any_query(self):
        cases = [["heartbeat", "bad id"], ["done", "t1", "x", "--fence", "-1"],
                 ["fail", "t1", "--retry-in", "x"], ["add", "T", "--max-attempts", "0"],
                 ["add", "T", "--max-attempts", "101"], ["add", "T", "--blocked-by", "a,,b"],
                 ["add", "T", "--blocked-by", "a,a"], ["next", "--max-active", "0"],
                 ["next", "--owner", "bad name"], ["block", "t1", "a b"], ["unblock", "t1", "a,b"]]
        for args in cases:
            r = self.run_board(*args, env={"BOARD_STRICT_FENCE": "0"})
            self.assertEqual(r.returncode, 1, args)
        self.assertEqual(self.calls(), [])

    def test_block_has_no_by_option_and_says_so(self):
        r = self.run_board("block", "t1", "--by", "t2")
        self.assertEqual(r.returncode, 1)
        self.assertIn("no --by", r.stderr)
        self.assertEqual(self.calls(), [])

    def test_more_than_32_blockers_are_refused(self):
        ids = ",".join("t%d" % i for i in range(33))
        r = self.run_board("add", "T", "--blocked-by", ids)
        self.assertEqual(r.returncode, 1)
        self.assertIn("32", r.stderr)

    def test_list_knows_the_new_states(self):
        self.rules = [{"match": "SELECT id, state", "out": "id\tstate"}]
        for st in ("dead", "cancelled", "awaiting_approval"):
            self.assertEqual(self.run_board("list", st).returncode, 0, st)
        self.assertEqual(self.run_board("list", "bogus").returncode, 1)

    def test_text_that_looks_like_an_option_is_still_text(self):
        self.rules = [self.info(), {"match": NOTE_TX, "out": "t\t5"}]
        r = self.run_board("note", "t1", "--priority is high")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.vars_of(self.calls_matching(NOTE_TX)[0])["text"], "--priority is high")


class Claim(Cli):
    def claim_rules(self, tx_out, blocked="", pending=""):
        return [{"match": LEASE_ROW, "out": "t1"}, {"match": BLOCKED_BY, "out": blocked},
                {"match": DONE_IDS, "out": pending}, {"match": CLAIM_TX, "out": tx_out}]

    def test_a_won_claim_exits_0_and_saves_the_fence(self):
        r = self.run_board("claim", "t1", rules=self.claim_rules("t\talice\tclaimed\t17\tt"))
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "claimed t1"))
        with open(self.fence_file("t1")) as f:
            self.assertEqual(f.read().strip(), "17")
        tx = self.calls_matching(CLAIM_TX)[0]
        v = self.vars_of(tx)
        self.assertEqual((v["lease"], v["author"], v["task"]), ("600", "alice", "t1"))
        self.assertIn(":dbnow + :lease", tx["sql"])
        self.assertIn("now()", tx["sql"])

    def test_the_lease_option_reaches_the_transaction(self):
        self.run_board("claim", "t1", "--lease", "45", rules=self.claim_rules("t\talice\tclaimed\t3\tt"))
        self.assertEqual(self.vars_of(self.calls_matching(CLAIM_TX)[0])["lease"], "45")

    def test_the_state_directory_is_private(self):
        self.run_board("claim", "t1", rules=self.claim_rules("t\talice\tclaimed\t3\tt"))
        self.assertEqual(os.stat(self.state).st_mode & 0o777, 0o700)
        self.assertEqual(os.stat(self.fence_file("t1")).st_mode & 0o777, 0o600)

    def test_a_lost_claim_exits_2_and_names_the_holder(self):
        r = self.run_board("claim", "t1", rules=self.claim_rules("f\tbob\tclaimed\t8\tt"))
        self.assertEqual(r.returncode, 2)
        self.assertEqual(r.stdout.strip(), "not claimed: t1 is claimed, owner bob")
        self.assertFalse(os.path.exists(self.fence_file("t1")))

    def test_claiming_your_own_task_again_refreshes_the_fence(self):
        self.put_fence("t1", 4)
        r = self.run_board("claim", "t1", rules=self.claim_rules("f\talice\tclaimed\t12\tt"))
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "already yours: t1"))
        with open(self.fence_file("t1")) as f:
            self.assertEqual(f.read().strip(), "12")

    def test_a_task_in_backoff_exits_2(self):
        r = self.run_board("claim", "t1", rules=self.claim_rules("f\t\topen\t0\tf"))
        self.assertEqual(r.returncode, 2)
        self.assertIn("retry delay", r.stdout)

    def test_a_blocked_task_is_not_claimed_and_no_write_is_sent(self):
        r = self.run_board("claim", "t1", rules=self.claim_rules("t\talice\tclaimed\t1\tt",
                                                                 blocked="ta,tb", pending="ta"))
        self.assertEqual(r.returncode, 2)
        self.assertIn("blocked by tb", r.stdout)
        self.assertEqual(self.calls_matching(CLAIM_TX), [])

    def test_all_blockers_done_lets_the_claim_through(self):
        r = self.run_board("claim", "t1", rules=self.claim_rules("t\talice\tclaimed\t1\tt",
                                                                 blocked="ta,tb", pending="ta\ntb"))
        self.assertEqual(r.returncode, 0)

    def test_a_missing_task_is_an_error(self):
        r = self.run_board("claim", "t1", rules=[{"match": LEASE_ROW, "out": ""},
                                                 {"match": r"SELECT id FROM tasks WHERE id =", "out": ""}])
        self.assertEqual(r.returncode, 1)
        self.assertIn("no such task", r.stderr)

    def test_a_write_conflict_is_retried(self):
        rules = self.claim_rules("t\talice\tclaimed\t17\tt")
        rules[3] = {"match": CLAIM_TX, "rc": 1, "err": "ERROR:  40001: could not serialize"}
        r = self.run_board("claim", "t1", rules=rules, env={"BOARD_TX_MAX": "4"})
        self.assertEqual(r.returncode, 1)  # the fake keeps failing, so the helper gives up
        self.assertEqual(len(self.calls_matching(CLAIM_TX)), 4)
        self.assertIn("gave up after 4", r.stderr)


class Fencing(Cli):
    def test_heartbeat_uses_the_saved_fence(self):
        self.put_fence("t1", 17)
        rules = [{"match": TASK_INFO, "out": "claimed\talice\t17"}, {"match": LEASE_ROW, "out": "t1"},
                 {"match": HEARTBEAT_TX, "out": "t\t1700"}]
        r = self.run_board("heartbeat", "t1", "--lease", "90", rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        v = self.vars_of(self.calls_matching(HEARTBEAT_TX)[0])
        self.assertEqual((v["fence"], v["lease"]), ("17", "90"))
        self.assertIn("1700", r.stdout)

    def test_heartbeat_with_a_lost_lease_exits_3(self):
        self.put_fence("t1", 17)
        rules = [self.info("claimed", "bob", 21), {"match": LEASE_ROW, "out": "t1"},
                 {"match": HEARTBEAT_TX, "out": "f\t1700"}]
        r = self.run_board("heartbeat", "t1", rules=rules)
        self.assertEqual(r.returncode, 3)
        self.assertIn("lease lost", r.stderr)
        self.assertIn("owner bob", r.stderr)
        self.assertFalse(os.path.exists(self.fence_file("t1")))  # a lost fence is not kept

    def test_an_explicit_fence_beats_the_file(self):
        self.put_fence("t1", 17)
        rules = [self.info(), {"match": DONE_TX, "out": "t\t30"}, {"match": LEASE_ROW, "out": "t1"}]
        self.run_board("done", "t1", "--fence", "77", rules=rules)
        self.assertEqual(self.vars_of(self.calls_matching(DONE_TX)[0])["fence"], "77")

    def test_done_by_the_holder_clears_the_fence_file(self):
        self.put_fence("t1", 17)
        rules = [self.info(), {"match": LEASE_ROW, "out": "t1"}, {"match": DONE_TX, "out": "t\t30"}]
        r = self.run_board("done", "t1", "all good", rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("done t1 as event 30", r.stdout)
        self.assertFalse(os.path.exists(self.fence_file("t1")))
        self.assertEqual(self.vars_of(self.calls_matching(DONE_TX)[0])["fence"], "17")

    def test_done_without_a_fence_sends_minus_one(self):
        rules = [self.info("open", "", 0), {"match": LEASE_ROW, "out": "t1"}, {"match": DONE_TX, "out": "t\t30"}]
        self.run_board("done", "t1", rules=rules)
        self.assertEqual(self.vars_of(self.calls_matching(DONE_TX)[0])["fence"], "-1")

    def test_a_zombie_done_exits_3_and_records_a_fenced_event(self):
        self.put_fence("t1", 17)
        rules = [self.info("claimed", "bob", 21), {"match": LEASE_ROW, "out": "t1"},
                 {"match": DONE_TX, "out": "f\t30"}]
        r = self.run_board("done", "t1", rules=rules)
        self.assertEqual(r.returncode, 3)
        fenced = self.calls_matching(FENCED_TX)
        self.assertEqual(len(fenced), 1)
        self.assertIn("refused done with fence 17", self.vars_of(fenced[0])["text"])
        self.assertFalse(os.path.exists(self.fence_file("t1")))

    def test_done_twice_is_a_no_op_with_exit_0_and_no_write(self):
        rules = [self.info("done", "alice", 9)]
        r = self.run_board("done", "t1", rules=rules)
        self.assertEqual(r.returncode, 0)
        self.assertIn("already done", r.stdout)
        self.assertEqual(self.calls_matching(r"BEGIN"), [])

    def test_done_on_a_dead_task_is_an_error_not_a_fence_refusal(self):
        rules = [self.info("dead", "", 9), {"match": LEASE_ROW, "out": "t1"}, {"match": DONE_TX, "out": "f\t30"}]
        r = self.run_board("done", "t1", rules=rules)
        self.assertEqual(r.returncode, 1)
        self.assertIn("dead", r.stderr)
        self.assertEqual(self.calls_matching(FENCED_TX), [])

    def test_strict_mode_refuses_without_a_fence(self):
        for args in (["done", "t1"], ["heartbeat", "t1"], ["fail", "t1"], ["release", "t1"]):
            r = self.run_board(*args, env={"BOARD_STRICT_FENCE": "1"}, rules=[self.info()])
            self.assertEqual(r.returncode, 1, args)
            self.assertIn("STRICT_FENCE", r.stderr)
        self.assertEqual(self.calls(), [])

    def test_a_note_without_a_fence_is_still_allowed_in_strict_mode(self):
        rules = [self.info("claimed", "bob", 9), {"match": NOTE_TX, "out": "t\t5"}]
        r = self.run_board("note", "t1", "hello", env={"BOARD_STRICT_FENCE": "1"}, rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.vars_of(self.calls_matching(NOTE_TX)[0])["fence"], "-1")

    def test_a_note_with_a_saved_fence_is_fenced(self):
        self.put_fence("t1", 17)
        rules = [self.info("claimed", "bob", 21), {"match": NOTE_TX, "out": "f\t5"}]
        r = self.run_board("note", "t1", "hello", rules=rules)
        self.assertEqual(r.returncode, 3)
        self.assertEqual(len(self.calls_matching(FENCED_TX)), 1)

    def test_a_damaged_fence_file_is_an_error(self):
        self.put_fence("t1", "abc")
        r = self.run_board("done", "t1", rules=[self.info()])
        self.assertEqual(r.returncode, 1)
        self.assertIn("damaged", r.stderr)

    def test_the_fence_files_are_per_author(self):
        self.put_fence("t1", 17, author="alice")
        rules = [self.info(), {"match": LEASE_ROW, "out": "t1"}, {"match": DONE_TX, "out": "t\t30"}]
        self.run_board("done", "t1", author="bob", rules=rules)
        self.assertEqual(self.vars_of(self.calls_matching(DONE_TX)[0])["fence"], "-1")


class RetryAndDead(Cli):
    def fail_rules(self, attempts, tx_out="t\tf"):
        return [self.info(), {"match": LEASE_ROW, "out": "t1"}, {"match": ATTEMPTS, "out": str(attempts)},
                {"match": FAIL_TX, "out": tx_out}]

    def test_the_default_retry_delay_doubles_with_the_attempts(self):
        for attempts, want in ((0, "30"), (1, "60"), (2, "120"), (7, "3600"), (30, "3600")):
            self.setUp()
            self.put_fence("t1", 9)
            self.run_board("fail", "t1", "boom", rules=self.fail_rules(attempts))
            self.assertEqual(self.vars_of(self.calls_matching(FAIL_TX)[0])["delay"], want, attempts)

    def test_retry_in_overrides_the_default(self):
        self.put_fence("t1", 9)
        r = self.run_board("fail", "t1", "boom", "--retry-in", "5", rules=self.fail_rules(9))
        self.assertEqual(self.vars_of(self.calls_matching(FAIL_TX)[0])["delay"], "5")
        self.assertIn("open again in 5 seconds", r.stdout)
        self.assertEqual(self.calls_matching(ATTEMPTS), [])

    def test_a_fail_that_uses_the_last_attempt_says_dead(self):
        self.put_fence("t1", 9)
        r = self.run_board("fail", "t1", "boom", rules=self.fail_rules(2, "t\tt"))
        self.assertEqual(r.returncode, 0)
        self.assertIn("dead", r.stdout)
        self.assertFalse(os.path.exists(self.fence_file("t1")))

    def test_the_failure_text_is_stored_as_the_last_error(self):
        self.put_fence("t1", 9)
        self.run_board("fail", "t1", "it said 'no'", rules=self.fail_rules(0))
        tx = self.calls_matching(FAIL_TX)[0]
        self.assertEqual(self.vars_of(tx)["text"], "it said 'no'")
        self.assertIn("last_error = :'text'", tx["sql"])
        self.assertIn("attempts = attempts + 1", tx["sql"])
        self.assertIn("attempts + 1 >= max_attempts", tx["sql"])

    def test_a_zombie_fail_exits_3(self):
        self.put_fence("t1", 9)
        rules = self.fail_rules(0, "f\tf")
        rules[0] = self.info("claimed", "bob", 12)
        r = self.run_board("fail", "t1", rules=rules)
        self.assertEqual(r.returncode, 3)

    def test_release_does_not_count_an_attempt(self):
        self.put_fence("t1", 9)
        rules = [self.info(), {"match": LEASE_ROW, "out": "t1"}, {"match": RELEASE_TX, "out": "t\t4"}]
        r = self.run_board("release", "t1", rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertNotIn("attempts", self.calls_matching(RELEASE_TX)[0]["sql"])


class Reap(Cli):
    def test_reap_expires_each_candidate_with_a_compare_and_set(self):
        rules = [{"match": DB_NOW, "out": "5000"},
                 {"match": REAP_LIST, "out": "ta\t0\ntb\t2"},
                 {"match": r"SELECT state, coalesce\(owner[\s\S]*task=ta", "out": "claimed\tbob\t31"},
                 {"match": r"SELECT state, coalesce\(owner[\s\S]*task=tb", "out": "claimed\tcy\t32"},
                 {"match": REAP_TX, "out": "t\tf"}]
        r = self.run_board("reap", rules=rules)
        self.assertEqual((r.returncode, r.stdout.strip().splitlines()[-1]), (0, "reaped 2"))
        txs = self.calls_matching(REAP_TX)
        self.assertEqual([self.vars_of(t)["fence"] for t in txs], ["31", "32"])
        self.assertEqual([self.vars_of(t)["delay"] for t in txs], ["30", "120"])
        sql = txs[0]["sql"]
        self.assertIn("state = 'claimed' AND claim_seq = :fence", sql)
        self.assertIn("lease_until > 0 AND lease_until < :dbnow", sql)

    def test_reap_skips_a_candidate_that_is_no_longer_claimed(self):
        rules = [{"match": DB_NOW, "out": "5000"}, {"match": REAP_LIST, "out": "ta\t0"},
                 self.info("open", "", 31)]
        r = self.run_board("reap", rules=rules)
        self.assertEqual(r.returncode, 0)
        self.assertEqual(self.calls_matching(REAP_TX), [])
        self.assertIn("reaped 0", r.stdout)

    def test_a_lost_compare_is_not_counted(self):
        rules = [{"match": DB_NOW, "out": "5000"}, {"match": REAP_LIST, "out": "ta\t0"},
                 self.info("claimed", "bob", 31), {"match": REAP_TX, "out": "f\tf"}]
        r = self.run_board("reap", rules=rules)
        self.assertIn("reaped 0", r.stdout)

    def test_reap_with_nothing_expired(self):
        r = self.run_board("reap", rules=[{"match": DB_NOW, "out": "5000"}])
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "reaped 0"))

    def test_a_database_without_now_is_a_clear_error(self):
        r = self.run_board("reap", rules=[{"match": DB_NOW, "out": ""}])
        self.assertEqual(r.returncode, 1)
        self.assertIn("now()", r.stderr)


class Next(Cli):
    def next_rules(self, cands, leases, claim_out="t\talice\tclaimed\t5\tt", held="", extra=()):
        rules = [{"match": CAP_QUERY, "out": held}, {"match": DB_NOW, "out": "1000"},
                 {"match": CANDIDATES, "outs": [cands, ""]}, {"match": CAND_LEASES, "out": leases}]
        rules += list(extra)
        rules += [{"match": LEASE_ROW, "out": "x"}, {"match": BLOCKED_BY, "out": ""},
                  {"match": CLAIM_TX, "out": claim_out}]
        return rules

    def test_next_claims_the_first_runnable_candidate_and_prints_only_its_id(self):
        rules = self.next_rules("ta\ntb\ntc", "ta\t2000\t\ntb\t0\t\ntc\t0\t")
        r = self.run_board("next", rules=rules)
        self.assertEqual((r.returncode, r.stdout), (0, "tb\n"))
        self.assertEqual(self.vars_of(self.calls_matching(CLAIM_TX)[0])["task"], "tb")

    def test_next_skips_a_candidate_with_an_unfinished_blocker(self):
        rules = self.next_rules("ta\ntb", "ta\t0\ttx\ntb\t0\t", extra=[{"match": DONE_IDS, "out": ""}])
        r = self.run_board("next", rules=rules)
        self.assertEqual(r.stdout, "tb\n")

    def test_next_with_nothing_runnable_exits_4(self):
        rules = self.next_rules("ta", "ta\t2000\t")
        r = self.run_board("next", rules=rules)
        self.assertEqual((r.returncode, r.stdout), (4, ""))
        self.assertIn("nothing runnable", r.stderr)
        self.assertEqual(self.calls_matching(CLAIM_TX), [])

    def test_next_with_no_open_task_exits_4(self):
        r = self.run_board("next", rules=self.next_rules("", ""))
        self.assertEqual(r.returncode, 4)

    def test_next_tries_the_following_candidate_when_a_claim_is_lost(self):
        rules = self.next_rules("ta\ntb", "ta\t0\t\ntb\t0\t")
        rules[-1] = {"match": CLAIM_TX, "outs": ["f\tbob\tclaimed\t4\tt", "t\talice\tclaimed\t6\tt"]}
        r = self.run_board("next", rules=rules)
        self.assertEqual((r.returncode, r.stdout), (0, "tb\n"))

    def test_next_stops_at_the_cap_before_looking_for_work(self):
        rules = self.next_rules("ta", "ta\t0\t", held="tz")
        r = self.run_board("next", rules=rules)
        self.assertEqual(r.returncode, 4)
        self.assertIn("at capacity", r.stderr)
        self.assertEqual(self.calls_matching(CANDIDATES), [])

    def test_a_larger_cap_allows_another_claim(self):
        rules = self.next_rules("ta", "ta\t0\t", held="tz")
        r = self.run_board("next", "--max-active", "2", rules=rules)
        self.assertEqual(r.returncode, 0)
        self.assertEqual(self.vars_of(self.calls_matching(CAP_QUERY)[0])["cap"], "2")

    def test_owner_option_sets_the_claiming_name(self):
        rules = self.next_rules("ta", "ta\t0\t")
        r = self.run_board("next", "--owner", "worker-7", author=None, rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.vars_of(self.calls_matching(CLAIM_TX)[0])["author"], "worker-7")
        self.assertTrue(os.path.exists(self.fence_file("ta", "worker-7")))

    def test_next_reaps_before_it_claims(self):
        rules = self.next_rules("ta", "ta\t0\t")
        self.run_board("next", rules=rules)
        sqls = [c["sql"] for c in self.calls()]
        first_reap = next(i for i, s in enumerate(sqls) if "lease_until > 0" in s)
        first_cand = next(i for i, s in enumerate(sqls) if "created_seq LIMIT 50" in s)
        self.assertLess(first_reap, first_cand)

    def test_a_backoff_in_the_future_is_compared_with_the_database_clock(self):
        rules = self.next_rules("ta", "ta\t1001\t")  # database clock says 1000
        self.assertEqual(self.run_board("next", rules=rules).returncode, 4)
        self.setUp()
        rules = self.next_rules("ta", "ta\t1000\t")
        self.assertEqual(self.run_board("next", rules=rules).returncode, 0)


class Dependencies(Cli):
    def block_rules(self, graph, state="open", tx_out="t"):
        rules = [{"match": TASK_INFO, "out": "%s\t\t0" % state}, {"match": EXISTS_IN, "out": "tb\ntc"},
                 {"match": LEASE_ROW, "out": "t1"}]
        for tid, deps in graph.items():
            rules.append({"match": r"SELECT blocked_by FROM task_leases[\s\S]*task=%s\b" % tid, "out": deps})
        rules.append({"match": BLOCK_TX, "out": tx_out})
        return rules

    def test_block_adds_the_blockers(self):
        r = self.run_board("block", "ta", "tb,tc", rules=self.block_rules({"ta": "", "tb": "", "tc": ""}))
        self.assertEqual(r.returncode, 0, r.stderr)
        tx = self.calls_matching(BLOCK_TX)[0]
        v = self.vars_of(tx)
        self.assertEqual((v["new"], v["old"], v["kind"]), ("tb,tc", "", "block"))

    def test_block_merges_with_existing_blockers(self):
        rules = self.block_rules({"ta": "tz", "tb": "", "tc": ""})
        rules = [x for x in rules if "task=ta" not in x["match"]] + \
            [{"match": r"SELECT blocked_by FROM task_leases[\s\S]*task=ta\b", "out": "tz,tb"}]
        r = self.run_board("block", "ta", "tb,tc", rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.vars_of(self.calls_matching(BLOCK_TX)[0])["new"], "tz,tb,tc")

    def test_a_direct_cycle_is_refused(self):
        r = self.run_board("block", "ta", "tb,tc", rules=self.block_rules({"tb": "ta", "tc": ""}))
        self.assertEqual(r.returncode, 1)
        self.assertIn("cycle", r.stderr)
        self.assertEqual(self.calls_matching(BLOCK_TX), [])

    def test_an_indirect_cycle_is_refused(self):
        r = self.run_board("block", "ta", "tb,tc", rules=self.block_rules({"tb": "tx", "tx": "ty", "ty": "ta", "tc": ""}))
        self.assertEqual(r.returncode, 1)
        self.assertIn("cycle", r.stderr)

    def test_a_task_cannot_wait_for_itself(self):
        rules = self.block_rules({})
        rules[1] = {"match": EXISTS_IN, "out": "ta"}
        r = self.run_board("block", "ta", "ta", rules=rules)
        self.assertEqual(r.returncode, 1)
        self.assertIn("itself", r.stderr)

    def test_the_walk_is_bounded(self):
        graph = {"tb": ",".join("w%d" % i for i in range(30)), "tc": ""}
        for i in range(30):
            graph["w%d" % i] = ",".join("w%d_%d" % (i, j) for j in range(30))
        r = self.run_board("block", "ta", "tb,tc", rules=self.block_rules(graph))
        self.assertEqual(r.returncode, 1)
        self.assertIn("256", r.stderr)

    def test_a_blocker_that_does_not_exist_is_refused(self):
        rules = self.block_rules({})
        rules[1] = {"match": EXISTS_IN, "out": "tb"}
        r = self.run_board("block", "ta", "tb,tc", rules=rules)
        self.assertEqual(r.returncode, 1)
        self.assertIn("does not exist", r.stderr)

    def test_a_finished_task_cannot_be_blocked(self):
        r = self.run_board("block", "ta", "tb,tc", rules=self.block_rules({}, state="done"))
        self.assertEqual(r.returncode, 1)

    def test_a_lost_compare_is_retried_with_a_fresh_read(self):
        rules = self.block_rules({"ta": "", "tb": "", "tc": ""})
        rules[-1] = {"match": BLOCK_TX, "outs": ["f", "t"]}
        r = self.run_board("block", "ta", "tb,tc", rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.calls_matching(BLOCK_TX)), 2)

    def test_unblock_removes_one_blocker(self):
        rules = [{"match": TASK_INFO, "out": "open\t\t0"}, {"match": LEASE_ROW, "out": "t1"},
                 {"match": BLOCKED_BY, "out": "tb,tc,td"}, {"match": BLOCK_TX, "out": "t"}]
        r = self.run_board("unblock", "ta", "tc", rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        v = self.vars_of(self.calls_matching(BLOCK_TX)[0])
        self.assertEqual((v["new"], v["old"], v["kind"]), ("tb,td", "tb,tc,td", "unblock"))

    def test_unblock_of_an_absent_blocker_is_a_no_op(self):
        rules = [{"match": TASK_INFO, "out": "open\t\t0"}, {"match": LEASE_ROW, "out": "t1"},
                 {"match": BLOCKED_BY, "out": "tb"}]
        r = self.run_board("unblock", "ta", "tz", rules=rules)
        self.assertEqual(r.returncode, 0)
        self.assertEqual(self.calls_matching(BLOCK_TX), [])


class AddApproveCancel(Cli):
    def test_add_puts_the_lease_row_in_the_same_transaction(self):
        r = self.run_board("add", "Title", "--max-attempts", "5", rules=[{"match": r"INSERT INTO tasks", "out": "7"}])
        self.assertEqual(r.returncode, 0, r.stderr)
        tx = self.calls_matching(r"INSERT INTO tasks")[0]
        self.assertIn("INSERT INTO task_leases", tx["sql"])
        self.assertEqual(self.vars_of(tx)["maxa"], "5")
        self.assertEqual(self.vars_of(tx)["state"], "open")

    def test_needs_approval_creates_an_awaiting_task(self):
        self.run_board("add", "Title", "--needs-approval", rules=[{"match": r"INSERT INTO tasks", "out": "7"}])
        self.assertEqual(self.vars_of(self.calls_matching(r"INSERT INTO tasks")[0])["state"], "awaiting_approval")

    def test_blockers_must_exist(self):
        rules = [{"match": EXISTS_IN, "out": "ta"}, {"match": r"INSERT INTO tasks", "out": "7"}]
        r = self.run_board("add", "Title", "--blocked-by", "ta,tb", rules=rules)
        self.assertEqual(r.returncode, 1)
        self.assertEqual(self.calls_matching(r"INSERT INTO tasks"), [])

    def test_user_text_reaches_psql_only_as_a_variable(self):
        sentinel = "x'; DROP TABLE tasks; -- é"
        self.run_board("add", sentinel, "--body", "it's body", rules=[{"match": r"INSERT INTO tasks", "out": "7"}])
        tx = self.calls_matching(r"INSERT INTO tasks")[0]
        self.assertNotIn("DROP TABLE", tx["sql"])
        self.assertEqual(self.vars_of(tx)["title"], sentinel)

    def test_approve_moves_a_waiting_task(self):
        rules = [{"match": TASK_INFO, "out": "awaiting_approval\t\t0"}, {"match": APPROVE_TX, "out": "t\t41"}]
        r = self.run_board("approve", "t1", "ok to go", author="boss", rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.vars_of(self.calls_matching(APPROVE_TX)[0])["author"], "boss")

    def test_approving_twice_is_a_no_op(self):
        r = self.run_board("approve", "t1", rules=[{"match": TASK_INFO, "out": "open\t\t0"}])
        self.assertEqual(r.returncode, 0)
        self.assertIn("already past approval", r.stdout)
        self.assertEqual(self.calls_matching(APPROVE_TX), [])

    def test_a_dead_task_cannot_be_approved(self):
        r = self.run_board("approve", "t1", rules=[{"match": TASK_INFO, "out": "dead\t\t0"}])
        self.assertEqual(r.returncode, 1)

    def test_cancel(self):
        rules = [{"match": TASK_INFO, "out": "claimed\tbob\t9"}, {"match": LEASE_ROW, "out": "t1"},
                 {"match": CANCEL_TX, "out": "t\t50"}]
        self.assertEqual(self.run_board("cancel", "t1", "not needed", rules=rules).returncode, 0)
        self.assertIn("lease_until = 0", self.calls_matching(CANCEL_TX)[0]["sql"])

    def test_a_done_task_cannot_be_cancelled(self):
        self.assertEqual(self.run_board("cancel", "t1", rules=[{"match": TASK_INFO, "out": "done\talice\t9"}]).returncode, 1)


class Migrate(Cli):
    def test_migrate_adds_only_the_missing_lease_rows(self):
        rules = [{"match": r"SELECT n FROM counters", "out": "0"},
                 {"match": r"SELECT id FROM tasks ORDER BY id", "out": "ta\ntb\ntc"},
                 {"match": r"SELECT task_id FROM task_leases ORDER BY", "outs": ["tb", "ta\ntb\ntc"]}]
        r = self.run_board("migrate", rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("2 lease rows added", r.stdout)
        ins = self.calls_matching(r"INSERT INTO task_leases")[0]
        self.assertEqual(sorted(v for k, v in self.vars_of(ins).items() if k.startswith("m")), ["ta", "tc"])

    def test_migrate_on_a_migrated_board_adds_nothing(self):
        rules = [{"match": r"SELECT n FROM counters", "out": "0"},
                 {"match": r"SELECT id FROM tasks ORDER BY id", "out": "ta"},
                 {"match": r"SELECT task_id FROM task_leases ORDER BY", "out": "ta"}]
        r = self.run_board("migrate", rules=rules)
        self.assertIn("0 lease rows added", r.stdout)
        self.assertEqual(self.calls_matching(r"INSERT INTO task_leases"), [])

    def test_the_schema_defines_the_lease_table_without_alter(self):
        with open(os.path.join(os.path.dirname(HERE), "schema.sql")) as f:
            schema = f.read()
        self.assertIn("CREATE TABLE IF NOT EXISTS task_leases", schema)
        code = "\n".join(l for l in schema.splitlines() if not l.lstrip().startswith("--"))
        self.assertNotIn("ALTER", code.upper())


CONNECT_DROP = ('psql: error: connection to server at "127.0.0.1", port 5432 failed: '
                "server closed the connection unexpectedly")


class ConnectRetry(Cli):
    """The alpha.1 server drops some new connections. A failed connect sent no statement, so it is
    retried; nothing else is."""

    def test_a_dropped_connect_is_retried_and_the_command_succeeds(self):
        rules = [{"match": r"SELECT id FROM events ORDER BY id DESC",
                  "seq": [{"rc": 2, "err": CONNECT_DROP}, {"rc": 2, "err": CONNECT_DROP}, {"out": "42"}]}]
        r = self.run_board("head", rules=rules)
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "42"))
        self.assertEqual(len(self.calls()), 3)

    def test_the_connect_retries_are_bounded(self):
        rules = [{"match": "SELECT", "rc": 2, "err": CONNECT_DROP}]
        r = self.run_board("head", rules=rules, env={"BOARD_CONNECT_TRIES": "3"})
        self.assertEqual(r.returncode, 1)
        self.assertEqual(len(self.calls()), 3)
        self.assertIn("connection to server", r.stderr)

    def test_a_refused_login_is_not_retried(self):
        err = 'psql: error: connection to server at "127.0.0.1", port 5432 failed: FATAL:  password authentication failed'
        r = self.run_board("head", rules=[{"match": "SELECT", "rc": 2, "err": err}])
        self.assertEqual(r.returncode, 1)
        self.assertEqual(len(self.calls()), 1)

    def test_a_connection_lost_after_the_statement_was_sent_is_not_retried(self):
        err = "psql: error: connection to the server was lost"
        r = self.run_board("head", rules=[{"match": "SELECT", "rc": 2, "err": err}])
        self.assertEqual(r.returncode, 1)
        self.assertEqual(len(self.calls()), 1)

    def test_an_sql_error_is_not_retried(self):
        r = self.run_board("head", rules=[{"match": "SELECT", "rc": 3, "err": "ERROR: boom"}])
        self.assertEqual(r.returncode, 1)
        self.assertEqual(len(self.calls()), 1)

    def test_a_write_is_not_resent_after_it_succeeded_on_a_retried_connect(self):
        self.put_fence("t1", 17)
        rules = [self.info(), {"match": LEASE_ROW, "out": "t1"},
                 {"match": DONE_TX, "seq": [{"rc": 2, "err": CONNECT_DROP}, {"out": "t\t30"}]}]
        r = self.run_board("done", "t1", rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.calls_matching(DONE_TX)), 2)  # first never connected, second ran


def drop(msg):
    return {"rc": 2, "err": 'psql: error: connection to server at "127.0.0.1", port 5432 failed: ' + msg}


class ConnectErrorClasses(Cli):
    """Which connect-phase messages are retried, and which never are."""

    def attempts(self, err, tries="4"):
        rules = [{"match": "SELECT", "rc": 2, "err": err}]
        r = self.run_board("head", rules=rules, env={"BOARD_CONNECT_TRIES": tries})
        return len(self.calls()), r

    def test_transient_messages_are_retried_up_to_the_limit(self):
        for msg in ("server closed the connection unexpectedly", "Connection reset by peer",
                    "Connection refused", "timeout expired"):
            self.setUp()
            n, r = self.attempts(drop(msg)["err"])
            self.assertEqual((n, r.returncode), (4, 1), msg)

    def test_these_are_never_retried(self):
        base = 'psql: error: connection to server at "127.0.0.1", port 5432 failed: '
        for msg in ("fe_sendauth: no password supplied",
                    'FATAL:  password authentication failed for user "board"',
                    "SSL error: certificate verify failed",
                    'server certificate for "x" does not match host name',
                    "TLS handshake failed: Connection reset by peer",
                    'FATAL:  database "lattice" does not exist',
                    "some message nobody has seen before",
                    'could not translate host name "nope" to address: Name or service not known'):
            self.setUp()
            n, r = self.attempts(base + msg)
            self.assertEqual((n, r.returncode), (1, 1), msg)

    def test_a_message_without_the_connect_prefix_is_never_retried(self):
        n, _ = self.attempts("psql: error: server closed the connection unexpectedly")
        self.assertEqual(n, 1)
        self.setUp()
        n, _ = self.attempts("psql: error: connection to the server was lost")
        self.assertEqual(n, 1)

    def test_an_exit_status_other_than_2_is_not_retried(self):
        rules = [{"match": "SELECT", "rc": 1, "err": drop("server closed the connection unexpectedly")["err"]}]
        self.run_board("head", rules=rules)
        self.assertEqual(len(self.calls()), 1)

    def test_the_default_limit_is_12_attempts(self):
        rules = [{"match": "SELECT", "rc": 2, "err": drop("Connection refused")["err"]}]
        r = self.run_board("head", rules=rules)
        self.assertEqual(r.returncode, 1)
        self.assertEqual(len(self.calls()), 12)

    def test_every_psql_call_has_w_so_a_missing_password_cannot_prompt(self):
        rules = [{"match": TASK_INFO, "out": "claimed\talice\t9"}, {"match": LEASE_ROW, "out": "t1"},
                 {"match": HEARTBEAT_TX, "out": "t\t1"}]
        self.run_board("heartbeat", "t1", "--fence", "9", rules=rules)
        calls = self.calls()
        self.assertGreaterEqual(len(calls), 3)
        for c in calls:
            self.assertIn("-w", c["argv"])


class Hardening(Cli):
    def test_double_dash_ends_the_options(self):
        rules = [self.info(), {"match": NOTE_TX, "out": "t\t5"}]
        r = self.run_board("note", "t1", "--", "--fence", rules=rules)
        self.assertEqual(r.returncode, 0, r.stderr)
        v = self.vars_of(self.calls_matching(NOTE_TX)[0])
        self.assertEqual((v["text"], v["fence"]), ("--fence", "-1"))

    def test_options_still_work_before_the_double_dash(self):
        rules = [self.info(), {"match": NOTE_TX, "out": "t\t5"}]
        self.run_board("note", "--fence", "7", "t1", "--", "--lease", rules=rules)
        v = self.vars_of(self.calls_matching(NOTE_TX)[0])
        self.assertEqual((v["text"], v["fence"]), ("--lease", "7"))

    def test_task_ids_are_limited_to_64_characters(self):
        self.assertEqual(self.run_board("heartbeat", "t" * 65).returncode, 1)
        self.assertEqual(self.calls(), [])
        r = self.run_board("heartbeat", "t" * 64, rules=[{"match": TASK_INFO, "out": ""}])
        self.assertIn("no such task", r.stderr)

    def test_numbers_are_bounded(self):
        for args in (["since", "-1"], ["since", "1" * 19], ["watch", "t1", "--after", "-3"],
                     ["watch", "t1", "--after", "9" * 19], ["add", "T", "--priority", "1" * 10],
                     ["add", "T", "--priority", "-"], ["add", "T", "--priority", "x"]):
            self.assertEqual(self.run_board(*args).returncode, 1, args)
        self.assertEqual(self.calls(), [])

    def test_numbers_at_the_bounds_are_accepted(self):
        rules = [{"match": "SELECT id, task_id", "out": ""}, {"match": r"INSERT INTO tasks", "out": "7"}]
        self.assertEqual(self.run_board("since", "9" * 18, rules=rules).returncode, 0)
        self.assertEqual(self.run_board("add", "T", "--priority", "-999999999", rules=rules).returncode, 0)

    def test_the_hold_seam_needs_test_mode(self):
        rules = [self.info(), {"match": NOTE_TX, "out": "t\t5"}]
        hold = os.path.join(self.tmp, "hold")
        self.run_board("note", "t1", "x", rules=rules, env={"BOARD_TEST_HOLD_FILE": hold})
        self.assertNotIn(".started", self.calls_matching(NOTE_TX)[0]["sql"])
        self.setUp()
        open(hold, "w").close()  # already present, so the seam does not wait
        self.run_board("note", "t1", "x", rules=rules, env={"BOARD_TEST_HOLD_FILE": hold, "BOARD_TEST_MODE": "1"})
        self.assertIn(".started", self.calls_matching(NOTE_TX)[0]["sql"])

    # -- the fence directory
    def claim_rules(self):
        return [{"match": LEASE_ROW, "out": "t1"}, {"match": BLOCKED_BY, "out": ""},
                {"match": CLAIM_TX, "out": "t\talice\tclaimed\t17\tt"}]

    def test_a_state_directory_with_loose_permissions_is_refused(self):
        os.makedirs(self.state, mode=0o755)
        os.chmod(self.state, 0o755)
        r = self.run_board("claim", "t1", rules=self.claim_rules())
        self.assertEqual(r.returncode, 1)
        self.assertIn("0700", r.stderr)
        self.assertEqual(self.calls_matching(CLAIM_TX), [])  # refused before the claim is sent

    def test_a_symlinked_state_directory_is_refused(self):
        real = os.path.join(self.tmp, "real")
        os.makedirs(real, mode=0o700)
        os.symlink(real, self.state)
        r = self.run_board("claim", "t1", rules=self.claim_rules())
        self.assertEqual(r.returncode, 1)
        self.assertIn("symbolic link", r.stderr)
        self.assertEqual(self.calls_matching(CLAIM_TX), [])

    def test_a_symlinked_fence_file_is_refused(self):
        os.makedirs(self.state, mode=0o700)
        target = os.path.join(self.tmp, "elsewhere")
        with open(target, "w") as f:
            f.write("5\n")
        os.symlink(target, self.fence_file("t1"))
        r = self.run_board("done", "t1", rules=[self.info()])
        self.assertEqual(r.returncode, 1)
        self.assertIn("symbolic link", r.stderr)

    def test_a_loose_state_directory_is_refused_when_reading_a_fence_too(self):
        os.makedirs(self.state, mode=0o755)
        os.chmod(self.state, 0o755)
        r = self.run_board("heartbeat", "t1", rules=[self.info()])
        self.assertEqual(r.returncode, 1)
        self.assertIn("0700", r.stderr)

    def test_the_fence_file_is_written_through_a_temporary_name_and_leaves_none_behind(self):
        self.run_board("claim", "t1", rules=self.claim_rules())
        self.assertEqual(os.listdir(self.state), ["alice.t1"])

    def test_a_good_directory_is_accepted(self):
        os.makedirs(self.state, mode=0o700)
        r = self.run_board("claim", "t1", rules=self.claim_rules())
        self.assertEqual(r.returncode, 0, r.stderr)


class ZombieExitCodes(Cli):
    def test_a_zombie_done_on_a_task_reaped_to_dead_exits_3_with_a_fenced_event(self):
        self.put_fence("t1", 17)
        rules = [self.info("dead", "", 17), {"match": LEASE_ROW, "out": "t1"}, {"match": DONE_TX, "out": "f\t30"}]
        r = self.run_board("done", "t1", rules=rules)
        self.assertEqual(r.returncode, 3)
        self.assertEqual(len(self.calls_matching(FENCED_TX)), 1)

    def test_a_zombie_done_on_a_cancelled_task_exits_3(self):
        self.put_fence("t1", 17)
        rules = [self.info("cancelled", "", 17), {"match": LEASE_ROW, "out": "t1"}, {"match": DONE_TX, "out": "f\t30"}]
        self.assertEqual(self.run_board("done", "t1", rules=rules).returncode, 3)

    def test_a_zombie_done_on_a_reclaimed_task_exits_3(self):
        self.put_fence("t1", 17)
        rules = [self.info("claimed", "bob", 21), {"match": LEASE_ROW, "out": "t1"}, {"match": DONE_TX, "out": "f\t30"}]
        self.assertEqual(self.run_board("done", "t1", rules=rules).returncode, 3)

    def test_without_a_fence_done_on_a_dead_task_is_still_a_plain_error(self):
        rules = [self.info("dead", "", 17), {"match": LEASE_ROW, "out": "t1"}, {"match": DONE_TX, "out": "f\t30"}]
        r = self.run_board("done", "t1", rules=rules)
        self.assertEqual(r.returncode, 1)
        self.assertEqual(self.calls_matching(FENCED_TX), [])

    def test_done_by_someone_else_already_is_exit_0_even_with_a_stale_fence(self):
        self.put_fence("t1", 17)
        r = self.run_board("done", "t1", rules=[self.info("done", "bob", 21)])
        self.assertEqual(r.returncode, 0)
        self.assertIn("already done", r.stdout)


if __name__ == "__main__":
    unittest.main()
