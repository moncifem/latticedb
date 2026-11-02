#!/usr/bin/env python3
"""A stand-in for psql used by test_cli.py. It never talks to a database.

It logs every call (argument list and the SQL read from stdin) as one JSON line in $FAKE_LOG and
answers from the rules in the JSON file $FAKE_RULES: the first rule whose "match" regex is found in
the SQL followed by the arguments wins. A rule has "out" (the text printed, as psql -t would) or
"outs" (a list, one entry per call that reaches the rule; the last repeats), "seq" (a list of
{out, err, rc} steps, the same way), and optionally "rc"
and "err". A call that matches no rule prints nothing and succeeds.
"""

import json
import os
import re
import sys

sql = sys.stdin.read()
argv = sys.argv[1:]
log_path = os.environ["FAKE_LOG"]
with open(log_path, "a", encoding="utf-8") as f:
    f.write(json.dumps({"argv": argv, "sql": sql}) + "\n")

with open(os.environ["FAKE_RULES"], encoding="utf-8") as f:
    rules = json.load(f)

hay = sql + "\n" + " ".join(argv)
counts_path = log_path + ".counts"
try:
    with open(counts_path, encoding="utf-8") as f:
        counts = json.load(f)
except (OSError, ValueError):
    counts = {}

for i, rule in enumerate(rules):
    if re.search(rule["match"], hay, re.S):
        n = counts.get(str(i), 0)
        counts[str(i)] = n + 1
        with open(counts_path, "w", encoding="utf-8") as f:
            json.dump(counts, f)
        if "seq" in rule:
            step = rule["seq"][min(n, len(rule["seq"]) - 1)]
            if step.get("out"):
                sys.stdout.write(step["out"] + "\n")
            if step.get("err"):
                sys.stderr.write(step["err"] + "\n")
            sys.exit(step.get("rc", 0))
        if "outs" in rule:
            out = rule["outs"][min(n, len(rule["outs"]) - 1)]
        else:
            out = rule.get("out", "")
        if out:
            sys.stdout.write(out + "\n")
        if rule.get("err"):
            sys.stderr.write(rule["err"] + "\n")
        sys.exit(rule.get("rc", 0))
sys.exit(0)
