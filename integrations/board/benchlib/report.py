"""From the rows of one experiment to the honest report: per-arm numbers with intervals, the paired
comparison of every two arms, a task-by-arm grid and the infra errors. Pure functions: no database, no
clock, no randomness beyond a seeded bootstrap. The CLI prints it, the console serves the same dict.

How an attempt counts. Outcomes: pass, fail, timeout, error, planned.
  * pass is a success. fail and timeout are failures of the agent (a timeout means the agent did not finish).
  * error is a failure of the HARNESS (the fixture did not match its digest, the grader could not run, the
    disk was full). It is listed on its own and is never in any rate, so a broken machine cannot make an arm
    look worse.
  * planned means the attempt has not been recorded: not run yet, still running, or the worker died. It is
    counted as pending and is in no rate either.
Every proportion carries its interval and its n in the same string (the *_text fields); there is no field
that holds a percentage alone for the console or the CLI to print.
"""

from __future__ import annotations

import math
from itertools import combinations
from typing import Optional

from . import stats

VALID = ("pass", "fail", "timeout")
OUTCOMES = ("pass", "fail", "timeout", "error", "planned")
MIN_PAIRED_TASKS = 30  # a policy value, not a derived one: below it no arm is called better (see verdict)
ATTEMPT_FIELDS = ("board_task_id", "arm", "task_key", "repeat", "order", "outcome", "flaky", "tampered",
                  "duration_ms", "cost_usd", "tokens", "grader_exit", "grader_runs", "harness", "recorded_at",
                  "summary")
_INT_FIELDS = ("repeat", "order", "flaky", "tampered", "duration_ms", "tokens", "grader_exit", "recorded_at")


class RowsError(Exception):
    """The rows from the helper are not in the documented shape."""


def parse_rows(text: str) -> dict:
    """Parse the output of `board bench-rows` (tagged tab separated lines, see the helper's header)."""
    data = {"experiment": None, "arms": [], "tasks": [], "attempts": [], "truncated": False}
    for line in text.splitlines():
        if not line:
            continue
        parts = line.split("\t")
        tag = parts[0]
        try:
            if tag == "experiment":
                f = parts[1:]
                if len(f) != 9:
                    raise ValueError("experiment line")
                data["experiment"] = {"name": f[0], "suite": f[1], "suite_sha": f[2], "seed": int(f[3]),
                                      "repeats": int(f[4]), "planned": int(f[5]), "harness": f[6],
                                      "created_by": f[7], "created_at": int(f[8])}
            elif tag == "arm":
                if len(parts) != 4:
                    raise ValueError("arm line")
                data["arms"].append({"arm": parts[1], "agent": parts[2], "config_sha": parts[3]})
            elif tag == "task":
                if len(parts) != 3:
                    raise ValueError("task line")
                data["tasks"].append({"task": parts[1], "fixture_digest": parts[2]})
            elif tag == "attempt":
                if len(parts) != 17:
                    raise ValueError("attempt line")
                row = dict(zip(ATTEMPT_FIELDS, parts[1:]))
                for k in _INT_FIELDS:
                    row[k] = int(row[k]) if row[k] != "" else None
                row["cost_usd"] = float(row["cost_usd"]) if row["cost_usd"] != "" else None
                if row["outcome"] not in OUTCOMES:
                    raise ValueError("outcome")
                row["flaky"] = bool(row["flaky"])
                row["tampered"] = row["tampered"] or 0
                data["attempts"].append(row)
            elif tag == "truncated":
                data["truncated"] = True
            else:
                raise ValueError("tag")
        except ValueError:
            raise RowsError("unexpected %s line from the board helper" % tag[:20]) from None
    if data["experiment"] is None:
        raise RowsError("no experiment line in the rows")
    return data


# ---- formatting: a percentage is never produced without its interval and n ----------------------


def _tenths(p: float, up: bool) -> float:
    """p (in percent) rounded to one decimal, down or up, ignoring float noise of 1e-9."""
    return (math.ceil(p * 10 - 1e-9) if up else math.floor(p * 10 + 1e-9)) / 10.0


def _whole(p: float, up: bool) -> int:
    return int(math.ceil(p - 1e-9) if up else math.floor(p + 1e-9))


def pct(x: float) -> str:
    """A point value, rounded to the nearest 0.1 in the tails and to a whole percent elsewhere. A value below
    1 never prints as 100%, and a value above 0 never prints as 0% (it gets a second decimal instead)."""
    p = x * 100.0
    if p >= 100.0:
        return "100%"
    if p <= 0.0:
        return "0%"
    if p < 10 or p > 90:
        s = "%.1f" % p
        if s == "100.0":
            s = "%.2f" % p
            if s == "100.00":
                return ">99.99%"
        elif s == "0.0":
            s = "%.2f" % p
            if s == "0.00":
                return "<0.01%"
        return s + "%"
    return "%.0f%%" % p


def pct_lo(x: float) -> str:
    """A lower bound, rounded DOWN, so the printed bound is never above the true one (and a bound below 1
    never prints as 100.0%)."""
    p = x * 100.0
    if p <= 0.0:
        return "0%"
    if p >= 100.0:
        return "100%"
    return ("%.1f%%" % _tenths(p, False)) if (p < 10 or p > 90) else ("%d%%" % _whole(p, False))


def pct_hi(x: float) -> str:
    """An upper bound, rounded UP. A bound below 1 whose rounding would reach 100.0% gets a second decimal
    (or ">99.99%"), never "100.0%"."""
    p = x * 100.0
    if p >= 100.0:
        return "100%"
    if p <= 0.0:
        return "0%"
    if p < 10 or p > 90:
        t = _tenths(p, True)
        if t >= 100.0:
            h = math.ceil(p * 100 - 1e-9) / 100.0
            return ">99.99%" if h >= 100.0 else "%.2f%%" % h
        return "%.1f%%" % t
    return "%d%%" % _whole(p, True)


def rate_text(value: Optional[float], ci: Optional[tuple], n_tasks: int, n_attempts: int) -> str:
    """"62.5% (95% CI 35.4% to 84.8%), n = 8 tasks, 24 attempts"; says so when there is nothing to show."""
    if value is None:
        return "no result (n = 0 tasks)"
    span = "95%% CI %s to %s" % (pct_lo(ci[0]), pct_hi(ci[1])) if ci else "no interval"
    return "%s (%s), n = %d task%s, %d attempt%s" % (pct(value), span, n_tasks, "" if n_tasks == 1 else "s",
                                                     n_attempts, "" if n_attempts == 1 else "s")


# ---- per arm ------------------------------------------------------------------------------------


def _by_cell(attempts):
    cells = {}
    for a in attempts:
        cells.setdefault((a["arm"], a["task_key"]), []).append(a)
    return cells


def cell_counts(rows) -> dict:
    c = {o: 0 for o in OUTCOMES}
    for r in rows:
        c[r["outcome"]] += 1
    n = c["pass"] + c["fail"] + c["timeout"]
    return {"n": n, "passes": c["pass"], "fails": c["fail"], "timeouts": c["timeout"], "errors": c["error"],
            "pending": c["planned"], "flaky": sum(1 for r in rows if r["flaky"] and r["outcome"] in VALID),
            "tampered": sum(1 for r in rows if r["tampered"] and r["outcome"] in VALID)}


def arm_summary(arm: str, agent: str, planned: int, cells: dict, tasks: list, k: int) -> dict:
    per = [(t, cell_counts(cells.get((arm, t), []))) for t in tasks]
    counted = [(t, c) for t, c in per if c["n"] > 0]
    rates = [c["passes"] / c["n"] for _, c in counted]
    n_att = sum(c["n"] for _, c in counted)
    passes = sum(c["passes"] for _, c in counted)
    p1, _, _ = stats.mean_over_tasks(rates)
    ci = stats.task_level_wilson(rates) if rates else None
    naive = stats.wilson(passes, n_att) if n_att else None
    atk_vals = [stats.pass_at_k(c["n"], c["passes"], k) for _, c in counted]
    ptk_vals = [stats.pass_pow_k(c["n"], c["passes"], k) for _, c in counted]
    atk = stats.mean_over_tasks(atk_vals)
    ptk = stats.mean_over_tasks(ptk_vals)
    rows = [a for (ar, _), rs in cells.items() if ar == arm for a in rs]
    durs = [a["duration_ms"] for a in rows if a["outcome"] in VALID and a["duration_ms"] is not None]
    costs = [a["cost_usd"] for a in rows if a["outcome"] in VALID and a["cost_usd"] is not None]
    total = {key: sum(c[key] for _, c in per) for key in ("errors", "pending", "flaky", "tampered", "timeouts")}
    return {
        "arm": arm, "agent": agent, "planned": planned,
        "recorded": sum(1 for a in rows if a["outcome"] != "planned"),
        "tasks": len(counted), "attempts": n_att, "passes": passes,
        "errors": total["errors"], "pending": total["pending"], "timeouts": total["timeouts"],
        "flaky": total["flaky"], "tampered": total["tampered"],
        "pass_at_1": {"value": p1, "ci": list(ci) if ci else None, "n_tasks": len(counted), "n_attempts": n_att,
                      "text": rate_text(p1, ci, len(counted), n_att) + _excluded(total["errors"], total["pending"]),
                      "method": "mean of per-task pass rates; interval is Wilson with n = number of tasks"},
        "naive_ci": list(naive) if naive else None,
        "pass_at_k": _kmetric(k, atk, "pass@%d" % k, "at least one of %d attempts passes" % k, atk_vals),
        "pass_pow_k": _kmetric(k, ptk, "pass^%d" % k, "all %d attempts pass" % k, ptk_vals),
        "duration": _mean_with_n(durs, "ms"),
        "cost": _mean_with_n(costs, "usd", total=True),
    }


def _excluded(errors, pending):
    bits = []
    if errors:
        bits.append("%d harness error%s excluded" % (errors, "" if errors == 1 else "s"))
    if pending:
        bits.append("%d not recorded yet" % pending)
    return ("; " + ", ".join(bits)) if bits else ""


def _kmetric(k, triple, label, meaning, values):
    """pass@k or pass^k with a bootstrap interval over the tasks it covers (None below two tasks)."""
    value, used, skipped = triple
    ci = stats.bootstrap_mean_ci([v for v in values if v is not None])
    if value is None:
        text = "no result"
    else:
        span = ("95%% CI %s to %s" % (pct_lo(ci[1]), pct_hi(ci[2]))) if ci else "no interval"
        text = "%s (%s) over %d task%s" % (pct(value), span, used, "" if used == 1 else "s")
    if skipped:
        text += " (%d task%s with fewer than %d counted attempts left out)" % (skipped, "" if skipped == 1 else "s", k)
    return {"k": k, "label": label, "meaning": meaning, "value": value, "ci": [ci[1], ci[2]] if ci else None,
            "n_tasks": used, "skipped": skipped, "text": text}


def _mean_with_n(vals, unit, total=False):
    out = {"unit": unit, "n": len(vals), "mean": (sum(vals) / len(vals)) if vals else None}
    if total:
        out["total"] = sum(vals) if vals else None
    return out


# ---- two arms -----------------------------------------------------------------------------------


def solved(counts: dict, rule: str) -> bool:
    """A task is solved by an arm under the "majority" rule when strictly more than half of its counted
    attempts passed (a tie is not solved), under "any" when at least one did."""
    if rule == "any":
        return counts["passes"] >= 1
    return counts["passes"] * 2 > counts["n"]


MAX_ROUNDS = 10000  # bootstrap resamples per comparison, at most (cost grows with arms, tasks and rounds)


def compare(a: dict, b: dict, cells: dict, tasks: list, rule: str, min_tasks: int, comparisons: int) -> dict:
    """Arm B against arm A on the tasks both have counted attempts for."""
    paired, dropped = [], []
    for t in tasks:
        ca, cb = cell_counts(cells.get((a["arm"], t), [])), cell_counts(cells.get((b["arm"], t), []))
        if ca["n"] > 0 and cb["n"] > 0:
            paired.append((t, ca, cb))
        elif ca["n"] > 0 or cb["n"] > 0:
            dropped.append(t)
    ra = [ca["passes"] / ca["n"] for _, ca, _ in paired]
    rb = [cb["passes"] / cb["n"] for _, _, cb in paired]
    a_only = sum(1 for _, ca, cb in paired if solved(ca, rule) and not solved(cb, rule))
    b_only = sum(1 for _, ca, cb in paired if solved(cb, rule) and not solved(ca, rule))
    both = sum(1 for _, ca, cb in paired if solved(ca, rule) and solved(cb, rule))
    neither = len(paired) - a_only - b_only - both
    alpha = 0.05 / max(comparisons, 1)
    # The percentile bootstrap needs about 20 draws beyond each tail to place an interval of level 1 - alpha; as
    # Bonferroni shrinks alpha the number of resamples grows, up to a cap that the report states.
    rounds = min(MAX_ROUNDS, max(stats.BOOTSTRAP_ROUNDS, int(math.ceil(20.0 / alpha))))
    diff = stats.paired_bootstrap_diff(ra, rb, alpha=alpha, rounds=rounds) if len(paired) >= 2 else None
    p = stats.mcnemar_exact(a_only, b_only)
    out = {
        "a": a["arm"], "b": b["arm"], "paired_tasks": len(paired), "rule": rule,
        "rule_text": ("a task counts as solved by an arm when more than half of its counted attempts passed"
                      if rule == "majority" else "a task counts as solved by an arm when any counted attempt passed"),
        "table": {"both": both, "a_only": a_only, "b_only": b_only, "neither": neither},
        "mcnemar_p": p, "mcnemar_p_adjusted": stats.bonferroni(p, comparisons), "comparisons": comparisons,
        "ci_level": 1.0 - alpha, "rounds": rounds, "rounds_capped": 20.0 / alpha > MAX_ROUNDS,
        "degenerate": len(paired) >= 2 and len({round(y - x, 12) for x, y in zip(ra, rb)}) == 1,
        "dropped_tasks": len(dropped), "errors": {a["arm"]: a["errors"], b["arm"]: b["errors"]},
        "diff": ({"mean": diff[0], "lo": diff[1], "hi": diff[2]} if diff else None),
        "min_tasks": min_tasks,
        "tasks": [{"task": t, "a": _pair_cell(ca), "b": _pair_cell(cb)} for t, ca, cb in paired],
    }
    out["verdict"] = verdict(out)
    return out


def _pair_cell(c):
    return {"n": c["n"], "passes": c["passes"]}


def _pts(x):
    v = round(x * 100.0, 1)
    return "%+.1f points" % (v if v != 0 else 0.0)


ALPHA = 0.05


def verdict(cmp: dict) -> dict:
    """label is "inconclusive" unless ALL of these hold: at least min_tasks tasks are paired, the bootstrap
    interval of B minus A excludes zero, and the exact McNemar test, adjusted for the number of comparisons,
    rejects "no difference" at 0.05. The exact test is required because the bootstrap alone can name a winner
    from a handful of changed tasks (4 of 30 tasks changed, none the other way: bootstrap excludes zero, the
    exact two-sided p is 0.125). Every text states n, the interval and the minimum; warnings are appended when
    arms lost different attempts to harness errors or tasks could not be paired."""
    d, n, mn = cmp["diff"], cmp["paired_tasks"], cmp["min_tasks"]
    a, b = cmp["a"], cmp["b"]
    p_adj = cmp.get("mcnemar_p_adjusted")
    warn = _warnings(cmp)
    lvl = "%.1f%%" % (cmp["ci_level"] * 100.0)
    if d is None:
        return {"label": "inconclusive", "text": "Inconclusive: %d paired task%s is too few to estimate a difference.%s"
                % (n, "" if n == 1 else "s", warn)}
    interval = "%s CI %s to %s" % (lvl, _pts(d["lo"]), _pts(d["hi"]))
    head = "%s minus %s: %s (%s), %d paired tasks" % (b, a, _pts(d["mean"]), interval, n)
    if n < mn:
        return {"label": "inconclusive", "text": "Inconclusive: %s. Fewer than the minimum of %d paired tasks, so no arm is called better, "
                                                   "whatever the interval shows.%s" % (head, mn, warn)}
    if cmp.get("degenerate") and d["mean"] != 0:
        # every paired difference is the same number: resampling cannot show any uncertainty
        label = "b_better" if d["mean"] > 0 else "a_better"
        better = b if d["mean"] > 0 else a
        if p_adj is not None and p_adj < ALPHA:
            return {"label": label, "text": "%s did better: %s. Every paired task differed by the same amount, so the bootstrap interval has "
                                            "no width and says nothing; the call rests on the exact McNemar test (adjusted p = %.3g).%s"
                    % (better[0].upper() + better[1:], head, p_adj, warn)}
        return {"label": "inconclusive", "text": "Inconclusive: %s. Every paired task differed by the same amount, so the bootstrap interval "
                                                   "has no width, and the exact McNemar test does not reject (adjusted p = %.3g).%s"
                % (head, p_adj if p_adj is not None else 1.0, warn)}
    if d["lo"] > 0:
        label, said = "b_better", "%s did better than %s" % (b, a)
    elif d["hi"] < 0:
        label, said = "a_better", "%s did better than %s" % (a, b)
    else:
        return {"label": "inconclusive", "text": "Inconclusive: %s. The interval includes zero.%s" % (head, warn)}
    if p_adj is None or p_adj >= ALPHA:
        return {"label": "inconclusive",
                "text": "Inconclusive: %s. The interval excludes zero but the exact McNemar test on tasks does not reject 'no difference' "
                        "(p = %.3g after adjusting for %d comparison%s), so the two disagree and no arm is called better.%s"
                % (head, p_adj if p_adj is not None else 1.0, cmp.get("comparisons", 1), "" if cmp.get("comparisons", 1) == 1 else "s", warn)}
    return {"label": label, "text": "%s: %s. The interval excludes zero and the exact McNemar test agrees (adjusted p = %.3g).%s"
            % (said[0].upper() + said[1:], head, p_adj, warn)}


def _warnings(cmp: dict) -> str:
    out = []
    if cmp.get("dropped_tasks"):
        out.append("%d task%s counted for only one arm and left out of the pairing." % (
            cmp["dropped_tasks"], " was" if cmp["dropped_tasks"] == 1 else "s were"))
    errs = cmp.get("errors") or {}
    if len(set(errs.values())) > 1:
        out.append("Harness errors differ between the arms (%s); errors are excluded, so a difference can come from which attempts were "
                   "dropped." % ", ".join("%s: %d" % kv for kv in sorted(errs.items())))
    if cmp.get("rounds_capped"):
        out.append("With this many comparisons the bootstrap uses %d resamples, fewer than its tails need; the interval is approximate."
                   % cmp.get("rounds", 0))
    return (" " + " ".join(out)) if out else ""


# ---- the report ---------------------------------------------------------------------------------


def build_report(data: dict, rule: str = "majority", min_tasks: int = MIN_PAIRED_TASKS) -> dict:
    exp = data["experiment"]
    arms = sorted(a["arm"] for a in data["arms"])
    agent_of = {a["arm"]: a["agent"] for a in data["arms"]}
    tasks = sorted({t["task"] for t in data["tasks"]} | {a["task_key"] for a in data["attempts"]})
    cells = _by_cell(data["attempts"])
    k = exp["repeats"]
    planned_per_arm = len(tasks) * k
    summaries = [arm_summary(a, agent_of.get(a, "?"), planned_per_arm, cells, tasks, k) for a in arms]
    by_arm = {s["arm"]: s for s in summaries}
    pairs = list(combinations(arms, 2))
    comps = [compare(by_arm[x], by_arm[y], cells, tasks, rule, min_tasks, len(pairs)) for x, y in pairs]
    grid = {t: {a: cell_counts(cells.get((a, t), [])) for a in arms} for t in tasks}
    errors = [{"board_task_id": a["board_task_id"], "arm": a["arm"], "task": a["task_key"], "repeat": a["repeat"],
               "summary": a["summary"]} for a in data["attempts"] if a["outcome"] == "error"]
    pending = sum(1 for a in data["attempts"] if a["outcome"] == "planned")
    notes = []
    if data["truncated"]:
        notes.append("More than 20000 attempts exist; only the first 20000 are in this report.")
    if pending:
        notes.append("%d attempt%s not recorded yet (not run, still running, or the worker died); they are in no rate." % (pending, "" if pending == 1 else "s"))
    if errors:
        notes.append("%d attempt%s ended in a harness error and %s left out of every rate." % (len(errors), "" if len(errors) == 1 else "s", "is" if len(errors) == 1 else "are"))
    if len({s["errors"] for s in summaries}) > 1:
        notes.append("Harness errors differ between arms (%s): they are excluded from every rate, so compare the arms with that in mind."
                     % ", ".join("%s: %d" % (s["arm"], s["errors"]) for s in summaries))
    if any(s["tampered"] for s in summaries):
        notes.append("Some agents changed files the grader protects; those files were restored before grading, and the attempts are counted as graded.")
    if any(s["flaky"] for s in summaries):
        notes.append("Flaky attempts (the grader's reruns disagreed) are marked; they were decided by a strict majority of the grader runs.")
    return {
        "experiment": exp, "rule": rule, "min_tasks": min_tasks, "k": k,
        "arms": summaries, "comparisons": comps, "tasks": tasks, "grid": grid, "errors": errors,
        "pending": pending, "notes": notes,
        "rules": {
            "counted": "pass, fail and timeout count; error (harness) and planned (not recorded) are in no rate",
            "unit": "the task is the unit: attempts of one task are not independent, so every interval resamples or counts tasks",
            "pass_at_1": "mean over tasks of passes/attempts, with a Wilson interval whose n is the number of tasks",
            "pass_at_k": "1 - C(n-c, k) / C(n, k) per task, averaged over tasks with at least k counted attempts",
            "pass_pow_k": "C(c, k) / C(n, k) per task, averaged over the same tasks",
            "verdict": "inconclusive unless the interval of the paired difference excludes zero, the adjusted exact McNemar test gives p < 0.05, and there are at least %d paired tasks" % min_tasks,
        },
    }


def _dur(ms: float) -> str:
    return ("%.0f ms" % ms) if ms < 1000 else ("%.1f s" % (ms / 1000.0))


def render_text(rep: dict) -> str:
    """The report as plain text for a terminal."""
    e = rep["experiment"]
    out = ["experiment %s   suite %s   seed %d   repeats %d   attempts planned %d   harness %s"
           % (e["name"], e["suite"], e["seed"], e["repeats"], e["planned"], e["harness"]), ""]
    for s in rep["arms"]:
        out.append("arm %s (%s): %d recorded of %d planned, %d errors left out, %d pending, %d flaky, %d timeouts"
                   % (s["arm"], s["agent"], s["recorded"], s["planned"], s["errors"], s["pending"], s["flaky"], s["timeouts"]))
        out.append("  pass@1   %s" % s["pass_at_1"]["text"])
        ci = s["pass_at_1"]["ci"]
        if s["naive_ci"] and ci and (s["naive_ci"][1] - s["naive_ci"][0]) < (ci[1] - ci[0]) - 0.001:
            out.append("           (counting every attempt as independent would give %s to %s, which is too narrow)"
                       % (pct(s["naive_ci"][0]), pct(s["naive_ci"][1])))
        out.append("  %-8s %s  [%s]" % (s["pass_at_k"]["label"], s["pass_at_k"]["text"], s["pass_at_k"]["meaning"]))
        out.append("  %-8s %s  [%s]" % (s["pass_pow_k"]["label"], s["pass_pow_k"]["text"], s["pass_pow_k"]["meaning"]))
        d, c = s["duration"], s["cost"]
        out.append("  duration %s" % (("mean %s over %d attempts" % (_dur(d["mean"]), d["n"])) if d["n"] else "not reported"))
        out.append("  cost     %s" % ("mean $%.4f, total $%.4f, over %d of %d counted attempts (only where the agent reported a cost)"
                                      % (c["mean"], c["total"], c["n"], s["attempts"]) if c["n"] else "not reported by the agent"))
        out.append("")
    for c in rep["comparisons"]:
        t = c["table"]
        out.append("%s vs %s, %d paired tasks (rule: %s)" % (c["a"], c["b"], c["paired_tasks"], c["rule_text"]))
        out.append("  solved by both %d, only %s %d, only %s %d, neither %d" % (t["both"], c["a"], t["a_only"], c["b"], t["b_only"], t["neither"]))
        out.append("  exact McNemar p = %.4g (%.4g adjusted for %d comparison%s)" % (c["mcnemar_p"], c["mcnemar_p_adjusted"], c["comparisons"], "" if c["comparisons"] == 1 else "s"))
        out.append("  VERDICT: %s" % c["verdict"]["text"])
        out.append("")
    if rep["errors"]:
        out.append("harness errors (not counted against any arm):")
        for x in rep["errors"][:50]:
            out.append("  %s %s/%s #%d: %s" % (x["board_task_id"], x["arm"], x["task"], x["repeat"], x["summary"]))
        out.append("")
    for n in rep["notes"]:
        out.append("note: " + n)
    return "\n".join(out).rstrip() + "\n"
