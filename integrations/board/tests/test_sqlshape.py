"""The SQL the helper sends is protected by its compare-and-set, and the check can fail.

check_all (sqlshape.py) must find nothing in the real helper, and must find something in every
copy of the helper from which one protection was removed (a mutation). Each mutation below removes
one protection that the CONVENTIONS promise: the fence, the owner, the lease past, the state
guard, the attempt limit, the `\\if` that keeps a write inside its guard.
"""

import os
import re
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import sqlshape  # noqa: E402

BOARD_DIR = os.path.dirname(HERE)
BOARD = os.path.join(BOARD_DIR, "board")


def read_board():
    with open(BOARD) as f:
        return f.read()


def function_body(src, name):
    m = re.search(r"^%s\(\) \{\n(.*?)^\}\n" % re.escape(name), src, re.S | re.M)
    assert m, name
    return m.group(0)


def mutate(src, old, new, count=1):
    assert old in src, "the mutation target is gone: %r" % old[:80]
    return src.replace(old, new, count)


def drop_if_blocks(src, func):
    body = function_body(src, func)
    new = body.replace("\\if :ok\n", "").replace("\\endif\n", "")
    assert new != body
    return src.replace(body, new)


# (name, mutation). Each removes one protection. Written from the CONVENTIONS, not from the tests.
MUTATIONS = [
    ("the fence compare of HELD_SQL (heartbeat, fail, release)",
     lambda s: mutate(s, " AND (claim_seq = :fence OR :fence < 0)\"", "\"")),
    ("the owner compare of HELD_SQL",
     lambda s: mutate(s, "AND coalesce(owner, '') = :'author' AND (claim_seq = :fence OR :fence < 0)\"",
                      "AND (claim_seq = :fence OR :fence < 0)\"")),
    ("the state compare of HELD_SQL",
     lambda s: mutate(s, "HELD_SQL=\"state = 'claimed' AND ", "HELD_SQL=\"")),
    ("the fence compare of note",
     lambda s: mutate(s, "AND claim_seq = :fence)) AS ok\n  FROM tasks", ")) AS ok\n  FROM tasks")),
    ("the owner compare of note",
     lambda s: mutate(s, "(state = 'claimed' AND coalesce(owner, '') = :'author' AND claim_seq = :fence)) AS ok",
                      "(state = 'claimed' AND claim_seq = :fence)) AS ok")),
    ("the fence compare of done",
     lambda s: mutate(s, "AND (claim_seq = :fence OR :fence < 0))\n        OR (state = 'open'",
                      ")\n        OR (state = 'open'")),
    ("the fence guard on done of an open task",
     lambda s: mutate(s, "OR (state = 'open' AND :fence < 0)", "OR (state = 'open')")),
    ("the done_seq marker of done",
     lambda s: mutate(s, "FROM tasks WHERE id = :'task' AND done_seq = :seq;", "FROM tasks WHERE id = :'task';")),
    ("the claim_seq compare of reap",
     lambda s: mutate(s, "SELECT (state = 'claimed' AND claim_seq = :fence) AS ok",
                      "SELECT (state = 'claimed') AS ok")),
    ("the lease-past compare of reap",
     lambda s: mutate(s, "SELECT (lease_until > 0 AND lease_until < :dbnow) AS ok", "SELECT (true) AS ok")),
    ("the state guard of cancel",
     lambda s: mutate(s, "SELECT (state = 'open' OR state = 'claimed' OR state = 'awaiting_approval') AS ok",
                      "SELECT (true) AS ok")),
    ("the state guard of approve",
     lambda s: mutate(s, "SELECT (state = 'awaiting_approval') AS ok", "SELECT (true) AS ok")),
    ("the compare and set of block",
     lambda s: mutate(s, "SELECT (blocked_by = :'old') AS ok", "SELECT (true) AS ok")),
    ("the open-state condition of claim",
     lambda s: mutate(s, "WHERE id = :'task' AND owner IS NULL AND state = 'open';",
                      "WHERE id = :'task';")),
    ("the retry-delay check of claim",
     lambda s: mutate(s, "SELECT (not_before <= :dbnow) AS ok", "SELECT (true) AS ok")),
    ("the attempt limit of fail",
     lambda s: mutate(s, "SELECT (attempts + 1 >= max_attempts) AS exhausted", "SELECT (false) AS exhausted", 1)),
    ("the guard block around release",
     lambda s: drop_if_blocks(s, "cmd_release")),
    ("the guard block around cancel",
     lambda s: drop_if_blocks(s, "cmd_cancel")),
    ("the guard block around approve",
     lambda s: drop_if_blocks(s, "cmd_approve")),
    ("the guard block around note",
     lambda s: drop_if_blocks(s, "cmd_note")),
    ("the guard block around done",
     lambda s: drop_if_blocks(s, "cmd_done")),
    ("the guard block around fail",
     lambda s: drop_if_blocks(s, "cmd_fail")),
    ("the guard block around heartbeat",
     lambda s: drop_if_blocks(s, "cmd_heartbeat")),
    ("the guard block around reap",
     lambda s: drop_if_blocks(s, "reap_one")),
    ("the fence compare of bench-record",
     lambda s: mutate(s, "AND claim_seq = :fence) AS ok\n  FROM tasks WHERE id = :'task' \\gset\n\\if :ok\nUPDATE bench_attempts",
                      ") AS ok\n  FROM tasks WHERE id = :'task' \\gset\n\\if :ok\nUPDATE bench_attempts")),
    ("the owner compare of bench-record",
     lambda s: mutate(s, "SELECT (state = 'claimed' AND coalesce(owner, '') = :'author' AND claim_seq = :fence) AS ok\n  FROM tasks WHERE id = :'task' \\gset\n\\if :ok\nUPDATE bench_attempts",
                      "SELECT (state = 'claimed' AND claim_seq = :fence) AS ok\n  FROM tasks WHERE id = :'task' \\gset\n\\if :ok\nUPDATE bench_attempts")),
    ("the state compare of bench-record",
     lambda s: mutate(s, "SELECT (state = 'claimed' AND coalesce(owner, '') = :'author' AND claim_seq = :fence) AS ok\n  FROM tasks WHERE id = :'task' \\gset\n\\if :ok\nUPDATE bench_attempts",
                      "SELECT (coalesce(owner, '') = :'author' AND claim_seq = :fence) AS ok\n  FROM tasks WHERE id = :'task' \\gset\n\\if :ok\nUPDATE bench_attempts")),
    ("the guard block around bench-record",
     lambda s: drop_if_blocks(s, "cmd_bench_record")),
    ("the once-only condition of bench-record",
     lambda s: mutate(s, "AND task_key = :'key' AND outcome = 'planned';", "AND task_key = :'key';")),
    ("the identity condition of bench-record",
     lambda s: mutate(s, "AND experiment_id = :'exp' AND arm = :'arm' AND task_key = :'key' AND outcome = 'planned';", "AND outcome = 'planned';")),
]


class RealHelper(unittest.TestCase):
    def test_every_write_path_is_protected(self):
        self.assertEqual(sqlshape.check_all(BOARD), [])

    def test_every_write_path_is_covered(self):
        self.assertEqual(sorted(sqlshape.PATHS),
                         sorted(["heartbeat", "note", "done", "fail", "release", "cancel", "approve",
                                 "block", "reap", "claim", "bench-record"]))


class Mutations(unittest.TestCase):
    def check_caught(self, label, fn):
        tmp = tempfile.mkdtemp(prefix="mut-")
        try:
            shutil.copy(os.path.join(BOARD_DIR, "schema.sql"), tmp)
            path = os.path.join(tmp, "board")
            with open(path, "w") as f:
                f.write(fn(read_board()))
            self.assertNotEqual(sqlshape.check_all(path), [], "not caught: %s" % label)
        finally:
            shutil.rmtree(tmp, True)

    def test_each_removed_protection_is_caught(self):
        for label, fn in MUTATIONS:
            with self.subTest(label):
                self.check_caught(label, fn)

    def test_the_two_mutations_the_reviewer_tried(self):
        # AND (claim_seq = :fence OR :fence < 0) out of HELD_SQL, and AND claim_seq = :fence out of note
        self.check_caught("review 1", MUTATIONS[0][1])
        self.check_caught("review 2", MUTATIONS[3][1])

    def test_an_unmutated_copy_still_passes(self):
        tmp = tempfile.mkdtemp(prefix="mut-")
        try:
            shutil.copy(os.path.join(BOARD_DIR, "schema.sql"), tmp)
            path = os.path.join(tmp, "board")
            with open(path, "w") as f:
                f.write(read_board())
            self.assertEqual(sqlshape.check_all(path), [])
        finally:
            shutil.rmtree(tmp, True)


class Structure(unittest.TestCase):
    """The checker itself, on small scripts."""

    def test_a_write_outside_the_block_is_reported(self):
        sql = ("BEGIN;\nUPDATE counters SET n = n + 1 WHERE name = 'events';\n"
               "SELECT (state = 'awaiting_approval') AS ok FROM tasks WHERE id = :'task' \\gset\n"
               "UPDATE tasks SET state = 'open' WHERE id = :'task';\nCOMMIT;")
        self.assertTrue([p for p in sqlshape.check_script("approve", sql) if "outside" in p])

    def test_a_write_before_the_guard_is_reported(self):
        sql = ("BEGIN;\nUPDATE tasks SET state = 'open' WHERE id = :'task';\n"
               "SELECT (state = 'awaiting_approval') AS ok FROM tasks WHERE id = :'task' \\gset\n"
               "\\if :ok\nUPDATE tasks SET state = 'open' WHERE id = :'task';\n\\endif\nCOMMIT;")
        self.assertTrue([p for p in sqlshape.check_script("approve", sql) if "before" in p])

    def test_a_protected_script_passes(self):
        sql = ("BEGIN;\nUPDATE counters SET n = n + 1 WHERE name = 'events';\n\\set ok f\n"
               "SELECT (state = 'awaiting_approval') AS ok FROM tasks WHERE id = :'task' \\gset\n"
               "\\if :ok\nUPDATE tasks SET state = 'open', updated_at = :ts WHERE id = :'task';\n\\endif\nCOMMIT;")
        self.assertEqual(sqlshape.check_script("approve", sql), [])


if __name__ == "__main__":
    unittest.main()
