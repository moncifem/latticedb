"""Statistics and the scorecard metrics, from task facts only (no database, no clock).

Every proportion carries a Wilson interval. Every metric states its sample size n and the
minimum n below which no number is shown. Definitions follow the research memo of 2026-11-02
(see README.md, "Scorecard"). Nothing here calls a model.
"""

from __future__ import annotations

import math
import random
from typing import Callable, Optional, Sequence

Z95 = 1.959964  # two-sided 95 percent normal quantile
BOOTSTRAP_ROUNDS = 1000
BOOTSTRAP_SEED = 20261102
BOOTSTRAP_MAX_VALUES = 5000  # resampling cost grows with n; a larger sample is cut (and said so)
COMPARE_MIN_N = 30  # per window; the memo advises against ranking on fewer paired tasks

# Metrics the schema cannot support, with the exact change that would make them measurable.
NOT_MEASURABLE = [
    {
        "id": "rework_rate",
        "name": "Rework / reopen rate",
        "status": "not_measurable",
        "message": "not measurable: no reopen event",
        "schema_change": (
            "Add event kind 'reopen' and make the board helper write one (with the reopening "
            "author and the reason) in the same transaction that moves a task out of 'done'. "
            "Today a done task can never leave 'done', and events has no state history."
        ),
    },
    {
        "id": "spot_check_agreement",
        "name": "Spot-check agreement",
        "status": "not_measurable",
        "message": "not measurable: no audit record",
        "schema_change": (
            "Add event kind 'verify' (author different from the task's claimant, body 'pass' or "
            "'fail' plus a pointer to the artifact checked). Without it nothing on the board says "
            "whether a 'done' task is true; every other metric measures how the board was used."
        ),
    },
]


def wilson(k: int, n: int, z: float = Z95) -> Optional[tuple[float, float]]:
    """Wilson score interval for k successes in n trials; None when n is 0."""
    if n <= 0:
        return None
    k = max(0, min(k, n))
    p = k / n
    z2 = z * z
    denom = 1.0 + z2 / n
    centre = (p + z2 / (2.0 * n)) / denom
    half = z * math.sqrt(p * (1.0 - p) / n + z2 / (4.0 * n * n)) / denom
    return (max(0.0, centre - half), min(1.0, centre + half))


def percentile(values: Sequence[float], q: float) -> Optional[float]:
    """Linear-interpolated percentile (q in 0..1) of unsorted values; None when empty."""
    if not values:
        return None
    s = sorted(values)
    if len(s) == 1:
        return float(s[0])
    pos = q * (len(s) - 1)
    lo = int(math.floor(pos))
    hi = min(lo + 1, len(s) - 1)
    return float(s[lo] + (s[hi] - s[lo]) * (pos - lo))


def median(values: Sequence[float]) -> Optional[float]:
    return percentile(values, 0.5)


def mean(values: Sequence[float]) -> Optional[float]:
    return (sum(values) / len(values)) if values else None


def _cap(values: Sequence[float]) -> list:
    return list(values[:BOOTSTRAP_MAX_VALUES])


def bootstrap_ci(
    values: Sequence[float],
    stat: Callable[[Sequence[float]], Optional[float]] = median,
    rounds: int = BOOTSTRAP_ROUNDS,
    seed: int = BOOTSTRAP_SEED,
    alpha: float = 0.05,
) -> Optional[tuple[float, float]]:
    """Percentile bootstrap interval of stat over values (resampling tasks). Seeded."""
    data = _cap(values)
    if len(data) < 2:
        return None
    rng = random.Random(seed)
    stats = []
    for _ in range(rounds):
        r = stat(rng.choices(data, k=len(data)))
        if r is not None:
            stats.append(r)
    if not stats:
        return None
    return (percentile(stats, alpha / 2.0), percentile(stats, 1.0 - alpha / 2.0))


def bootstrap_diff(
    a: Sequence[float],
    b: Sequence[float],
    stat: Callable[[Sequence[float]], Optional[float]] = median,
    rounds: int = BOOTSTRAP_ROUNDS,
    seed: int = BOOTSTRAP_SEED,
    alpha: float = 0.05,
) -> Optional[tuple[float, float, float]]:
    """stat(a) - stat(b) with a percentile bootstrap interval. The two samples are independent
    (different tasks), so each is resampled on its own. Returns (delta, lo, hi)."""
    da, db = _cap(a), _cap(b)
    if len(da) < 2 or len(db) < 2:
        return None
    sa, sb = stat(da), stat(db)
    if sa is None or sb is None:
        return None
    rng = random.Random(seed)
    deltas = []
    for _ in range(rounds):
        x = stat(rng.choices(da, k=len(da)))
        y = stat(rng.choices(db, k=len(db)))
        if x is not None and y is not None:
            deltas.append(x - y)
    if not deltas:
        return None
    return (sa - sb, percentile(deltas, alpha / 2.0), percentile(deltas, 1.0 - alpha / 2.0))


# ---------------------------------------------------------------- metric builders


def _insufficient(base: dict, n: int, min_n: int) -> dict:
    base.update(
        {
            "status": "insufficient",
            "n": n,
            "min_n": min_n,
            "message": "not enough data (n < %d)" % min_n,
        }
    )
    return base


def proportion_metric(
    mid: str, name: str, definition: str, k: int, n: int, min_n: int, caveat: str = ""
) -> dict:
    base = {"id": mid, "name": name, "kind": "proportion", "definition": definition,
            "caveat": caveat}
    if n < min_n or n <= 0:
        return _insufficient(base, n, min_n)
    lo, hi = wilson(k, n)
    base.update(
        {"status": "ok", "n": n, "min_n": min_n, "k": k, "value": k / n, "lo": lo, "hi": hi,
         "interval": "Wilson 95%"}
    )
    return base


def distribution_metric(
    mid: str, name: str, definition: str, values: Sequence[float], min_n: int, unit: str,
    caveat: str = "", extra: Optional[dict] = None,
) -> dict:
    base = {"id": mid, "name": name, "kind": "distribution", "definition": definition,
            "unit": unit, "caveat": caveat}
    if extra:
        base["extra"] = extra
    n = len(values)
    if n < min_n or n <= 0:
        return _insufficient(base, n, min_n)
    ci = bootstrap_ci(values)
    base.update(
        {
            "status": "ok", "n": n, "min_n": min_n,
            "p10": percentile(values, 0.10), "p50": median(values), "p90": percentile(values, 0.90),
            "interval": "bootstrap over tasks, 95%%, seed %d, %d rounds"
            % (BOOTSTRAP_SEED, BOOTSTRAP_ROUNDS),
            "lo": ci[0] if ci else None, "hi": ci[1] if ci else None,
        }
    )
    if n > BOOTSTRAP_MAX_VALUES:
        base["caveat"] = (caveat + " Interval uses the first %d values." % BOOTSTRAP_MAX_VALUES).strip()
    return base


# ---------------------------------------------------------------- the scorecard


def select_facts(facts, start=None, end=None, agent=None) -> list:
    """Tasks created in [start, end) (None = unbounded), attributed to agent when given."""
    out = []
    for f in facts:
        t = f.task["created_at"]
        if start is not None and t < start:
            continue
        if end is not None and t >= end:
            continue
        if agent is not None and f.claimant != agent:
            continue
        out.append(f)
    return out


def _durations(pairs):
    """Non-negative differences only; negative ones (clock skew between writers) are counted."""
    vals, skipped = [], 0
    for later, earlier in pairs:
        d = later - earlier
        if d < 0:
            skipped += 1
        else:
            vals.append(float(d))
    return vals, skipped


def time_to_claim_values(sel):
    return _durations(
        (f.first_claim_ts, f.task["created_at"]) for f in sel if f.first_claim_ts is not None
    )


def time_to_done_values(sel):
    return _durations(
        (f.done_ts, f.first_claim_ts)
        for f in sel
        if f.done_ts is not None and f.first_claim_ts is not None
    )


TRUNCATED_EVENTS_NOTE = (
    "The loaded events are only the newest window, so a task whose claim, done or note events are "
    "older has none of them here: it is left out or undercounted."
)
TRUNCATED_DECISIONS_NOTE = (
    "The loaded decisions are only the newest window, so older decisions are not counted."
)
# Metrics that read event times or counts, and so are affected when the events are cut off.
EVENT_METRICS = ("time_to_claim", "time_to_done", "claim_contention", "handoff_share",
                 "notes_per_task", "completion_rate")


def compute_scorecard(facts, now: int, stale_s: int, start=None, end=None, agent=None,
                      truncated_events: bool = False, truncated_decisions: bool = False) -> dict:
    """The scorecard for one cohort. `facts` come from model.task_facts."""
    sel = select_facts(facts, start, end, agent)
    metrics = []

    # 1. completion rate: top-level tasks (parent_id empty) of the cohort that are done.
    top = [f for f in sel if not f.task["parent_id"]]
    done_top = sum(1 for f in top if f.task["state"] == "done")
    cm = proportion_metric(
        "completion_rate", "Completion rate",
        "Share of top-level tasks (no parent) created in the window that are done now. For one "
        "agent: top-level tasks that agent claimed first. Done means the board says done; it is "
        "not a check that the work is right.",
        done_top, len(top), 20,
        "Right-censored: a task created recently has had no time to finish.",
    )
    all_ttd, _ = time_to_done_values(facts)
    med_all = median(all_ttd)
    if cm.get("status") == "ok" and med_all is not None:
        # Like with like: both sides are "time since the first claim". A claimed task that has run
        # for less than the median claim-to-done time may well still finish.
        running = [f for f in top if f.task["state"] == "claimed" and f.first_claim_ts is not None
                   and 0 <= now - f.first_claim_ts < med_all]
        unclaimed_top = sum(1 for f in top if f.task["state"] == "open")
        parts = []
        if running:
            parts.append("%d claimed tasks have run for less than the median claim-to-done time "
                         "(%d s) and may still finish" % (len(running), int(med_all)))
        if unclaimed_top:
            parts.append("%d are still open and count as not done" % unclaimed_top)
        if parts:
            cm["warning"] = "; ".join(parts) + ": the rate is biased low."
    metrics.append(cm)

    # 2. time to claim
    ttc, ttc_skipped = time_to_claim_values(sel)
    unclaimed = sum(1 for f in sel if f.first_claim_ts is None and f.task["state"] == "open")
    metrics.append(distribution_metric(
        "time_to_claim", "Time to claim",
        "Seconds from the task's creation to its first claim event. Tasks still open are left "
        "out and counted separately.",
        ttc, 15, "s", "Censored: open tasks are excluded.",
        {"unclaimed_open": unclaimed, "excluded_negative": ttc_skipped},
    ))

    # 3. time to done
    ttd, ttd_skipped = time_to_done_values(sel)
    metrics.append(distribution_metric(
        "time_to_done", "Time to done",
        "Seconds from the first claim event to the done event.",
        ttd, 15, "s", "Depends on task size; compare within a priority band only.",
        {"excluded_negative": ttd_skipped},
    ))

    # 4. stale-claim rate
    claimed_now = [f for f in sel if f.task["state"] == "claimed"]
    stale = sum(1 for f in claimed_now if now - f.last_activity > stale_s)
    denom = len([f for f in sel if f.task["state"] in ("claimed", "done")])
    metrics.append(proportion_metric(
        "stale_claim_rate", "Stale-claim rate",
        "Claimed tasks with no event for more than %d minutes, out of tasks that are claimed or "
        "done. A snapshot of now, not a rate over time." % (stale_s // 60),
        stale, denom, 20, "The threshold is a policy choice, not a measurement.",
    ))

    # 5. claim contention
    claimed_any = [f for f in sel if f.claims]
    contended = sum(1 for f in claimed_any if len({a for a, _, _ in f.claims}) >= 2)
    metrics.append(proportion_metric(
        "claim_contention", "Claim contention",
        "Tasks with claim events from two or more different authors, out of tasks with a claim.",
        contended, len(claimed_any), 20,
        "Lower bound. The board helper records only the winning claim, so a lost race leaves no "
        "event; only direct writes can produce a second claim event.",
    ))

    # 6. handoffs
    done_claimed = [f for f in sel if f.task["state"] == "done" and f.claims]
    handed = sum(1 for f in done_claimed if len({a for a, _, _ in f.claims}) >= 2)
    metrics.append(proportion_metric(
        "handoff_share", "Tasks with a handoff",
        "Done tasks claimed by two or more different authors, out of done tasks with a claim.",
        handed, len(done_claimed), 20,
        "The helper allows one claim per task, so this stays near zero until reassignment "
        "exists. An agent that restarts under a new name looks like a handoff.",
    ))

    # 7. decisions per done task (volume, not quality)
    done_all = [f for f in sel if f.task["state"] == "done"]
    metrics.append(distribution_metric(
        "decisions_per_done", "Decisions per done task",
        "Number of decisions recorded on each done task. A volume count; never a target.",
        [float(f.decisions) for f in done_all], 20, "decisions",
        "More decisions is not better. Do not optimise this.",
    ))

    # 8. notes per task (diagnostic)
    metrics.append(distribution_metric(
        "notes_per_task", "Notes per task",
        "Number of note events on each task in the cohort. Diagnostic only.",
        [float(f.notes) for f in sel], 30, "notes",
        "Confounded by task size and by how chatty the agent is.",
    ))

    for m in metrics:
        if truncated_events and m["id"] in EVENT_METRICS:
            m["caveat"] = (m["caveat"] + " " + TRUNCATED_EVENTS_NOTE).strip()
            m["truncated"] = True
        if truncated_decisions and m["id"] == "decisions_per_done":
            m["caveat"] = (m["caveat"] + " " + TRUNCATED_DECISIONS_NOTE).strip()
            m["truncated"] = True
    return {
        "scope": ("agent %s" % agent) if agent else "all agents",
        "tasks_in_cohort": len(sel),
        "window": {"from": start, "to": end},
        "metrics": metrics,
    }


def _compare_one(mid, name, a_vals, b_vals, stat, unit):
    out = {"id": mid, "name": name, "unit": unit, "n_a": len(a_vals), "n_b": len(b_vals),
           "min_n": COMPARE_MIN_N}
    if len(a_vals) < COMPARE_MIN_N or len(b_vals) < COMPARE_MIN_N:
        out.update({"status": "insufficient",
                    "message": "not enough data (n < %d in each window)" % COMPARE_MIN_N})
        return out
    r = bootstrap_diff(a_vals, b_vals, stat)
    if r is None:
        out.update({"status": "insufficient", "message": "not enough data for an interval"})
        return out
    delta, lo, hi = r
    out.update({"status": "ok", "delta": delta, "lo": lo, "hi": hi,
                "excludes_zero": bool(lo > 0 or hi < 0)})
    return out


def compare_windows(facts, win_a, win_b, agent=None, truncated_events: bool = False) -> dict:
    """A minus B for completion rate, time to claim and time to done. The windows hold
    different tasks, so a per-task pairing is impossible; the difference is an unpaired
    bootstrap over tasks, shown only when each window has COMPARE_MIN_N values."""
    sa = select_facts(facts, win_a[0], win_a[1], agent)
    sb = select_facts(facts, win_b[0], win_b[1], agent)

    def comp(sel):
        return [1.0 if f.task["state"] == "done" else 0.0 for f in sel if not f.task["parent_id"]]

    rows = [
        _compare_one("completion_rate", "Completion rate", comp(sa), comp(sb), mean, "share"),
        _compare_one("time_to_claim", "Time to claim (median)", time_to_claim_values(sa)[0],
                     time_to_claim_values(sb)[0], median, "s"),
        _compare_one("time_to_done", "Time to done (median)", time_to_done_values(sa)[0],
                     time_to_done_values(sb)[0], median, "s"),
    ]
    return {
        "method": "A minus B, unpaired bootstrap over tasks (95%%, seed %d). The windows contain "
                  "different tasks, so a paired comparison does not apply." % BOOTSTRAP_SEED,
        "window_a": {"from": win_a[0], "to": win_a[1], "tasks": len(sa)},
        "window_b": {"from": win_b[0], "to": win_b[1], "tasks": len(sb)},
        "note": "Do not rank agents or runs on fewer than about %d tasks per window." % COMPARE_MIN_N,
        "warning": TRUNCATED_EVENTS_NOTE if truncated_events else None,
        "rows": rows,
    }
