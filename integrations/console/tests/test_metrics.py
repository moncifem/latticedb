import unittest

import helpers  # noqa: F401  (sets sys.path)
from helpers import event, task

from console import metrics, model


def facts_for(tasks, events=(), decisions=()):
    return model.task_facts(tasks, sorted(events, key=lambda e: e["id"]), list(decisions))


class WilsonTests(unittest.TestCase):
    def test_one_in_ten_matches_the_hand_value(self):
        lo, hi = metrics.wilson(1, 10)
        self.assertAlmostEqual(lo, 0.018, delta=0.0006)
        self.assertAlmostEqual(hi, 0.404, delta=0.0006)

    def test_zero_of_ten(self):
        # Closed forms for k = 0: lower 0, upper z^2 / (n + z^2).
        lo, hi = metrics.wilson(0, 10)
        z2 = 1.959964 ** 2
        self.assertAlmostEqual(lo, 0.0, places=12)
        self.assertAlmostEqual(hi, z2 / (10 + z2), places=9)
        self.assertGreater(hi, 0.27)  # Wald would say 0 width here

    def test_ten_of_ten(self):
        # Closed forms for k = n: lower n / (n + z^2), upper 1.
        lo, hi = metrics.wilson(10, 10)
        z2 = 1.959964 ** 2
        self.assertAlmostEqual(lo, 10 / (10 + z2), places=9)
        self.assertAlmostEqual(hi, 1.0, places=12)

    def test_empty_sample_has_no_interval(self):
        self.assertIsNone(metrics.wilson(0, 0))
        self.assertIsNone(metrics.wilson(3, -1))

    def test_k_is_clamped_into_range(self):
        self.assertEqual(metrics.wilson(15, 10), metrics.wilson(10, 10))
        self.assertEqual(metrics.wilson(-4, 10), metrics.wilson(0, 10))

    def test_half_is_symmetric_around_one_half(self):
        lo, hi = metrics.wilson(50, 100)
        self.assertAlmostEqual(lo + hi, 1.0, places=9)
        self.assertAlmostEqual(lo, 0.404, delta=0.001)  # published value for 50 of 100

    def test_interval_narrows_with_n(self):
        w10 = metrics.wilson(5, 10)
        w1000 = metrics.wilson(500, 1000)
        self.assertGreater(w10[1] - w10[0], w1000[1] - w1000[0])


class SummaryTests(unittest.TestCase):
    def test_median_even_and_odd(self):
        self.assertEqual(metrics.median([4, 1, 3, 2]), 2.5)
        self.assertEqual(metrics.median([9, 1, 5]), 5.0)
        self.assertEqual(metrics.median([7]), 7.0)

    def test_empty_inputs_give_none(self):
        self.assertIsNone(metrics.median([]))
        self.assertIsNone(metrics.percentile([], 0.9))
        self.assertIsNone(metrics.mean([]))

    def test_percentiles_interpolate(self):
        self.assertEqual(metrics.percentile(list(range(1, 12)), 0.9), 10.0)  # 1..11, rank 9
        self.assertEqual(metrics.percentile([0, 10], 0.1), 1.0)
        self.assertEqual(metrics.percentile([0, 10], 1.0), 10.0)
        self.assertEqual(metrics.percentile([0, 10], 0.0), 0.0)

    def test_mean(self):
        self.assertEqual(metrics.mean([1, 2, 3, 6]), 3.0)


class BootstrapTests(unittest.TestCase):
    def test_is_deterministic_for_a_seed(self):
        data = [float(x) for x in range(1, 31)]
        self.assertEqual(metrics.bootstrap_ci(data), metrics.bootstrap_ci(data))
        self.assertNotEqual(metrics.bootstrap_ci(data, seed=1), metrics.bootstrap_ci(data, seed=2))

    def test_constant_data_has_a_point_interval(self):
        self.assertEqual(metrics.bootstrap_ci([5.0] * 20), (5.0, 5.0))

    def test_interval_brackets_the_median_of_a_spread_sample(self):
        data = [float(x) for x in range(1, 41)]  # median 20.5
        lo, hi = metrics.bootstrap_ci(data)
        self.assertLess(lo, 20.5)
        self.assertGreater(hi, 20.5)
        self.assertGreaterEqual(lo, 1.0)
        self.assertLessEqual(hi, 40.0)

    def test_too_few_values(self):
        self.assertIsNone(metrics.bootstrap_ci([]))
        self.assertIsNone(metrics.bootstrap_ci([3.0]))

    def test_diff_of_two_constant_samples(self):
        d = metrics.bootstrap_diff([10.0] * 40, [4.0] * 40)
        self.assertEqual(d, (6.0, 6.0, 6.0))

    def test_diff_needs_two_values_each(self):
        self.assertIsNone(metrics.bootstrap_diff([1.0], [1.0, 2.0]))
        self.assertIsNone(metrics.bootstrap_diff([1.0, 2.0], []))

    def test_diff_interval_includes_zero_for_identical_noise(self):
        a = [float(x % 10) for x in range(60)]
        b = [float((x + 3) % 10) for x in range(60)]
        delta, lo, hi = metrics.bootstrap_diff(a, b, metrics.mean)
        self.assertLessEqual(lo, 0.0)
        self.assertGreaterEqual(hi, 0.0)
        self.assertAlmostEqual(delta, 0.0, places=9)


class BuilderTests(unittest.TestCase):
    def test_proportion_below_minimum_shows_no_number(self):
        m = metrics.proportion_metric("x", "X", "def", 3, 9, 10)
        self.assertEqual(m["status"], "insufficient")
        self.assertEqual(m["n"], 9)
        self.assertEqual(m["message"], "not enough data (n < 10)")
        for key in ("value", "lo", "hi", "k"):
            self.assertNotIn(key, m)

    def test_proportion_at_minimum_has_interval(self):
        m = metrics.proportion_metric("x", "X", "def", 1, 10, 10)
        self.assertEqual(m["status"], "ok")
        self.assertAlmostEqual(m["value"], 0.1)
        self.assertAlmostEqual(m["lo"], 0.018, delta=0.0006)
        self.assertAlmostEqual(m["hi"], 0.404, delta=0.0006)
        self.assertEqual(m["interval"], "Wilson 95%")

    def test_empty_proportion_is_insufficient_even_with_min_zero(self):
        m = metrics.proportion_metric("x", "X", "def", 0, 0, 0)
        self.assertEqual(m["status"], "insufficient")

    def test_distribution_below_minimum(self):
        m = metrics.distribution_metric("t", "T", "d", [1.0, 2.0], 15, "s")
        self.assertEqual(m["status"], "insufficient")
        self.assertEqual(m["n"], 2)
        self.assertNotIn("p50", m)

    def test_distribution_values(self):
        vals = [float(v) for v in range(1, 22)]  # 1..21: median 11, p10 3, p90 19
        m = metrics.distribution_metric("t", "T", "d", vals, 15, "s")
        self.assertEqual(m["status"], "ok")
        self.assertEqual(m["p50"], 11.0)
        self.assertEqual(m["p10"], 3.0)
        self.assertEqual(m["p90"], 19.0)
        self.assertLess(m["lo"], 11.0)
        self.assertGreater(m["hi"], 11.0)

    def test_distribution_empty(self):
        m = metrics.distribution_metric("t", "T", "d", [], 1, "s")
        self.assertEqual(m["status"], "insufficient")
        self.assertEqual(m["n"], 0)


def by_id(card):
    return {m["id"]: m for m in card["metrics"]}


class ScorecardTests(unittest.TestCase):
    def build_cohort(self, n_top=20, n_done=5):
        tasks, events = [], []
        eid = 0
        for i in range(n_top):
            tid = "t%02d" % i
            created = 1000 + i * 10
            state = "done" if i < n_done else "open"
            tasks.append(task(tid, state, "ann" if i < n_done else None, created=created))
            eid += 1
            events.append(event(eid, tid, "maint", "add", created))
            if i < n_done:
                eid += 1
                events.append(event(eid, tid, "ann", "claim", created + 100))  # claim after 100 s
                eid += 1
                events.append(event(eid, tid, "ann", "done", created + 100 + 50 * (i + 1)))
        return tasks, events

    def test_completion_rate_with_wilson_interval(self):
        tasks, events = self.build_cohort(20, 5)
        card = metrics.compute_scorecard(facts_for(tasks, events), 10**6, 1800)
        m = by_id(card)["completion_rate"]
        self.assertEqual((m["status"], m["n"], m["k"]), ("ok", 20, 5))
        self.assertAlmostEqual(m["value"], 0.25)
        self.assertEqual((m["lo"], m["hi"]), metrics.wilson(5, 20))

    def test_nineteen_top_level_tasks_is_not_enough(self):
        tasks, events = self.build_cohort(19, 5)
        m = by_id(metrics.compute_scorecard(facts_for(tasks, events), 10**6, 1800))["completion_rate"]
        self.assertEqual(m["status"], "insufficient")
        self.assertEqual(m["n"], 19)
        self.assertNotIn("value", m)

    def test_children_are_not_in_the_completion_cohort(self):
        tasks, events = self.build_cohort(20, 5)
        tasks.append(task("kid", "done", "ann", parent="t00", created=1500))
        m = by_id(metrics.compute_scorecard(facts_for(tasks, events), 10**6, 1800))["completion_rate"]
        self.assertEqual(m["n"], 20)

    def test_time_to_claim_and_done_hand_values(self):
        tasks, events = self.build_cohort(20, 16)
        card = by_id(metrics.compute_scorecard(facts_for(tasks, events), 10**6, 1800))
        ttc = card["time_to_claim"]
        self.assertEqual((ttc["n"], ttc["p50"]), (16, 100.0))  # every claim came 100 s after creation
        self.assertEqual(ttc["extra"]["unclaimed_open"], 4)
        ttd = card["time_to_done"]
        # durations are 50, 100, ..., 800: median is the mean of 400 and 450
        self.assertEqual(ttd["n"], 16)
        self.assertEqual(ttd["p50"], 425.0)

    def test_negative_durations_are_excluded_and_counted(self):
        tasks = [task("a", "claimed", "x", created=500)]
        events = [event(1, "a", "x", "claim", 400)]  # claim stamped before the task existed
        facts = facts_for(tasks, events)
        vals, skipped = metrics.time_to_claim_values(facts)
        self.assertEqual((vals, skipped), ([], 1))

    def test_stale_claim_rate_uses_the_threshold(self):
        tasks = [task("s%d" % i, "claimed", "x", created=0, updated=0) for i in range(5)]
        tasks += [task("d%d" % i, "done", "x", created=0, updated=0) for i in range(15)]
        facts = facts_for(tasks)
        at = lambda now, stale_s: by_id(metrics.compute_scorecard(facts, now, stale_s))["stale_claim_rate"]
        m = at(5000, 1800)
        self.assertEqual((m["k"], m["n"]), (5, 20))
        self.assertEqual(at(1800, 1800)["k"], 0)  # exactly at the threshold is not stale
        self.assertEqual(at(1801, 1800)["k"], 5)

    def test_claim_contention_counts_two_authors(self):
        tasks = [task("c%02d" % i, "claimed", "a", created=0) for i in range(20)]
        events = [event(i + 1, "c%02d" % i, "a", "claim", 10) for i in range(20)]
        events.append(event(100, "c00", "b", "claim", 20))
        events.append(event(101, "c01", "a", "claim", 20))  # same author twice: not contention
        m = by_id(metrics.compute_scorecard(facts_for(tasks, events), 100, 1800))["claim_contention"]
        self.assertEqual((m["k"], m["n"]), (1, 20))
        self.assertIn("Lower bound", m["caveat"])

    def test_handoffs_only_among_done_claimed_tasks(self):
        tasks = [task("h%02d" % i, "done", "a", created=0) for i in range(20)]
        events = [event(i + 1, "h%02d" % i, "a", "claim", 5) for i in range(20)]
        events.append(event(50, "h03", "b", "claim", 9))
        m = by_id(metrics.compute_scorecard(facts_for(tasks, events), 100, 1800))["handoff_share"]
        self.assertEqual((m["k"], m["n"]), (1, 20))

    def test_decisions_and_notes_counts(self):
        tasks = [task("n%02d" % i, "done", "a", created=0) for i in range(30)]
        events = [event(i + 1, "n%02d" % i, "a", "note", 5) for i in range(30)]
        decisions = [{"id": 900 + i, "task_id": "n%02d" % i, "author": "a", "body": "x",
                      "created_at": 6} for i in range(10)]
        card = by_id(metrics.compute_scorecard(facts_for(tasks, events, decisions), 100, 1800))
        self.assertEqual(card["decisions_per_done"]["n"], 30)
        self.assertEqual(card["decisions_per_done"]["p50"], 0.0)  # only 10 of 30 have one
        self.assertEqual(card["notes_per_task"]["p50"], 1.0)

    def test_agent_scope_attributes_by_first_claimant(self):
        tasks = [task("a%d" % i, "done", "ann", created=0) for i in range(3)]
        tasks += [task("b%d" % i, "done", "bob", created=0) for i in range(2)]
        events = [event(i + 1, t["id"], t["owner"], "claim", 5) for i, t in enumerate(tasks)]
        facts = facts_for(tasks, events)
        self.assertEqual(metrics.compute_scorecard(facts, 100, 1800, agent="ann")["tasks_in_cohort"], 3)
        self.assertEqual(metrics.compute_scorecard(facts, 100, 1800, agent="bob")["tasks_in_cohort"], 2)
        self.assertEqual(metrics.compute_scorecard(facts, 100, 1800)["tasks_in_cohort"], 5)

    def test_window_selects_by_creation_time_half_open(self):
        tasks = [task("w%d" % i, "open", created=100 * i) for i in range(10)]
        facts = facts_for(tasks)
        card = metrics.compute_scorecard(facts, 5000, 1800, start=200, end=500)
        self.assertEqual(card["tasks_in_cohort"], 3)  # 200, 300, 400
        self.assertEqual(card["window"], {"from": 200, "to": 500})

    def test_open_tasks_are_named_in_the_completion_warning(self):
        tasks, events = self.build_cohort(20, 5)
        card = metrics.compute_scorecard(facts_for(tasks, events), 10**6, 1800)
        self.assertIn("15 are still open", by_id(card)["completion_rate"]["warning"])

    def test_running_claims_are_compared_by_time_since_first_claim(self):
        # 5 done tasks took 50..250 s from claim to done (median 150 s).
        tasks, events = self.build_cohort(20, 5)
        now = 100000
        for i in range(3):
            tid = "run%d" % i
            tasks.append(task(tid, "claimed", "bob", created=10))  # created long ago ...
            events.append(event(500 + i, tid, "bob", "claim", now - 100))  # ... claimed 100 s ago
        tasks.append(task("slow", "claimed", "bob", created=10))
        events.append(event(600, "slow", "bob", "claim", now - 5000))  # running 5000 s: not "young"
        w = by_id(metrics.compute_scorecard(facts_for(tasks, events), now, 10**9))["completion_rate"]["warning"]
        self.assertIn("3 claimed tasks have run for less than the median claim-to-done time (150 s)", w)

    def test_age_since_creation_does_not_trigger_the_running_warning(self):
        # created a moment ago but claimed long ago must not count as running briefly, and vice versa
        tasks, events = self.build_cohort(20, 5)
        tasks.append(task("late", "claimed", "bob", created=99990))
        events.append(event(500, "late", "bob", "claim", 99995))
        w = by_id(metrics.compute_scorecard(facts_for(tasks, events), 100000, 10**9))["completion_rate"]["warning"]
        self.assertIn("1 claimed tasks have run", w)  # 5 s since first claim < 150 s

    def test_truncated_events_add_a_caveat_to_the_event_metrics_only(self):
        tasks, events = self.build_cohort(20, 5)
        card = by_id(metrics.compute_scorecard(facts_for(tasks, events), 10**6, 1800, truncated_events=True))
        for mid in ("time_to_claim", "time_to_done", "claim_contention", "handoff_share",
                    "notes_per_task", "completion_rate"):
            self.assertIn("newest window", card[mid]["caveat"], mid)
            self.assertTrue(card[mid]["truncated"], mid)
        for mid in ("stale_claim_rate", "decisions_per_done"):
            self.assertNotIn("newest window", card[mid]["caveat"], mid)
            self.assertNotIn("truncated", card[mid])

    def test_truncated_decisions_add_a_caveat_to_the_decision_metric(self):
        card = by_id(metrics.compute_scorecard([], 100, 1800, truncated_decisions=True))
        self.assertIn("decisions are only the newest window", card["decisions_per_done"]["caveat"])
        self.assertNotIn("truncated", card["time_to_claim"])

    def test_no_flags_no_truncation_text(self):
        card = by_id(metrics.compute_scorecard([], 100, 1800))
        for m in card.values():
            self.assertNotIn("newest window", m["caveat"])

    def test_compare_carries_the_truncation_warning(self):
        out = metrics.compare_windows([], (0, 1), (1, 2), None, True)
        self.assertIn("newest window", out["warning"])
        self.assertIsNone(metrics.compare_windows([], (0, 1), (1, 2))["warning"])

    def test_empty_board_everything_insufficient(self):
        card = metrics.compute_scorecard([], 100, 1800)
        self.assertEqual(card["tasks_in_cohort"], 0)
        for m in card["metrics"]:
            self.assertEqual(m["status"], "insufficient", m["id"])
            self.assertEqual(m["n"], 0)

    def test_every_metric_states_its_definition_and_minimum(self):
        card = metrics.compute_scorecard([], 100, 1800)
        ids = [m["id"] for m in card["metrics"]]
        self.assertEqual(len(ids), len(set(ids)))
        for m in card["metrics"]:
            self.assertTrue(m["definition"])
            self.assertGreaterEqual(m["min_n"], 15)


class NotMeasurableTests(unittest.TestCase):
    def test_lists_rework_and_spot_check_with_the_schema_change(self):
        by = {m["id"]: m for m in metrics.NOT_MEASURABLE}
        self.assertEqual(by["rework_rate"]["message"], "not measurable: no reopen event")
        self.assertIn("'reopen'", by["rework_rate"]["schema_change"])
        self.assertIn("'verify'", by["spot_check_agreement"]["schema_change"])
        for m in metrics.NOT_MEASURABLE:
            self.assertEqual(m["status"], "not_measurable")

    def test_not_in_the_computed_metrics(self):
        ids = {m["id"] for m in metrics.compute_scorecard([], 1, 1800)["metrics"]}
        self.assertFalse(ids & {"rework_rate", "spot_check_agreement"})


class CompareTests(unittest.TestCase):
    def cohort(self, start, n, claim_after):
        tasks, events = [], []
        for i in range(n):
            tid = "w%d-%d" % (start, i)
            created = start + i
            tasks.append(task(tid, "done", "a", created=created))
            events.append(event(len(events) + 1 + start, tid, "a", "claim", created + claim_after))
            events.append(event(len(events) + 1 + start, tid, "a", "done", created + claim_after + 60))
        return tasks, events

    def test_insufficient_below_thirty_per_window(self):
        ta, ea = self.cohort(0, 10, 100)
        tb, eb = self.cohort(10000, 10, 200)
        facts = facts_for(ta + tb, ea + eb)
        out = metrics.compare_windows(facts, (0, 5000), (10000, 15000))
        for row in out["rows"]:
            self.assertEqual(row["status"], "insufficient")
            self.assertIn("n < 30", row["message"])
        self.assertIn("different tasks", out["method"])

    def test_shift_in_time_to_claim_is_detected(self):
        ta, ea = self.cohort(0, 40, 100)
        tb, eb = self.cohort(10000, 40, 200)
        facts = facts_for(ta + tb, ea + eb)
        out = metrics.compare_windows(facts, (0, 5000), (10000, 15000))
        row = {r["id"]: r for r in out["rows"]}["time_to_claim"]
        self.assertEqual(row["status"], "ok")
        self.assertEqual((row["delta"], row["lo"], row["hi"]), (-100.0, -100.0, -100.0))
        self.assertTrue(row["excludes_zero"])
        td = {r["id"]: r for r in out["rows"]}["time_to_done"]
        self.assertEqual(td["delta"], 0.0)
        self.assertFalse(td["excludes_zero"])
        self.assertEqual(out["window_a"]["tasks"], 40)

    def test_completion_rate_difference(self):
        ta, ea = self.cohort(0, 40, 100)  # all done
        tb = [task("o%d" % i, "open", created=10000 + i) for i in range(40)]  # none done
        out = metrics.compare_windows(facts_for(ta + tb, ea), (0, 5000), (10000, 15000))
        row = {r["id"]: r for r in out["rows"]}["completion_rate"]
        self.assertEqual(row["status"], "ok")
        self.assertEqual(row["delta"], 1.0)
        self.assertTrue(row["excludes_zero"])


if __name__ == "__main__":
    unittest.main()
