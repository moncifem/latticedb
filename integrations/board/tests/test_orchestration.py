"""Property and scenario tests of the board's orchestration rules, on the executable model.

The clean runs must find no violation of the invariants; every planted bug must be found by some
seed, which shows the checks can fail. See boardmodel.py for what is modelled and what is not.
Run: python3 -m unittest discover -s integrations/board/tests
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import boardmodel as m  # noqa: E402

CLEAN_SEEDS = range(1, 81)
SEARCH_SEEDS = range(1, 701)


def drive(gen):
    """Run an operation to the end with no other agent in between; return its result."""
    try:
        while True:
            next(gen)
    except StopIteration as stop:
        return stop.value


def step(gen):
    """Advance an operation to its next yield point; return (done, value)."""
    try:
        next(gen)
        return False, None
    except StopIteration as stop:
        return True, stop.value


def board_with(*tasks, **kw):
    """A board with tasks added by 'maint'. Each item is an id or (id, {options})."""
    b = m.Board(**kw)
    p = m.Proc("maint")
    for t in tasks:
        tid, opts = (t, {}) if isinstance(t, str) else t
        drive(m.op_add(b, p, tid, **opts))
    return b


class TxnSemantics(unittest.TestCase):
    def test_snapshot_reads_do_not_see_later_commits(self):
        b = board_with("t1")
        reader = b.begin("x", "r")
        w = b.begin("x", "w")
        w.set_task("t1", priority=9)
        w.commit()
        self.assertEqual(reader.task("t1")["priority"], 0)
        self.assertEqual(b.tasks["t1"]["priority"], 9)

    def test_first_committer_wins_on_a_shared_row(self):
        b = board_with("t1")
        t1 = b.begin("x", "a")
        t2 = b.begin("x", "b")
        t1.set_task("t1", priority=1)
        t2.set_task("t1", priority=2)
        t1.commit()
        with self.assertRaises(m.Conflict):
            t2.commit()
        self.assertEqual(b.tasks["t1"]["priority"], 1)

    def test_writes_to_different_rows_do_not_conflict(self):
        b = board_with("t1", "t2")
        t1 = b.begin("x", "a")
        t2 = b.begin("x", "b")
        t1.set_task("t1", priority=1)
        t2.set_task("t2", priority=2)
        t1.commit()
        t2.commit()

    def test_every_counter_writer_conflicts(self):
        b = board_with("t1")
        t1 = b.begin("x", "a")
        t2 = b.begin("x", "b")
        t1.bump()
        t2.bump()
        t1.commit()
        with self.assertRaises(m.Conflict):
            t2.commit()


class Claims(unittest.TestCase):
    def test_two_claimants_exactly_one_wins(self):
        b = board_with("t1")
        pa, pb = m.Proc("a"), m.Proc("b")
        ga, gb = m.op_claim(b, pa, "t1"), m.op_claim(b, pb, "t1")
        # both run to the point just before COMMIT, then commit in turn
        for _ in range(2):
            step(ga)
            step(gb)
        ra, rb = drive(ga), drive(gb)
        kinds = sorted([ra[0], rb[0]])
        self.assertEqual(kinds, ["lost", "won"])
        self.assertEqual(b.tasks["t1"]["state"], "claimed")
        self.assertEqual(sum(1 for e in b.events if e["kind"] == "claim"), 1)
        self.assertEqual(b.violations, [])

    def test_same_name_twice_is_one_claim(self):
        b = board_with("t1")
        p1, p2 = m.Proc("a"), m.Proc("a")
        self.assertEqual(drive(m.op_claim(b, p1, "t1"))[0], "won")
        self.assertEqual(drive(m.op_claim(b, p2, "t1"))[0], "yours")
        self.assertEqual(sum(1 for e in b.events if e["kind"] == "claim"), 1)

    def test_fence_rises_with_every_claim(self):
        b = board_with("t1")
        p = m.Proc("a", lease=10)
        f1 = drive(m.op_claim(b, p, "t1"))[1]
        drive(m.op_release(b, p, "t1"))
        f2 = drive(m.op_claim(b, p, "t1"))[1]
        self.assertGreater(f2, f1)

    def test_the_lease_is_set_from_the_database_clock(self):
        b = board_with("t1")
        b.clock = 5000
        p = m.Proc("a", lease=90)
        drive(m.op_claim(b, p, "t1"))
        self.assertEqual(b.leases["t1"]["lease_until"], 5090)
        self.assertEqual(b.leases["t1"]["heartbeat_at"], 5000)

    def test_a_blocked_task_is_not_claimed_until_its_blocker_is_done(self):
        b = board_with("t1", ("t2", {"blocked_by": ["t1"]}))
        p = m.Proc("a")
        self.assertEqual(drive(m.op_claim(b, p, "t2")), ("blocked", ["t1"]))
        drive(m.op_claim(b, p, "t1"))
        drive(m.op_done(b, p, "t1"))
        self.assertEqual(drive(m.op_claim(b, p, "t2"))[0], "won")

    def test_a_dead_or_cancelled_blocker_blocks_for_ever(self):
        b = board_with("t1", ("t2", {"blocked_by": ["t1"]}))
        drive(m.op_cancel(b, m.Proc("maint"), "t1"))
        self.assertEqual(drive(m.op_claim(b, m.Proc("a"), "t2"))[0], "blocked")

    def test_next_takes_the_highest_priority_then_the_oldest(self):
        b = board_with(("t1", {"priority": 1}), ("t2", {"priority": 5}), ("t3", {"priority": 5}))
        p = m.Proc("a", cap=3)
        order = [drive(m.op_next(b, p))[1] for _ in range(3)]
        self.assertEqual(order, ["t2", "t3", "t1"])

    def test_next_skips_blocked_tasks_and_waits_out_backoff(self):
        b = board_with(("t1", {"priority": 9, "blocked_by": ["t9"]}), "t9x", ("t2", {"priority": 1}))
        b.tasks["t9x"]["state"] = "cancelled"
        p = m.Proc("a")
        self.assertEqual(drive(m.op_next(b, p))[1], "t2")
        drive(m.op_fail(b, p, "t2", retry_in=100))
        self.assertEqual(drive(m.op_next(b, p)), ("none",))
        b.clock += 101
        self.assertEqual(drive(m.op_next(b, p))[1], "t2")

    def test_next_stops_at_the_cap(self):
        b = board_with("t1", "t2")
        p = m.Proc("a", cap=1)
        self.assertEqual(drive(m.op_next(b, p))[0], "claimed")
        self.assertEqual(drive(m.op_next(b, p)), ("full",))

    def test_approval_gate(self):
        b = board_with(("t1", {"needs_approval": True}))
        p = m.Proc("a")
        self.assertEqual(b.tasks["t1"]["state"], "awaiting_approval")
        self.assertEqual(drive(m.op_next(b, p)), ("none",))
        self.assertTrue(drive(m.op_approve(b, m.Proc("boss"), "t1")))
        self.assertFalse(drive(m.op_approve(b, m.Proc("boss"), "t1")))  # nothing left to approve
        self.assertEqual(drive(m.op_next(b, p))[1], "t1")


class Fencing(unittest.TestCase):
    def lost_lease(self):
        """a claims t1, stalls past its lease, the task is reaped and b claims it."""
        b = board_with("t1")
        a, b2 = m.Proc("a", lease=10), m.Proc("b", lease=10)
        drive(m.op_claim(b, a, "t1"))
        b.clock += 50
        self.assertEqual(drive(m.op_reap_all(b, m.Proc("reaper"))), 1)
        b.clock += 100  # past the retry delay
        self.assertEqual(drive(m.op_claim(b, b2, "t1"))[0], "won")
        return b, a, b2

    def test_a_zombie_done_is_refused_and_changes_nothing(self):
        b, a, b2 = self.lost_lease()
        before = dict(b.tasks["t1"])
        self.assertEqual(drive(m.op_done(b, a, "t1")), ("refused",))
        self.assertEqual(b.tasks["t1"], before)
        self.assertEqual(b.tasks["t1"]["owner"], "b")
        self.assertEqual(b.violations, [])

    def test_a_zombie_heartbeat_note_fail_release_are_refused(self):
        for op in (m.op_heartbeat, m.op_note, m.op_fail, m.op_release):
            b, a, b2 = self.lost_lease()
            before = (dict(b.tasks["t1"]), dict(b.leases["t1"]), len(b.events))
            res = drive(op(b, a, "t1"))
            self.assertIn(res[0], ("lost", "refused"), op.__name__)
            self.assertEqual((b.tasks["t1"], b.leases["t1"], len(b.events)), before, op.__name__)
            self.assertEqual(b.violations, [])

    def test_a_zombie_with_the_same_name_is_still_fenced(self):
        b = board_with("t1")
        old, new = m.Proc("a", lease=10), m.Proc("a", lease=10)
        drive(m.op_claim(b, old, "t1"))
        b.clock += 50
        drive(m.op_reap_all(b, m.Proc("reaper")))
        b.clock += 100
        self.assertEqual(drive(m.op_claim(b, new, "t1"))[0], "won")
        self.assertEqual(drive(m.op_done(b, old, "t1")), ("refused",))
        self.assertEqual(drive(m.op_done(b, new, "t1")), ("done",))

    def test_heartbeat_extends_only_a_held_lease(self):
        b = board_with("t1")
        p = m.Proc("a", lease=30)
        drive(m.op_claim(b, p, "t1"))
        b.clock += 20
        self.assertEqual(drive(m.op_heartbeat(b, p, "t1")), ("ok",))
        self.assertEqual(b.leases["t1"]["lease_until"], b.clock + 30)

    def test_heartbeat_does_not_conflict_with_an_unrelated_writer_but_does_with_reap(self):
        b = board_with("t1", "t2")
        p = m.Proc("a", lease=10)
        drive(m.op_claim(b, p, "t1"))
        hb = m.op_heartbeat(b, p, "t1")
        step(hb)  # in flight, before COMMIT
        drive(m.op_add(b, m.Proc("maint"), "t3"))  # bumps the counter: no conflict for a heartbeat
        self.assertEqual(drive(hb), ("ok",))
        # now a reap that lands first forces the heartbeat to retry and see the new state
        b.clock += 50
        hb = m.op_heartbeat(b, p, "t1")
        step(hb)
        drive(m.op_reap_all(b, m.Proc("reaper")))
        self.assertEqual(drive(hb), ("lost",))

    def test_reap_loses_to_a_heartbeat_that_lands_after_the_look(self):
        b = board_with("t1")
        p = m.Proc("a", lease=10)
        drive(m.op_claim(b, p, "t1"))
        b.clock += 20  # expired, not yet reaped
        rg = m.op_reap_all(b, m.Proc("reaper"))
        step(rg)  # the reaper has chosen t1
        self.assertEqual(drive(m.op_heartbeat(b, p, "t1")), ("ok",))  # the owner is alive after all
        self.assertEqual(drive(rg), 0)
        self.assertEqual(b.tasks["t1"]["state"], "claimed")
        self.assertEqual(b.violations, [])

    def test_two_reapers_expire_a_claim_once(self):
        b = board_with("t1")
        drive(m.op_claim(b, m.Proc("a", lease=10), "t1"))
        b.clock += 20
        r1, r2 = m.Proc("r1"), m.Proc("r2")
        g1, g2 = m.op_reap_all(b, r1), m.op_reap_all(b, r2)
        for _ in range(3):
            step(g1)
            step(g2)
        n = drive(g1) + drive(g2)
        self.assertEqual(n, 1)
        self.assertEqual(b.leases["t1"]["attempts"], 1)
        self.assertEqual(sum(1 for e in b.events if e["kind"] == "expired"), 1)


class RetryAndDone(unittest.TestCase):
    def test_attempts_end_in_dead_not_in_a_fourth_try(self):
        b = board_with(("t1", {"max_attempts": 3}))
        p = m.Proc("a")
        results = []
        for _ in range(3):
            b.clock += 10_000
            drive(m.op_claim(b, p, "t1"))
            results.append(drive(m.op_fail(b, p, "t1", retry_in=1)))
        self.assertEqual(results, [("retry",), ("retry",), ("dead",)])
        self.assertEqual(b.tasks["t1"]["state"], "dead")
        self.assertEqual(b.leases["t1"]["attempts"], 3)
        b.clock += 10_000
        self.assertEqual(drive(m.op_claim(b, p, "t1"))[0], "lost")
        self.assertEqual(b.violations, [])

    def test_expiry_counts_as_an_attempt(self):
        b = board_with(("t1", {"max_attempts": 2}))
        p = m.Proc("a", lease=5)
        for _ in range(2):
            b.clock += 10_000
            drive(m.op_claim(b, p, "t1"))
            b.clock += 100
            drive(m.op_reap_all(b, m.Proc("reaper")))
        self.assertEqual(b.tasks["t1"]["state"], "dead")
        self.assertEqual(b.leases["t1"]["last_error"], "lease expired")

    def test_default_backoff_doubles_and_is_capped(self):
        self.assertEqual([m.backoff_for(n) for n in range(0, 9)],
                         [30, 60, 120, 240, 480, 960, 1920, 3600, 3600])

    def test_the_retry_delay_uses_the_database_clock(self):
        b = board_with("t1")
        b.clock = 7000
        p = m.Proc("a")
        drive(m.op_claim(b, p, "t1"))
        drive(m.op_fail(b, p, "t1", retry_in=40))
        self.assertEqual(b.leases["t1"]["not_before"], 7040)

    def test_done_twice_is_a_no_op(self):
        b = board_with("t1")
        p = m.Proc("a")
        drive(m.op_claim(b, p, "t1"))
        self.assertEqual(drive(m.op_done(b, p, "t1")), ("done",))
        n_events = len(b.events)
        self.assertEqual(drive(m.op_done(b, p, "t1")), ("noop",))
        self.assertEqual(len(b.events), n_events)
        self.assertEqual(b.violations, [])

    def test_a_done_task_never_reopens(self):
        b = board_with("t1")
        p = m.Proc("a", lease=5)
        drive(m.op_claim(b, p, "t1"))
        drive(m.op_done(b, p, "t1"))
        b.clock += 100
        self.assertEqual(drive(m.op_reap_all(b, m.Proc("reaper"))), 0)
        self.assertFalse(drive(m.op_cancel(b, m.Proc("maint"), "t1")))
        self.assertEqual(b.tasks["t1"]["state"], "done")


class Properties(unittest.TestCase):
    def test_clean_runs_hold_every_invariant(self):
        bad = {}
        for seed in CLEAN_SEEDS:
            b = m.run_seed(seed)
            if b.violations:
                bad[seed] = b.violations[:3]
        self.assertEqual(bad, {})

    def test_the_clean_runs_exercise_the_interesting_paths(self):
        total = m.Counter()
        for seed in CLEAN_SEEDS:
            total.update(m.run_seed(seed).stats)
        for key in ("claim_won", "heartbeat_ok", "heartbeat_lost", "expired_retry", "expired_dead",
                    "fail_retry", "fail_dead", "done", "done_noop", "refused", "release", "conflict"):
            self.assertGreater(total[key], 0, key)

    def test_a_run_is_deterministic(self):
        a, b = m.run_seed(11), m.run_seed(11)
        self.assertEqual(a.events, b.events)
        self.assertEqual(a.tasks, b.tasks)
        self.assertNotEqual(m.run_seed(12).events, a.events)

    def test_reap_is_idempotent_once_leases_have_run_out(self):
        for seed in range(1, 21):
            r = m.Run(seed)
            b = r.run()
            snap = ({k: dict(v) for k, v in b.tasks.items()},
                    {k: dict(v) for k, v in b.leases.items()}, list(b.events))
            self.assertEqual(drive(m.op_reap_all(b, m.Proc("again"))), 0)
            self.assertEqual(({k: dict(v) for k, v in b.tasks.items()},
                              {k: dict(v) for k, v in b.leases.items()}, list(b.events)), snap)

    def test_no_task_is_lost_and_every_task_settles(self):
        for seed in range(1, 31):
            b = m.run_seed(seed)
            states = {t["state"] for t in b.tasks.values()}
            self.assertTrue(states <= {"open", "done", "dead", "cancelled", "awaiting_approval"}, states)
            self.assertGreater(len(b.tasks), 3)

    def test_at_most_one_claim_event_per_fence_and_fences_are_unique(self):
        for seed in range(1, 21):
            b = m.run_seed(seed)
            claims = [e["id"] for e in b.events if e["kind"] == "claim"]
            self.assertEqual(len(claims), len(set(claims)))
            ids = [e["id"] for e in b.events]
            self.assertEqual(len(ids), len(set(ids)))

    def test_attempts_never_exceed_max(self):
        for seed in range(1, 31):
            b = m.run_seed(seed)
            for tid, lease in b.leases.items():
                self.assertLessEqual(lease["attempts"], lease["max_attempts"], tid)

    def test_a_sequential_agent_never_holds_more_than_its_cap(self):
        # An agent that uses only next, one call at a time, is exact: cap 1 means one claimed task.
        b = board_with(*["t%d" % i for i in range(1, 6)])
        p = m.Proc("solo", cap=1, lease=1000)
        for _ in range(10):
            drive(m.op_next(b, p))
            mine = [t for t in b.tasks.values() if t["state"] == "claimed" and t["owner"] == "solo"]
            self.assertLessEqual(len(mine), 1)


class PlantedBugs(unittest.TestCase):
    """Each fault, switched on, must be caught by the same checks that pass on the clean model."""

    def expect(self, bug, kinds):
        found = m.first_violation(bug, SEARCH_SEEDS)
        self.assertIsNotNone(found, "no seed caught the planted bug %s" % bug)
        seed, got = found
        self.assertTrue(set(kinds) & set(got), (bug, seed, got))

    def test_a_stale_fence_that_is_accepted(self):
        self.expect("stale_fence_accepted", ["stale_write_accepted"])

    def test_a_heartbeat_that_ignores_the_fence(self):
        self.expect("heartbeat_ignores_fence", ["stale_write_accepted"])

    def test_a_reap_without_the_compare(self):
        self.expect("reap_without_cas", ["reaped_live_lease", "done_reopened"])

    def test_a_done_that_appends_twice(self):
        self.expect("done_twice_appends", ["done_event_twice"])

    def test_next_that_ignores_blockers(self):
        self.expect("claim_ignores_blockers", ["claimed_while_blocked"])

    def test_fail_that_never_goes_dead(self):
        self.expect("fail_never_dead", ["attempts_exceed_max", "open_with_no_attempts_left"])

    def test_every_planted_bug_has_a_test(self):
        tested = {n[len("test_"):] for n in dir(self) if n.startswith("test_")}
        self.assertEqual(len(m.BUGS), 6)
        self.assertGreaterEqual(len(tested), len(m.BUGS))

    def test_an_unknown_bug_name_is_refused(self):
        with self.assertRaises(ValueError):
            m.Board(bugs=("no_such_bug",))


if __name__ == "__main__":
    unittest.main()
