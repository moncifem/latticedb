import unittest

import helpers  # noqa: F401

from console import demo, model


class GeneratorTests(unittest.TestCase):
    def test_same_seed_same_board(self):
        a, b = demo.generate(7), demo.generate(7)
        self.assertEqual(a["events"], b["events"])
        self.assertEqual(a["decisions"], b["decisions"])
        self.assertEqual(a["specs"], b["specs"])

    def test_different_seed_different_board(self):
        self.assertNotEqual(demo.generate(7)["events"], demo.generate(8)["events"])

    def test_lcg_is_pinned(self):
        # Guards the "deterministic on every Python" claim: the generator uses its own arithmetic.
        r = demo.Lcg(1)
        first = [r.next() for _ in range(3)]
        r2 = demo.Lcg(1)
        self.assertEqual(first, [r2.next() for _ in range(3)])
        self.assertTrue(all(0 <= v < 2**47 for v in first))
        x = (1 * 2862933555777941757 + 3037000493) & 0xFFFFFFFFFFFFFFFF
        x = (x * 6364136223846793005 + 1442695040888963407) & 0xFFFFFFFFFFFFFFFF
        self.assertEqual(first[0], x >> 17)

    def test_event_ids_are_contiguous_and_times_do_not_go_backwards(self):
        ev = demo.generate(7)["events"]
        self.assertEqual([e["id"] for e in ev], list(range(1, len(ev) + 1)))
        times = [e["created_at"] for e in ev]
        self.assertEqual(times, sorted(times))

    def test_task_ids_are_unique(self):
        specs = demo.generate(7)["specs"]
        ids = [s["id"] for s in specs]
        self.assertEqual(len(ids), len(set(ids)))

    def test_every_event_names_a_known_task_and_kind(self):
        d = demo.generate(7)
        known = {s["id"] for s in d["specs"]}
        for e in d["events"]:
            self.assertIn(e["task_id"], known)
            self.assertIn(e["kind"], model.EVENT_KINDS)
            self.assertIn(e["author"], demo.AGENTS)

    def test_decision_rows_match_decision_events(self):
        d = demo.generate(7)
        ev = {e["id"]: e for e in d["events"] if e["kind"] == "decision"}
        self.assertEqual({x["id"] for x in d["decisions"]}, set(ev))
        for x in d["decisions"]:
            self.assertEqual(x["body"], ev[x["id"]]["body"])

    def test_the_board_is_big_enough_to_show_numbers(self):
        src = demo.DemoSource(reveal_every=0)
        tasks, _ = src.tasks(5000)
        top = [t for t in tasks if not t["parent_id"]]
        self.assertGreaterEqual(len(top), 20)  # the completion-rate minimum
        self.assertGreaterEqual(len([t for t in tasks if t["state"] == "done"]), 30)

    def test_no_text_contains_a_tab_or_newline(self):
        for e in demo.generate(7)["events"]:
            self.assertNotRegex(e["body"], r"[\t\n\r]")


class PlantedRowsTests(unittest.TestCase):
    def setUp(self):
        self.src = demo.DemoSource(reveal_every=0)
        self.tasks, _ = self.src.tasks(5000)
        self.events, _ = self.src.events_latest(10000)
        self.decisions = self.src.decisions_latest(5000)
        self.now = self.src.now()
        self.facts = model.task_facts(self.tasks, self.events, self.decisions)

    def test_a_stale_claim_exists(self):
        a = model.attention(self.tasks, self.facts, self.now, 1800, 86400)
        self.assertGreaterEqual(len(a["stale"]), 1)

    def test_a_double_claim_exists(self):
        a = model.attention(self.tasks, self.facts, self.now, 1800, 86400)
        self.assertEqual(len(a["double_claims"]), 1)

    def test_open_too_long_exists(self):
        a = model.attention(self.tasks, self.facts, self.now, 1800, 86400)
        self.assertGreaterEqual(len(a["open_too_long"]), 1)

    def test_orphan_and_cycle_are_walkable_without_looping(self):
        by_id = {t["id"]: t for t in self.tasks}
        issues = {model.parent_chain(t["id"], by_id)["issue"] for t in self.tasks}
        self.assertIn("missing_parent", issues)
        self.assertIn("cycle", issues)

    def test_agents_cover_all_three_states(self):
        agents = model.build_agents(self.tasks, self.events, self.facts, self.now, 1800)
        self.assertEqual({a["state"] for a in agents}, {"working", "stale", "idle"})

    def test_a_hostile_looking_note_is_present_as_plain_data(self):
        bodies = [e["body"] for e in self.events]
        self.assertTrue([b for b in bodies if "<img" in b and "onerror" in b])


class RevealTests(unittest.TestCase):
    def make(self, step=4.0):
        t = [0.0]
        src = demo.DemoSource(reveal_every=step, clock=lambda: t[0])
        return src, t

    def test_one_more_event_every_interval(self):
        src, t = self.make(4.0)
        start = src.visible()
        t[0] = 3.9
        self.assertEqual(src.visible(), start)
        t[0] = 4.0
        self.assertEqual(src.visible(), start + 1)
        t[0] = 12.5
        self.assertEqual(src.visible(), start + 3)

    def test_reveal_stops_at_the_full_board(self):
        src, t = self.make(1.0)
        t[0] = 10**6
        self.assertEqual(src.visible(), len(src.events))

    def test_events_after_returns_only_revealed_events(self):
        src, t = self.make(4.0)
        last = src.visible()
        self.assertEqual([e["id"] for e in src.events_after(last, 100)], [])
        t[0] = 8.0
        self.assertEqual([e["id"] for e in src.events_after(last, 100)], [last + 1, last + 2])

    def test_tasks_follow_the_revealed_events(self):
        src, t = self.make(4.0)
        n0 = len(src.tasks(5000)[0])
        t[0] = 10**6
        self.assertGreaterEqual(len(src.tasks(5000)[0]), n0)

    def test_now_follows_the_last_visible_event(self):
        src, t = self.make(4.0)
        n0 = src.now()
        t[0] = 40.0
        self.assertGreaterEqual(src.now(), n0)

    def test_frozen_demo_never_changes(self):
        src = demo.DemoSource(reveal_every=0)
        self.assertEqual(src.visible(), len(src.events))
        a = src.tasks(5000)
        self.assertEqual(a, src.tasks(5000))

    def test_source_flags(self):
        src = demo.DemoSource(reveal_every=0)
        self.assertTrue(src.demo)
        self.assertEqual(src.label, "DEMO DATA, not your board")

    def test_returned_rows_are_copies(self):
        src = demo.DemoSource(reveal_every=0)
        rows = src.events_after(0, 5)
        rows[0]["body"] = "tampered"
        self.assertNotEqual(src.events_after(0, 5)[0]["body"], "tampered")

    def test_make_demo_helper(self):
        self.assertEqual(demo.make_demo(0.0).visible(), len(demo.make_demo(0.0).events))


class OrchestrationExamplesTests(unittest.TestCase):
    """The demo board shows every part of the queue, so the screens are never empty."""

    def setUp(self):
        from console.service import BoardService
        self.s = BoardService(demo.DemoSource(reveal_every=0), ttl=0.0)
        self.q = self.s.queue_payload()

    def test_every_queue_section_has_a_row(self):
        for k in ("runnable", "blocked", "claimed", "retrying", "dead", "awaiting"):
            self.assertGreaterEqual(self.q["totals"][k], 1, k)

    def test_there_are_lease_states_of_each_kind(self):
        claimed = self.q["claimed"]
        self.assertTrue([r for r in claimed if r["expired"]])
        self.assertTrue([r for r in claimed if not r["expired"] and r["remaining_s"] is not None and r["remaining_s"] > 0])
        self.assertTrue([r for r in claimed if r["heartbeat_age_s"] is not None])

    def test_blocked_has_an_ordinary_wait_and_a_wait_for_ever(self):
        flags = {r["forever"] for r in self.q["blocked"]}
        self.assertEqual(flags, {True, False})
        problems = {b["problem"] for r in self.q["blocked"] for b in r["blockers"]}
        self.assertTrue({"dead", "cancelled"} <= problems)

    def test_the_retrying_and_dead_rows_have_attempts_and_an_error(self):
        r = self.q["retrying"][0]
        self.assertEqual((r["attempts"], r["max_attempts"]), (1, 3))
        self.assertTrue(r["last_error"])
        d = self.q["dead"][0]
        self.assertEqual(d["attempts"], d["max_attempts"])
        self.assertTrue(d["last_error"])

    def test_attention_has_the_new_items(self):
        a = self.s.now_payload()["attention"]
        self.assertGreaterEqual(len(a["expired_leases"]), 1)
        self.assertGreaterEqual(len(a["dead_tasks"]), 1)
        self.assertGreaterEqual(len(a["blocked_forever"]), 2)

    def test_the_new_event_kinds_appear_in_the_feed(self):
        kinds = {e["kind"] for e in demo.generate(7)["events"]}
        self.assertTrue({"fail", "expired", "cancel", "block", "fenced", "claim"} <= kinds)

    def test_the_overlay_is_deterministic(self):
        a = demo.DemoSource(reveal_every=0).leases(5000)
        b = demo.DemoSource(reveal_every=0).leases(5000)
        self.assertEqual(a, b)

    def test_every_task_has_a_lease_row(self):
        src = demo.DemoSource(reveal_every=0)
        tasks, _ = src.tasks(5000)
        leases, available = src.leases(5000)
        self.assertTrue(available)
        self.assertEqual(sorted(t["id"] for t in tasks), sorted(r["task_id"] for r in leases))

    def test_a_dead_task_has_no_owner_and_no_live_lease(self):
        src = demo.DemoSource(reveal_every=0)
        tasks, _ = src.tasks(5000)
        leases = {r["task_id"]: r for r in src.leases(5000)[0]}
        for t in tasks:
            if t["state"] in ("dead", "cancelled", "open"):
                self.assertIsNone(t["owner"], t["id"])
                self.assertEqual(leases[t["id"]]["lease_until"], 0, t["id"])


if __name__ == "__main__":
    unittest.main()
