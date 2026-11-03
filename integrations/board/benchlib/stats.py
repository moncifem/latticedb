"""The statistics of `board bench`: standard library only, no clock, no randomness except a seeded one.

Every formula here was derived again from its definition (the derivations are in the comments) and is
tested against values worked out by hand in tests/test_bench_stats.py. Nothing is copied from a library.

Vocabulary. A TASK is one suite task. An ATTEMPT is one run of one arm on one task (the suite's `repeats`
give several per task). For one arm and one task, n is the number of attempts that count (an infra error
does not count) and c the number of those that passed. Attempts of one task are NOT independent samples of
the arm: the same task is easy or hard each time. Every interval for an arm or for a difference between
two arms therefore treats the TASK as the unit that was sampled, never the attempt.
"""

from __future__ import annotations

import math
import random
from typing import Optional, Sequence

Z95 = 1.959964  # the two-sided 95 percent point of the standard normal distribution
BOOTSTRAP_ROUNDS = 2000
BOOTSTRAP_SEED = 20261103


def wilson(successes: float, n: float, z: float = Z95) -> Optional[tuple]:
    """Wilson score interval for a proportion.

    With p = successes / n, the interval is the set of p0 with |p - p0| <= z * sqrt(p0 (1 - p0) / n).
    Squaring and solving the quadratic in p0 gives
        (p + z^2 / 2n  +/-  z * sqrt(p (1 - p) / n + z^2 / 4n^2)) / (1 + z^2 / n).
    It stays inside [0, 1] and has a non-zero width at p = 0 and p = 1, which the Wald interval does not.
    `successes` may be fractional (the mean of per-task pass rates); n is then the number of tasks.
    None when n is 0."""
    if n <= 0:
        return None
    p = min(max(successes / n, 0.0), 1.0)
    z2 = z * z
    denom = 1.0 + z2 / n
    centre = (p + z2 / (2.0 * n)) / denom
    half = z * math.sqrt(max(p * (1.0 - p) / n + z2 / (4.0 * n * n), 0.0)) / denom
    return (max(0.0, centre - half), min(1.0, centre + half))


def pass_at_k(n: int, c: int, k: int) -> Optional[float]:
    """The probability that at least one of k attempts, drawn without replacement from the n attempts made
    of which c passed, passes: 1 - C(n - c, k) / C(n, k). It is an unbiased estimate of the chance that
    k fresh attempts contain a pass. Undefined (None) when n < k: fewer attempts than the draw.
    With k = 1 it is c / n."""
    if k < 1 or n < k or c < 0 or c > n:
        return None
    return 1.0 - math.comb(n - c, k) / math.comb(n, k)


def pass_pow_k(n: int, c: int, k: int) -> Optional[float]:
    """The probability that all k attempts drawn without replacement pass: C(c, k) / C(n, k) (zero when
    c < k, which math.comb gives). This is the "reliability" reading of tau-bench's pass^k: an agent that
    passes sometimes scores far lower than one that passes every time. None when n < k."""
    if k < 1 or n < k or c < 0 or c > n:
        return None
    return math.comb(c, k) / math.comb(n, k)


def mean_over_tasks(per_task: Sequence[Optional[float]]) -> tuple:
    """(mean of the defined values, how many tasks had one, how many did not)."""
    vals = [v for v in per_task if v is not None]
    return ((sum(vals) / len(vals)) if vals else None, len(vals), len(per_task) - len(vals))


def task_level_wilson(per_task_rates: Sequence[float], z: float = Z95) -> Optional[tuple]:
    """Interval for an arm's pass@1 that counts TASKS, not attempts: Wilson with n = the number of tasks
    and p = the mean of the per-task pass rates. Counting every attempt would pretend that five repeats
    of one task are five independent tasks. This is conservative: it ignores that repeats make each task's
    own rate less noisy, so the interval is wider than the truth when the repeats are informative and never
    narrower than the pooled-attempt Wilson interval for the same data."""
    n = len(per_task_rates)
    if n == 0:
        return None
    return wilson(sum(per_task_rates), n, z)


def binom_two_sided_half(b: int, c: int) -> float:
    """Two-sided exact p for b against c discordant pairs when each is equally likely: with m = b + c and
    X ~ Binomial(m, 1/2), p = min(1, 2 * P(X >= max(b, c))). m = 0 gives 1 (nothing to compare)."""
    m = b + c
    if m == 0:
        return 1.0
    hi = max(b, c)
    tail = sum(math.comb(m, i) for i in range(hi, m + 1)) / float(2 ** m)
    return min(1.0, 2.0 * tail)


def mcnemar_exact(a_only: int, b_only: int) -> float:
    """McNemar's exact test on a 2x2 table of paired outcomes. Only the discordant pairs carry information:
    a_only tasks solved by A and not B, b_only the reverse. Under "no difference" each discordant task is
    equally likely to go either way (a fair coin), so the p value is the two-sided binomial tail above."""
    if a_only < 0 or b_only < 0:
        raise ValueError("counts cannot be negative")
    return binom_two_sided_half(a_only, b_only)


def bonferroni(p: Optional[float], comparisons: int) -> Optional[float]:
    """p multiplied by the number of comparisons made, at most 1."""
    if p is None:
        return None
    return min(1.0, p * max(comparisons, 1))


def percentile_of_sorted(sorted_vals: Sequence[float], q: float) -> float:
    """The value at rank floor(q * B) of B sorted values (clamped to the last). This is the plain
    percentile bootstrap rule: with B = 1000 and q = 0.025 it returns the 26th smallest value."""
    idx = min(len(sorted_vals) - 1, max(0, int(q * len(sorted_vals))))
    return sorted_vals[idx]


def bootstrap_mean_ci(values: Sequence[float], alpha: float = 0.05, rounds: int = BOOTSTRAP_ROUNDS,
                      seed: int = BOOTSTRAP_SEED) -> Optional[tuple]:
    """Percentile bootstrap interval of the mean of `values`, resampling whole entries (one per task) with
    replacement. Deterministic: the generator is seeded and draws only random(), whose stream is the same
    on every Python 3 version. Returns (mean, low, high); None for fewer than two values (one task cannot
    show how much tasks differ)."""
    n = len(values)
    if n < 2:
        return None
    rng = random.Random(seed)
    rand = rng.random
    means = []
    for _ in range(rounds):
        total = 0.0
        for _i in range(n):
            total += values[int(rand() * n)]
        means.append(total / n)
    means.sort()
    return (sum(values) / n, percentile_of_sorted(means, alpha / 2.0),
            percentile_of_sorted(means, 1.0 - alpha / 2.0))


def paired_bootstrap_diff(rates_a: Sequence[float], rates_b: Sequence[float], alpha: float = 0.05,
                          rounds: int = BOOTSTRAP_ROUNDS, seed: int = BOOTSTRAP_SEED) -> Optional[tuple]:
    """B minus A in pass rate over the tasks both arms attempted: the per-task differences d_i = b_i - a_i
    are resampled as one list, so a task that is hard for both arms stays hard in both (that pairing is why
    the interval is tighter than two separate intervals). Returns (mean difference, low, high)."""
    if len(rates_a) != len(rates_b):
        raise ValueError("paired samples must have the same length")
    return bootstrap_mean_ci([b - a for a, b in zip(rates_a, rates_b)], alpha, rounds, seed)


def power_note(tasks: int, repeats: int) -> str:
    """The sentence `plan` prints. The rule of thumb is ours, not a measured property of any suite:
    with a few dozen tasks a gap of ten points is within the noise of which tasks were picked."""
    return ("With %d task%s and %d repeat%s, a 10-point difference between two arms is usually inconclusive: "
            "repeats of one task are not independent samples, so the number of tasks, not of attempts, "
            "limits what the experiment can show." % (tasks, "" if tasks == 1 else "s", repeats,
                                                     "" if repeats == 1 else "s"))
