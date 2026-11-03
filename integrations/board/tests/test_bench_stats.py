"""The statistics of board bench against values worked out by hand.

Every expected number below was derived on paper from the definition (the arithmetic is in the comments),
not read off the code under test. test_bench_mutations.py plants an off-by-one, a missing interval and a
miscounted error in a copy of the code and checks that these tests notice.

Run: python3 -m unittest discover -s integrations/board/tests
"""

import math
import unittest

import benchbase
from benchbase import att, data_of
from benchlib import report, stats


class WilsonTests(unittest.TestCase):
    def test_one_in_ten(self):
        # z = 1.959964, z^2 = 3.841459, p = .1, n = 10. denom = 1 + z^2/10 = 1.384146.
        # centre = (.1 + z^2/20) / denom = .292073 / 1.384146 = .211010.
        # half = z * sqrt(.1 * .9 / 10 + z^2 / 400) / denom = 1.959964 * sqrt(.018604) / 1.384146 = .193144.
        lo, hi = stats.wilson(1, 10)
        self.assertAlmostEqual(lo, 0.017866, places=3)
        self.assertAlmostEqual(hi, 0.404154, places=3)

    def test_nothing_in_five_has_width_and_a_zero_floor(self):
        # p = 0: lower = 0, upper = z^2 / (n + z^2) = 3.841459 / 8.841459 = .434483.
        lo, hi = stats.wilson(0, 5)
        self.assertAlmostEqual(lo, 0.0, places=9)
        self.assertAlmostEqual(hi, 0.434483, places=5)

    def test_everything_in_five_has_width_and_a_one_ceiling(self):
        # p = 1: upper = 1, lower = n / (n + z^2) = 5 / 8.841459 = .565517.
        lo, hi = stats.wilson(5, 5)
        self.assertAlmostEqual(lo, 0.565517, places=5)
        self.assertAlmostEqual(hi, 1.0, places=9)

    def test_half_of_a_hundred(self):
        # centre .5, half = 1.959964 * sqrt(.0025 + 3.841459 / 40000) / 1.0384146 = .096052.
        lo, hi = stats.wilson(50, 100)
        self.assertAlmostEqual(lo, 0.40395, places=3)
        self.assertAlmostEqual(hi, 0.59605, places=3)

    def test_no_trials_has_no_interval(self):
        self.assertIsNone(stats.wilson(0, 0))

    def test_fractional_successes_are_allowed(self):
        # 4/3 "successes" in 3 tasks: p = .444444, denom = 1 + z^2/3 = 2.280486,
        # centre = (.444444 + z^2/6) / denom = .475642, half = .373647 (see test_report_by_hand).
        lo, hi = stats.wilson(4.0 / 3.0, 3)
        self.assertAlmostEqual(lo, 0.101995, places=3)
        self.assertAlmostEqual(hi, 0.849289, places=3)


class PassAtKTests(unittest.TestCase):
    def test_values_by_hand(self):
        # pass@k = 1 - C(n-c, k) / C(n, k)
        cases = [
            (5, 2, 3, 1 - 1 / 10),       # C(3,3) = 1, C(5,3) = 10
            (5, 2, 1, 2 / 5),            # k = 1 is c / n
            (4, 3, 2, 1.0),              # C(1,2) = 0
            (3, 1, 2, 1 - 1 / 3),        # C(2,2) = 1, C(3,2) = 3
            (3, 0, 2, 0.0),              # C(3,2) / C(3,2) = 1
            (10, 3, 5, 1 - 21 / 252),    # C(7,5) = 21, C(10,5) = 252
            (3, 3, 3, 1.0),
            (1, 0, 1, 0.0),
        ]
        for n, c, k, want in cases:
            self.assertAlmostEqual(stats.pass_at_k(n, c, k), want, places=12, msg=(n, c, k))

    def test_undefined_when_fewer_attempts_than_k(self):
        self.assertIsNone(stats.pass_at_k(2, 1, 3))
        self.assertIsNone(stats.pass_pow_k(2, 1, 3))

    def test_pass_at_k_never_decreases_with_k(self):
        vals = [stats.pass_at_k(6, 2, k) for k in range(1, 7)]
        self.assertEqual(vals, sorted(vals))
        self.assertEqual(vals[-1], 1.0)  # with k = n and c >= 1 some drawn attempt passes


class PassPowKTests(unittest.TestCase):
    def test_values_by_hand(self):
        # pass^k = C(c, k) / C(n, k)
        cases = [
            (5, 2, 3, 0.0),              # C(2,3) = 0
            (4, 3, 2, 3 / 6),            # C(3,2) = 3, C(4,2) = 6
            (3, 2, 2, 1 / 3),            # C(2,2) = 1, C(3,2) = 3
            (10, 8, 3, 56 / 120),        # C(8,3) = 56, C(10,3) = 120
            (3, 3, 3, 1.0),
            (5, 2, 1, 2 / 5),
        ]
        for n, c, k, want in cases:
            self.assertAlmostEqual(stats.pass_pow_k(n, c, k), want, places=12, msg=(n, c, k))

    def test_never_above_pass_at_k(self):
        for n in range(1, 7):
            for c in range(n + 1):
                for k in range(1, n + 1):
                    self.assertLessEqual(stats.pass_pow_k(n, c, k), stats.pass_at_k(n, c, k) + 1e-12)


class McNemarTests(unittest.TestCase):
    def test_exact_p_by_hand(self):
        # two-sided p = min(1, 2 * P(X >= max(b, c))), X ~ Binomial(b + c, 1/2)
        cases = [
            (5, 0, 2 / 32),              # P(X >= 5 | 5) = 1/32
            (8, 2, 112 / 1024),          # (C(10,8) + C(10,9) + C(10,10)) = 45 + 10 + 1 = 56, 2 * 56 / 1024
            (2, 8, 112 / 1024),          # symmetric
            (9, 1, 22 / 1024),           # (10 + 1) / 1024 * 2
            (7, 0, 2 / 128),
            (6, 0, 2 / 64),
            (1, 0, 1.0),                 # 2 * 1/2 = 1
            (3, 3, 1.0),                 # 2 * 42/64 > 1
            (0, 0, 1.0),                 # nothing to compare
            (2, 0, 0.5),                 # 2 * 1/4
        ]
        for b, c, want in cases:
            self.assertAlmostEqual(stats.mcnemar_exact(b, c), want, places=12, msg=(b, c))

    def test_negative_counts_are_refused(self):
        with self.assertRaises(ValueError):
            stats.mcnemar_exact(-1, 2)

    def test_bonferroni(self):
        self.assertAlmostEqual(stats.bonferroni(0.02, 3), 0.06)
        self.assertEqual(stats.bonferroni(0.5, 4), 1.0)
        self.assertIsNone(stats.bonferroni(None, 3))


class BootstrapTests(unittest.TestCase):
    def test_percentile_rule(self):
        vals = list(range(1, 1001))
        self.assertEqual(stats.percentile_of_sorted(vals, 0.025), 26)   # index floor(.025 * 1000) = 25
        self.assertEqual(stats.percentile_of_sorted(vals, 0.975), 976)  # index 975
        self.assertEqual(stats.percentile_of_sorted(vals, 1.0), 1000)   # clamped to the last

    def test_same_seed_same_answer_other_seed_other_answer(self):
        data = [0.0, 1.0, 1.0, 0.5, 0.0, 1.0, 0.25, 0.75]
        a = stats.bootstrap_mean_ci(data, seed=5, rounds=40)
        self.assertEqual(a, stats.bootstrap_mean_ci(data, seed=5, rounds=40))
        self.assertNotEqual(a, stats.bootstrap_mean_ci(data, seed=6, rounds=40))

    def test_constant_data_has_a_zero_width_interval(self):
        self.assertEqual(stats.bootstrap_mean_ci([0.5] * 6), (0.5, 0.5, 0.5))

    def test_one_value_has_no_interval(self):
        self.assertIsNone(stats.bootstrap_mean_ci([1.0]))

    def test_two_tasks_one_pass_one_fail(self):
        # Resampling [0, 1] twice gives mean 0, .5 or 1 with chances 1/4, 1/2, 1/4; both tails hold more
        # than 2.5 percent, so the interval is the whole of [0, 1].
        mean, lo, hi = stats.bootstrap_mean_ci([0.0, 1.0])
        self.assertEqual((mean, lo, hi), (0.5, 0.0, 1.0))

    def test_paired_difference_by_hand(self):
        # d = b - a = [0, 1, 0, 1], mean .5. Four draws from {0, 1}: mean 0 and mean 1 each have chance
        # 1/16 = 6.25 percent, above 2.5, so the 2.5 and 97.5 percentiles are 0 and 1.
        self.assertEqual(stats.paired_bootstrap_diff([1, 0, 1, 0], [1, 1, 1, 1]), (0.5, 0.0, 1.0))

    def test_pairing_beats_two_separate_intervals(self):
        # Hard tasks stay hard in both arms; the gain is exactly .5 on every task. Paired, the interval is
        # a point. Compared as two independent intervals the same data look unconvincing.
        a = [0.0, 0.5, 0.0, 0.5, 0.0, 0.5, 0.0, 0.5]
        b = [x + 0.5 for x in a]
        mean, lo, hi = stats.paired_bootstrap_diff(a, b)
        self.assertEqual((mean, lo, hi), (0.5, 0.5, 0.5))
        wa, wb = stats.task_level_wilson(a), stats.task_level_wilson(b)
        self.assertLess(wa[0], wb[1])  # the separate intervals overlap

    def test_paired_samples_must_have_one_length(self):
        with self.assertRaises(ValueError):
            stats.paired_bootstrap_diff([1.0], [1.0, 0.0])


class ClusterTests(unittest.TestCase):
    def test_task_level_interval_is_wider_than_counting_attempts(self):
        # Ten tasks, five attempts each; five tasks always pass and five never do. Counting the 50 attempts
        # as independent gives Wilson(25, 50) = (.4040, .5960). Counting tasks gives Wilson(5, 10):
        # centre .5, half = 1.959964 * sqrt(.025 + z^2 / 400) / 1.384146 = .263416, so (.2366, .7634).
        naive = stats.wilson(25, 50)
        tasks = stats.task_level_wilson([1.0] * 5 + [0.0] * 5)
        self.assertAlmostEqual(tasks[0], 0.2366, places=3)
        self.assertAlmostEqual(tasks[1], 0.7634, places=3)
        self.assertLess(tasks[0], naive[0])
        self.assertGreater(tasks[1], naive[1])

    def test_mean_over_tasks(self):
        self.assertEqual(stats.mean_over_tasks([1.0, None, 0.5]), (0.75, 2, 1))
        self.assertEqual(stats.mean_over_tasks([]), (None, 0, 0))

    def test_power_note_says_what_it_is(self):
        text = stats.power_note(8, 3)
        self.assertIn("8 tasks", text)
        self.assertIn("3 repeats", text)
        self.assertIn("10-point", text)
        self.assertIn("inconclusive", text)


class ReportByHandTests(unittest.TestCase):
    """One arm, three tasks, k = 3: t1 3/3, t2 1/3, t3 two fails and one harness error."""

    def setUp(self):
        rows = [att("a", "t1", r) for r in range(3)]
        rows += [att("a", "t2", 0), att("a", "t2", 1, "fail"), att("a", "t2", 2, "fail")]
        rows += [att("a", "t3", 0, "fail"), att("a", "t3", 1, "fail"), att("a", "t3", 2, "error", summary="grader crashed")]
        self.rep = report.build_report(data_of(rows, repeats=3))
        self.arm = self.rep["arms"][0]

    def test_pass_at_1_is_the_mean_over_tasks(self):
        # (1 + 1/3 + 0) / 3 = 4/9; 8 counted attempts, 4 passed (the error is in no rate)
        self.assertAlmostEqual(self.arm["pass_at_1"]["value"], 4 / 9, places=12)
        self.assertEqual((self.arm["tasks"], self.arm["attempts"], self.arm["passes"]), (3, 8, 4))

    def test_interval_counts_tasks(self):
        lo, hi = self.arm["pass_at_1"]["ci"]
        self.assertAlmostEqual(lo, 0.101995, places=3)
        self.assertAlmostEqual(hi, 0.849289, places=3)
        self.assertIn("n = 3 tasks, 8 attempts", self.arm["pass_at_1"]["text"])

    def test_pass_at_k_and_pass_pow_k_skip_tasks_with_fewer_than_k_attempts(self):
        # t1: pass@3 = 1 - C(0,3)/C(3,3) = 1; t2: 1 - C(2,3)/1 = 1; t3 has n = 2 < 3, left out.
        self.assertEqual(self.arm["pass_at_k"]["value"], 1.0)
        self.assertEqual((self.arm["pass_at_k"]["n_tasks"], self.arm["pass_at_k"]["skipped"]), (2, 1))
        # pass^3: t1 = C(3,3)/C(3,3) = 1; t2 = C(1,3)/1 = 0; mean .5
        self.assertAlmostEqual(self.arm["pass_pow_k"]["value"], 0.5, places=12)

    def test_the_error_is_listed_and_in_no_rate(self):
        self.assertEqual(self.arm["errors"], 1)
        self.assertEqual([e["summary"] for e in self.rep["errors"]], ["grader crashed"])
        self.assertTrue(any("harness error" in n for n in self.rep["notes"]))

    def test_an_error_never_changes_a_rate(self):
        # one task: a pass and a harness error. The error is in no rate, so the rate is 1/1, not 1/2.
        arm = report.build_report(data_of([att("a", "t1", 0), att("a", "t1", 1, "error")]))["arms"][0]
        self.assertEqual((arm["attempts"], arm["errors"]), (1, 1))
        self.assertEqual(arm["pass_at_1"]["value"], 1.0)
        self.assertIn("n = 1 task, 1 attempt", arm["pass_at_1"]["text"])

    def test_pending_attempts_are_in_no_rate(self):
        rows = [att("a", "t1", 0), att("a", "t1", 1, "planned")]
        arm = report.build_report(data_of(rows))["arms"][0]
        self.assertEqual((arm["attempts"], arm["pending"]), (1, 1))
        self.assertEqual(arm["pass_at_1"]["value"], 1.0)

    def test_timeouts_count_as_failures(self):
        rows = [att("a", "t1", 0), att("a", "t1", 1, "timeout")]
        arm = report.build_report(data_of(rows))["arms"][0]
        self.assertEqual((arm["attempts"], arm["passes"], arm["timeouts"]), (2, 1, 1))
        self.assertAlmostEqual(arm["pass_at_1"]["value"], 0.5)

    def test_an_arm_with_no_counted_attempt_has_no_result_not_zero(self):
        arm = report.build_report(data_of([att("a", "t1", 0, "planned")]))["arms"][0]
        self.assertIsNone(arm["pass_at_1"]["value"])
        self.assertIn("no result", arm["pass_at_1"]["text"])


class ComparisonByHandTests(unittest.TestCase):
    """Two arms, four tasks, two repeats.
       t1 A 2/2 B 2/2 | t2 A 0/2 B 2/2 | t3 A 1/2 B 2/2 | t4 A 0/2 B 0/2"""

    def build(self, rule="majority", min_tasks=30):
        rows = []
        for task, (pa, pb) in {"t1": (2, 2), "t2": (0, 2), "t3": (1, 2), "t4": (0, 0)}.items():
            for r in range(2):
                rows.append(att("A", task, r, "pass" if r < pa else "fail"))
                rows.append(att("B", task, r, "pass" if r < pb else "fail"))
        return report.build_report(data_of(rows), rule, min_tasks)["comparisons"][0]

    def test_majority_rule_table_and_p(self):
        # A solves {t1} (t3 is a tie, not solved); B solves {t1, t2, t3}.
        c = self.build()
        self.assertEqual(c["table"], {"both": 1, "a_only": 0, "b_only": 2, "neither": 1})
        self.assertAlmostEqual(c["mcnemar_p"], 0.5)  # b + c = 2, 2 * P(X >= 2) = 2 * 1/4

    def test_any_rule_table_and_p(self):
        # A solves {t1, t3}; B solves {t1, t2, t3}: one discordant task, p = 2 * 1/2 = 1.
        c = self.build("any")
        self.assertEqual(c["table"], {"both": 2, "a_only": 0, "b_only": 1, "neither": 1})
        self.assertAlmostEqual(c["mcnemar_p"], 1.0)

    def test_difference_is_b_minus_a_over_paired_tasks(self):
        # rates A [1, 0, .5, 0], B [1, 1, 1, 0]: d = [0, 1, .5, 0], mean .375
        c = self.build()
        self.assertAlmostEqual(c["diff"]["mean"], 0.375)
        self.assertEqual(c["paired_tasks"], 4)

    def test_four_tasks_are_inconclusive_whatever_the_interval(self):
        c = self.build()
        self.assertEqual(c["verdict"]["label"], "inconclusive")
        self.assertIn("Fewer than the minimum of 30 paired tasks", c["verdict"]["text"])

    def test_with_a_lower_minimum_the_interval_decides(self):
        # d = [0, 1, .5, 0]; its 95 percent interval includes 0
        c = self.build(min_tasks=3)
        self.assertEqual(c["verdict"]["label"], "inconclusive")
        self.assertIn("includes zero", c["verdict"]["text"])

    def test_three_arms_make_three_comparisons_and_a_stricter_level(self):
        rows = [att(a, "t%d" % t, r, "pass") for a in "ABC" for t in range(3) for r in range(2)]
        rep = report.build_report(data_of(rows))
        self.assertEqual(len(rep["comparisons"]), 3)
        self.assertAlmostEqual(rep["comparisons"][0]["ci_level"], 1 - 0.05 / 3)
        self.assertAlmostEqual(stats.bonferroni(0.04, 3), 0.12)


class VerdictTests(unittest.TestCase):
    def cmp(self, n, lo, hi, mean=None, p_adj=0.001):
        mean = (lo + hi) / 2 if mean is None and lo is not None else mean
        return {"a": "A", "b": "B", "paired_tasks": n, "min_tasks": 30, "ci_level": 0.95, "comparisons": 1,
                "diff": None if lo is None else {"mean": mean, "lo": lo, "hi": hi}, "mcnemar_p_adjusted": p_adj}

    def test_clear_difference(self):
        v = report.verdict(self.cmp(40, 0.2, 0.5))
        self.assertEqual(v["label"], "b_better")
        self.assertIn("40 paired tasks", v["text"])
        self.assertIn("+20.0 points to +50.0 points", v["text"])

    def test_clear_difference_the_other_way(self):
        self.assertEqual(report.verdict(self.cmp(40, -0.5, -0.1))["label"], "a_better")

    def test_interval_including_zero_is_inconclusive(self):
        v = report.verdict(self.cmp(40, -0.1, 0.4))
        self.assertEqual(v["label"], "inconclusive")
        self.assertIn("includes zero", v["text"])

    def test_below_the_minimum_n_is_inconclusive_even_when_the_interval_excludes_zero(self):
        v = report.verdict(self.cmp(29, 0.2, 0.5))
        self.assertEqual(v["label"], "inconclusive")
        self.assertIn("29 paired tasks", v["text"])

    def test_exactly_the_minimum_counts(self):
        self.assertEqual(report.verdict(self.cmp(30, 0.2, 0.5))["label"], "b_better")

    def test_no_interval_is_inconclusive(self):
        self.assertEqual(report.verdict(self.cmp(1, None, None))["label"], "inconclusive")

    def test_a_disagreeing_exact_test_means_inconclusive(self):
        # the interval excludes zero but the adjusted exact p is .2: no winner may be named
        v = report.verdict(self.cmp(40, 0.05, 0.4, p_adj=0.2))
        self.assertEqual(v["label"], "inconclusive")
        self.assertIn("exact McNemar test", v["text"])
        self.assertIn("disagree", v["text"])

    def test_both_must_agree_for_b_better_and_for_a_better(self):
        self.assertEqual(report.verdict(self.cmp(40, 0.05, 0.4, p_adj=0.049))["label"], "b_better")
        self.assertEqual(report.verdict(self.cmp(40, 0.05, 0.4, p_adj=0.05))["label"], "inconclusive")
        self.assertEqual(report.verdict(self.cmp(40, -0.4, -0.05, p_adj=0.2))["label"], "inconclusive")
        self.assertEqual(report.verdict(self.cmp(40, -0.4, -0.05, p_adj=0.01))["label"], "a_better")

    def test_no_negative_zero(self):
        self.assertIn("+0.0 points", report._pts(-0.00001))


class SignTestGateTests(unittest.TestCase):
    """30 paired tasks, B solves k tasks that A does not, nothing else differs (repeats 1).
    The exact two-sided sign test has p = 2 * (1/2)^k: k = 4 gives 2/16 = .125, k = 5 gives 2/32 = .0625,
    k = 10 gives 2/1024 = .001953. The bootstrap interval alone excludes zero for all three (the chance of
    drawing no changed task in 30 draws is (1 - k/30)^30: .0137, .0042, 5e-6, all under 2.5 percent)."""

    def cmp_for(self, k):
        rows = []
        for t in range(30):
            rows.append(att("A", "t%02d" % t, 0, "fail" if t < k else "pass"))
            rows.append(att("B", "t%02d" % t, 0, "pass"))
        rep = report.build_report(data_of(rows, repeats=1))
        return rep["comparisons"][0]

    def test_four_changed_tasks_are_not_enough(self):
        c = self.cmp_for(4)
        self.assertAlmostEqual(c["mcnemar_p"], 0.125)
        self.assertGreater(c["diff"]["lo"], 0)            # the bootstrap alone would say "better"
        self.assertEqual(c["verdict"]["label"], "inconclusive")
        self.assertIn("does not reject", c["verdict"]["text"])

    def test_five_changed_tasks_can_never_reach_point_zero_five(self):
        c = self.cmp_for(5)
        self.assertAlmostEqual(c["mcnemar_p"], 0.0625)
        self.assertGreater(c["diff"]["lo"], 0)
        self.assertEqual(c["verdict"]["label"], "inconclusive")

    def test_ten_changed_tasks_pass_both(self):
        c = self.cmp_for(10)
        self.assertAlmostEqual(c["mcnemar_p"], 2 / 1024)
        self.assertGreater(c["diff"]["lo"], 0)
        self.assertEqual(c["verdict"]["label"], "b_better")
        self.assertIn("McNemar test agrees", c["verdict"]["text"])

    def test_the_console_never_gets_a_better_label_the_test_rejects(self):
        for k in (1, 2, 3, 4, 5):
            self.assertEqual(self.cmp_for(k)["verdict"]["label"], "inconclusive", k)

    def test_every_pair_identical_is_said_and_rests_on_the_exact_test(self):
        # A fails all 30 tasks, B passes all 30: every difference is +1, the bootstrap interval has no width
        rows = [att(a, "t%02d" % t, 0, "fail" if a == "A" else "pass") for a in "AB" for t in range(30)]
        c = report.build_report(data_of(rows, repeats=1))["comparisons"][0]
        self.assertTrue(c["degenerate"])
        self.assertAlmostEqual(c["mcnemar_p"], 2 / 2 ** 30)
        self.assertEqual(c["verdict"]["label"], "b_better")
        self.assertIn("no width", c["verdict"]["text"])
        # same difference of .5 on every task but no task flips under the majority rule: nothing to call
        rows = []
        for t in range(30):
            rows += [att("A", "t%02d" % t, 0, "fail"), att("A", "t%02d" % t, 1, "fail"),
                     att("B", "t%02d" % t, 0, "pass"), att("B", "t%02d" % t, 1, "fail")]
        c = report.build_report(data_of(rows))["comparisons"][0]
        self.assertTrue(c["degenerate"])
        self.assertEqual(c["table"]["b_only"], 0)
        self.assertEqual(c["verdict"]["label"], "inconclusive")


class WarningTests(unittest.TestCase):
    def test_different_error_counts_are_shown_and_warned_about(self):
        rows = [att(a, "t%d" % t, 0) for a in "AB" for t in range(3)]
        rows.append(att("B", "t9", 0, "error", summary="x"))
        rep = report.build_report(data_of(rows, repeats=1))
        self.assertTrue(any("Harness errors differ between arms (A: 0, B: 1)" in n for n in rep["notes"]))
        c = rep["comparisons"][0]
        self.assertEqual(c["errors"], {"A": 0, "B": 1})
        self.assertIn("Harness errors differ between the arms (A: 0, B: 1)", c["verdict"]["text"])
        b = [a for a in rep["arms"] if a["arm"] == "B"][0]
        self.assertIn("1 harness error excluded", b["pass_at_1"]["text"])

    def test_equal_error_counts_are_not_a_warning(self):
        rows = [att(a, "t%d" % t, 0) for a in "AB" for t in range(3)] + [att(a, "t9", 0, "error") for a in "AB"]
        rep = report.build_report(data_of(rows, repeats=1))
        self.assertFalse(any("differ" in n for n in rep["notes"]))
        self.assertNotIn("differ", rep["comparisons"][0]["verdict"]["text"])

    def test_a_task_counted_for_one_arm_only_is_reported_as_dropped_from_the_pairing(self):
        rows = [att(a, "t%d" % t, 0) for a in "AB" for t in range(3)] + [att("A", "t9", 0), att("B", "t9", 0, "error")]
        c = report.build_report(data_of(rows, repeats=1))["comparisons"][0]
        self.assertEqual((c["paired_tasks"], c["dropped_tasks"]), (3, 1))
        self.assertIn("1 task was counted for only one arm", c["verdict"]["text"])

    def test_the_bootstrap_gets_more_resamples_as_the_level_tightens_up_to_a_stated_cap(self):
        def rows_for(arms):
            return [att(a, "t%d" % t, 0, "pass" if t % 2 else "fail") for a in arms for t in range(3)]
        two = report.build_report(data_of(rows_for("AB"), repeats=1))["comparisons"][0]
        self.assertEqual((two["rounds"], two["rounds_capped"]), (2000, False))
        three = report.build_report(data_of(rows_for("ABC"), repeats=1))["comparisons"][0]
        self.assertEqual(three["rounds"], 2000)  # 20 / (.05 / 3) = 1200 < 2000
        eight = report.build_report(data_of(rows_for("ABCDEFGH"), repeats=1))["comparisons"][0]
        self.assertEqual((eight["rounds"], eight["rounds_capped"]), (10000, True))  # 20 / (.05 / 28) = 11200
        self.assertIn("approximate", eight["verdict"]["text"])


class RoundingTests(unittest.TestCase):
    def test_a_bound_below_one_never_prints_as_one_hundred(self):
        self.assertEqual(report.pct(0.9996), "99.96%")
        self.assertEqual(report.pct_lo(0.9996), "99.9%")      # floor(999.6) / 10
        self.assertEqual(report.pct_hi(0.9996), "99.96%")     # ceil to tenths would be 100.0, so two decimals
        self.assertEqual(report.pct_hi(0.99999), ">99.99%")
        self.assertEqual(report.pct(1.0), "100%")
        self.assertEqual(report.pct_hi(1.0), "100%")

    def test_lower_bounds_round_down_and_upper_bounds_up(self):
        self.assertEqual(report.pct_lo(0.2349), "23%")
        self.assertEqual(report.pct_hi(0.2301), "24%")
        self.assertEqual(report.pct_lo(0.05), "5.0%")         # 5.0 exactly stays 5.0 (float noise ignored)
        self.assertEqual(report.pct_hi(0.3), "30%")           # 30.000000000000004 must not become 31
        self.assertEqual(report.pct_lo(0.0), "0%")
        self.assertEqual(report.pct(0.00004), "<0.01%")

    def test_pass_at_k_and_pass_pow_k_carry_an_interval_too(self):
        rows = [att("A", "t%d" % t, r, "pass" if (t + r) % 3 else "fail") for t in range(6) for r in range(3)]
        arm = report.build_report(data_of(rows, repeats=3))["arms"][0]
        for key in ("pass_at_k", "pass_pow_k"):
            self.assertRegex(arm[key]["text"], r"\(95% CI .* to .*\) over 6 tasks")
            self.assertIsNotNone(arm[key]["ci"])
        one = report.build_report(data_of([att("A", "t0", r) for r in range(3)], repeats=3))["arms"][0]
        self.assertIn("no interval", one["pass_at_k"]["text"])


class NoBarePercentageTests(unittest.TestCase):
    """A rate is only ever produced together with its interval and its n."""

    def test_rate_text_always_has_interval_and_n(self):
        t = report.rate_text(0.5, (0.25, 0.75), 8, 24)
        self.assertEqual(t, "50% (95% CI 25% to 75%), n = 8 tasks, 24 attempts")
        self.assertIn("no interval", report.rate_text(0.5, None, 1, 1))
        self.assertIn("n = 0", report.rate_text(None, None, 0, 0))

    def test_every_rate_in_a_report_carries_ci_and_n(self):
        rows = [att(a, "t%d" % t, r, "pass" if (t + r) % 2 else "fail") for a in "AB" for t in range(4) for r in range(2)]
        rep = report.build_report(data_of(rows))
        for arm in rep["arms"]:
            self.assertRegex(arm["pass_at_1"]["text"], r"\(95% CI .* to .*\), n = \d+ task")
            self.assertRegex(arm["pass_at_k"]["text"], r"over \d+ task")
        text = report.render_text(rep)
        for line in text.splitlines():
            if "%" in line and "pass@1" in line:
                self.assertIn("CI", line)

    def test_the_text_report_states_the_rule_and_the_verdict(self):
        rows = [att(a, "t%d" % t, r) for a in "AB" for t in range(2) for r in range(2)]
        text = report.render_text(report.build_report(data_of(rows)))
        self.assertIn("VERDICT: Inconclusive", text)
        self.assertIn("more than half of its counted attempts passed", text)
        self.assertIn("too narrow", text)


class ParseRowsTests(unittest.TestCase):
    def line(self, *f):
        return "\t".join(str(x) for x in f)

    def test_round_trip_of_the_documented_format(self):
        text = "\n".join([
            self.line("experiment", "e1", "suite", "a" * 64, 7, 2, 4, "board-bench/1", "bob", 100),
            self.line("arm", "base", "cmd", "b" * 64),
            self.line("task", "t1", "c" * 64),
            self.line("attempt", "t001", "base", "t1", 0, 3, "pass", 1, 2, 1500, "0.0123", 900, 0, "pfp", "board-bench/1", 105, "grader runs pfp"),
            self.line("attempt", "t002", "base", "t1", 1, 4, "planned", 0, 0, "", "", "", "", "", "board-bench/1", 0, ""),
        ])
        d = report.parse_rows(text)
        self.assertEqual(d["experiment"]["seed"], 7)
        a, p = d["attempts"]
        self.assertEqual((a["outcome"], a["flaky"], a["tampered"], a["duration_ms"], a["cost_usd"], a["tokens"]),
                         ("pass", True, 2, 1500, 0.0123, 900))
        self.assertIsNone(p["duration_ms"])
        self.assertIsNone(p["cost_usd"])

    def test_a_malformed_line_is_an_error_not_a_guess(self):
        with self.assertRaises(report.RowsError):
            report.parse_rows(self.line("experiment", "e1"))
        with self.assertRaises(report.RowsError):
            report.parse_rows("nonsense\tx")
        with self.assertRaises(report.RowsError):
            report.parse_rows(self.line("arm", "a", "cmd", "x"))  # no experiment line


if __name__ == "__main__":
    unittest.main()
