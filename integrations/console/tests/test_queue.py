"""The queue: runnable, blocked, claimed, retrying, dead and awaiting tasks, built from task rows
and lease rows, and the extra needs-attention items. Hand-derived expectations throughout."""

import unittest

import helpers  # noqa: F401
from helpers import FakeSource, lease, task

from console import model
from console.service import BoardService

NOW = 10_000


def queue(tasks, leases, now=NOW, available=True):
    return model.build_queue(tasks, leases, now, available)


def ids(rows):
    return [r["id"] for r in rows]


class SplitBlockers(unittest.TestCase):
    def test_splits_and_trims(self):
        self.assertEqual(model.split_blockers("a, b ,c"), ["a", "b", "c"])

    def test_drops_blanks_and_repeats(self):
        self.assertEqual(model.split_blockers(",a,,a,b,"), ["a", "b"])

    def test_none_and_empty(self):
        self.assertEqual(model.split_blockers(None), [])
        self.assertEqual(model.split_blockers(""), [])

    def test_is_bounded(self):
        self.assertEqual(len(model.split_blockers(",".join("t%d" % i for i in range(500)))), 64)


class Runnable(unittest.TestCase):
    def test_an_open_task_with_no_lease_row_is_runnable_with_defaults(self):
        q = queue([task("a")], [])
        self.assertEqual(ids(q["runnable"]), ["a"])
        self.assertEqual((q["runnable"][0]["attempts"], q["runnable"][0]["max_attempts"]), (0, 3))

    def test_order_is_priority_then_creation(self):
        tasks = [task("lo", priority=1, seq=1), task("hi2", priority=5, seq=3),
                 task("hi1", priority=5, seq=2)]
        self.assertEqual(ids(queue(tasks, [])["runnable"]), ["hi1", "hi2", "lo"])

    def test_the_retry_delay_is_inclusive_at_the_boundary(self):
        # not_before == now is runnable, as `board next` compares not_before <= now
        tasks = [task("a"), task("b")]
        leases = [lease("a", not_before=NOW), lease("b", not_before=NOW + 1)]
        q = queue(tasks, leases)
        self.assertEqual(ids(q["runnable"]), ["a"])
        self.assertEqual(ids(q["retrying"]), ["b"])

    def test_done_cancelled_dead_and_claimed_are_not_runnable(self):
        tasks = [task("d", "done"), task("c", "cancelled"), task("x", "dead"), task("k", "claimed", "bob")]
        self.assertEqual(queue(tasks, [])["runnable"], [])


class Blocked(unittest.TestCase):
    def test_a_task_waits_for_a_blocker_that_is_not_done(self):
        tasks = [task("a"), task("b")]
        q = queue(tasks, [lease("b", blocked="a")])
        self.assertEqual(ids(q["runnable"]), ["a"])
        self.assertEqual(ids(q["blocked"]), ["b"])
        row = q["blocked"][0]
        self.assertEqual(row["blockers"][0]["id"], "a")
        self.assertFalse(row["forever"])

    def test_it_becomes_runnable_when_every_blocker_is_done(self):
        tasks = [task("a", "done"), task("a2", "done"), task("b")]
        q = queue(tasks, [lease("b", blocked="a,a2")])
        self.assertEqual(ids(q["runnable"]), ["b"])

    def test_one_unfinished_blocker_is_enough(self):
        tasks = [task("a", "done"), task("a2", "claimed", "x"), task("b")]
        q = queue(tasks, [lease("b", blocked="a,a2")])
        self.assertEqual(ids(q["blocked"]), ["b"])
        self.assertEqual([b["id"] for b in q["blocked"][0]["blockers"]], ["a2"])

    def test_a_dead_blocker_blocks_for_ever(self):
        q = queue([task("a", "dead"), task("b")], [lease("b", blocked="a")])
        row = q["blocked"][0]
        self.assertTrue(row["forever"])
        self.assertEqual(row["blockers"][0]["problem"], "dead")

    def test_a_cancelled_blocker_blocks_for_ever(self):
        row = queue([task("a", "cancelled"), task("b")], [lease("b", blocked="a")])["blocked"][0]
        self.assertTrue(row["forever"])
        self.assertEqual(row["blockers"][0]["problem"], "cancelled")

    def test_a_missing_blocker_blocks_for_ever(self):
        row = queue([task("b")], [lease("b", blocked="ghost")])["blocked"][0]
        self.assertTrue(row["forever"])
        self.assertEqual(row["blockers"][0]["problem"], "missing")
        self.assertIsNone(row["blockers"][0]["title"])

    def test_a_cycle_blocks_for_ever(self):
        tasks = [task("a"), task("b"), task("c")]
        leases = [lease("a", blocked="b"), lease("b", blocked="c"), lease("c", blocked="a")]
        q = queue(tasks, leases)
        self.assertEqual(sorted(ids(q["blocked"])), ["a", "b", "c"])
        self.assertTrue(all(r["forever"] and r["cycle"] for r in q["blocked"]))

    def test_a_task_that_waits_for_itself_is_a_cycle(self):
        row = queue([task("a")], [lease("a", blocked="a")])["blocked"][0]
        self.assertTrue(row["cycle"])

    def test_a_chain_is_not_a_cycle(self):
        tasks = [task("a"), task("b"), task("c")]
        q = queue(tasks, [lease("b", blocked="a"), lease("c", blocked="b")])
        self.assertFalse(any(r["cycle"] or r["forever"] for r in q["blocked"]))
        self.assertEqual(ids(q["runnable"]), ["a"])

    def test_a_blocked_task_is_not_also_retrying(self):
        q = queue([task("a"), task("b")], [lease("b", blocked="a", not_before=NOW + 50)])
        self.assertEqual(ids(q["blocked"]), ["b"])
        self.assertEqual(q["retrying"], [])

    def test_a_long_cycle_walk_is_bounded(self):
        n = 400
        tasks = [task("t%d" % i) for i in range(n)]
        leases = [lease("t%d" % i, blocked="t%d" % ((i + 1) % n)) for i in range(n)]
        q = queue(tasks, leases)  # must return, whatever it concludes about the far cycle
        self.assertEqual(q["totals"]["blocked"], n)


class Claimed(unittest.TestCase):
    def test_remaining_time_and_heartbeat_age(self):
        t = [task("a", "claimed", "bob")]
        row = queue(t, [lease("a", until=NOW + 300, hb=NOW - 20)])["claimed"][0]
        self.assertEqual((row["remaining_s"], row["heartbeat_age_s"], row["expired"]), (300, 20, False))
        self.assertEqual(row["owner"], "bob")

    def test_a_lease_in_the_past_is_expired(self):
        row = queue([task("a", "claimed", "bob")], [lease("a", until=NOW - 5, hb=NOW - 700)])["claimed"][0]
        self.assertTrue(row["expired"])
        self.assertEqual(row["remaining_s"], -5)

    def test_a_lease_that_ends_exactly_now_is_not_expired(self):
        # reap expires lease_until < now, so == now is still held
        row = queue([task("a", "claimed", "bob")], [lease("a", until=NOW)])["claimed"][0]
        self.assertFalse(row["expired"])

    def test_a_claim_with_no_lease_is_shown_without_a_countdown(self):
        row = queue([task("a", "claimed", "bob")], [])["claimed"][0]
        self.assertFalse(row["has_lease"])
        self.assertIsNone(row["remaining_s"])
        self.assertFalse(row["expired"])
        self.assertIsNone(row["heartbeat_age_s"])

    def test_the_soonest_to_expire_comes_first(self):
        tasks = [task("a", "claimed", "x"), task("b", "claimed", "y"), task("c", "claimed", "z")]
        leases = [lease("a", until=NOW + 500), lease("b", until=NOW - 10), lease("c", until=NOW + 5)]
        self.assertEqual(ids(queue(tasks, leases)["claimed"]), ["b", "c", "a"])


class RetryingDeadAwaiting(unittest.TestCase):
    def test_retrying_shows_attempts_wait_and_error(self):
        t = [task("a")]
        row = queue(t, [lease("a", attempts=2, not_before=NOW + 90, last_error="CI red")])["retrying"][0]
        self.assertEqual((row["attempts"], row["max_attempts"], row["wait_s"]), (2, 3, 90))
        self.assertEqual(row["last_error"], "CI red")

    def test_retrying_is_ordered_by_next_try(self):
        tasks = [task("a"), task("b")]
        leases = [lease("a", not_before=NOW + 90), lease("b", not_before=NOW + 10)]
        self.assertEqual(ids(queue(tasks, leases)["retrying"]), ["b", "a"])

    def test_dead_letter_list_carries_the_last_error(self):
        q = queue([task("a", "dead")], [lease("a", attempts=3, last_error="lease expired")])
        self.assertEqual(q["dead"][0]["last_error"], "lease expired")
        self.assertEqual(q["dead"][0]["attempts"], 3)

    def test_awaiting_approval(self):
        q = queue([task("a", "awaiting_approval", created=NOW - 100)], [])
        self.assertEqual(ids(q["awaiting"]), ["a"])
        self.assertEqual(q["awaiting"][0]["age_s"], 100)
        self.assertEqual(q["runnable"], [])

    def test_long_text_is_cut(self):
        row = queue([task("a", "dead")], [lease("a", last_error="x" * 9000)])["dead"][0]
        self.assertEqual(len(row["last_error"]), 300)


class Totals(unittest.TestCase):
    def test_totals_count_every_row_and_lists_are_capped(self):
        n = model.QUEUE_LIST_LIMIT + 7
        q = queue([task("t%d" % i, seq=i) for i in range(n)], [])
        self.assertEqual(q["totals"]["runnable"], n)
        self.assertEqual(len(q["runnable"]), model.QUEUE_LIST_LIMIT)
        self.assertTrue(q["truncated"]["runnable"])
        self.assertFalse(q["truncated"]["dead"])

    def test_the_flag_for_a_board_without_leases_is_passed_on(self):
        self.assertFalse(queue([task("a")], [], available=False)["leases_available"])

    def test_an_empty_board(self):
        q = queue([], [])
        self.assertEqual(sum(q["totals"].values()), 0)


class QueueAttention(unittest.TestCase):
    def build(self):
        tasks = [task("exp", "claimed", "bob", title="expired one"), task("live", "claimed", "amy"),
                 task("dead", "dead"), task("cx", "cancelled"), task("blk"), task("ok"),
                 task("later"), task("loop1"), task("loop2")]
        leases = [lease("exp", until=NOW - 400), lease("live", until=NOW + 400),
                  lease("dead", attempts=3, last_error="boom"), lease("blk", blocked="cx"),
                  lease("later", blocked="ok"), lease("loop1", blocked="loop2"),
                  lease("loop2", blocked="loop1")]
        return model.queue_attention(queue(tasks, leases))

    def test_expired_leases_are_listed_with_the_overdue_time(self):
        a = self.build()
        self.assertEqual([(x["task_id"], x["owner"], x["overdue_s"]) for x in a["expired_leases"]],
                         [("exp", "bob", 400)])

    def test_dead_tasks_are_listed(self):
        a = self.build()
        self.assertEqual([(x["task_id"], x["last_error"]) for x in a["dead_tasks"]], [("dead", "boom")])

    def test_blocked_for_ever_names_the_reason_and_leaves_out_ordinary_waits(self):
        a = self.build()
        got = {x["task_id"]: x["reasons"] for x in a["blocked_forever"]}
        self.assertEqual(sorted(got), ["blk", "loop1", "loop2"])
        self.assertEqual(got["blk"], ["cx (cancelled)"])
        self.assertIn("a cycle", got["loop1"])

    def test_every_item_comes_with_a_rule(self):
        a = self.build()
        self.assertEqual(sorted(a["queue_rules"]), ["blocked_forever", "dead_tasks", "expired_leases"])


class ThroughTheService(unittest.TestCase):
    def service(self, tasks, leases):
        src = FakeSource(tasks, [], [], now=NOW, leases=leases)
        return BoardService(src, ttl=0.0)

    def test_now_payload_has_the_new_attention_items_and_rules(self):
        s = self.service([task("e", "claimed", "bob"), task("d", "dead")],
                         [lease("e", until=NOW - 1), lease("d", attempts=3)])
        a = s.now_payload()["attention"]
        self.assertEqual(len(a["expired_leases"]), 1)
        self.assertEqual(len(a["dead_tasks"]), 1)
        for key in ("expired_leases", "dead_tasks", "blocked_forever", "stale", "open_too_long"):
            self.assertIn(key, a["rules"])

    def test_queue_payload_totals_and_rules(self):
        s = self.service([task("a"), task("b", "awaiting_approval")], [])
        q = s.queue_payload()
        self.assertEqual(q["totals"]["runnable"], 1)
        self.assertEqual(q["totals"]["awaiting"], 1)
        self.assertEqual(sorted(q["rules"]), ["awaiting", "blocked", "claimed", "dead", "retrying", "runnable"])
        self.assertEqual(q["meta"]["clock"], "database")
        self.assertTrue(q["meta"]["leases_available"])

    def test_a_source_without_leases_still_works(self):
        class Old(FakeSource):
            leases = None
        src = Old([task("a")], [], [], now=NOW)
        q = BoardService(src, ttl=0.0).queue_payload()
        self.assertFalse(q["leases_available"])
        self.assertEqual(q["totals"]["runnable"], 1)

    def test_the_client_clock_is_reported(self):
        src = FakeSource([task("a")], [], [], now=NOW)
        src.clock_source = "client"
        self.assertEqual(BoardService(src, ttl=0.0).queue_payload()["meta"]["clock"], "client")


class BlockerOutsideTheWindow(unittest.TestCase):
    def test_a_missing_blocker_is_unknown_when_the_task_window_was_cut(self):
        q = model.build_queue([task("b")], [lease("b", blocked="old1")], NOW, True, tasks_truncated=True)
        row = q["blocked"][0]
        self.assertEqual(row["blockers"][0]["problem"], "not_in_window")
        self.assertFalse(row["forever"])  # not known to be hopeless
        self.assertEqual(model.queue_attention(q)["blocked_forever"], [])

    def test_the_same_blocker_is_missing_when_nothing_was_cut(self):
        q = model.build_queue([task("b")], [lease("b", blocked="old1")], NOW, True, tasks_truncated=False)
        self.assertEqual(q["blocked"][0]["blockers"][0]["problem"], "missing")
        self.assertTrue(q["blocked"][0]["forever"])

    def test_a_dead_blocker_in_the_window_is_still_for_ever_when_the_window_was_cut(self):
        q = model.build_queue([task("a", "dead"), task("b")], [lease("b", blocked="a,old1")], NOW, True, True)
        self.assertTrue(q["blocked"][0]["forever"])
        self.assertEqual(model.queue_attention(q)["blocked_forever"][0]["reasons"], ["a (dead)"])

    def test_the_service_passes_the_truncation_flag(self):
        src = FakeSource([task("b")], [], [], now=NOW, leases=[lease("b", blocked="old1")])
        s = BoardService(src, ttl=0.0)
        self.assertEqual(s.queue_payload()["blocked"][0]["blockers"][0]["problem"], "missing")
        src.tasks = lambda limit: ([task("b")], True)
        s2 = BoardService(src, ttl=0.0)
        self.assertEqual(s2.queue_payload()["blocked"][0]["blockers"][0]["problem"], "not_in_window")

    def test_the_screen_says_not_in_window(self):
        import os
        with open(os.path.join(helpers.STATIC, "app.js")) as f:
            js = f.read()
        self.assertIn("not in window", js)
        self.assertIn("older than the loaded tasks", js)


if __name__ == "__main__":
    unittest.main()
