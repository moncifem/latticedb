#!/usr/bin/env python3
"""Checks sqllogictest scripts against a real PostgreSQL.

Usage: slt_reference.py SCRIPT...   (connection settings come from the usual PG* variables)

Every `statement ok` must succeed, every `statement error CODE` must fail with that SQLSTATE and
every `query` must return the expected values. Records that expect `0A000` are skipped: they are
constructs PostgreSQL accepts and Lattice rejects. Each statement runs in its own connection, so
scripts that depend on one session (BEGIN ... COMMIT) cannot be checked here and are listed in
SESSION_SCRIPTS. Exits non-zero on any mismatch.
"""
import subprocess
import sys

SESSION_SCRIPTS = {"transactions.test"}


def psql(sql):
    p = subprocess.run(
        ["psql", "-qAt", "-F", "\t", "-v", "VERBOSITY=sqlstate", "-c", sql],
        capture_output=True, text=True,
    )
    return p.returncode, p.stdout, p.stderr


def check(path):
    lines = open(path).read().split("\n")
    i = 0
    bad = 0
    psql("drop schema public cascade; create schema public;")

    def block(stop_at_separator):
        nonlocal i
        out = []
        while i < len(lines) and lines[i].strip() != "" and not (stop_at_separator and lines[i].strip() == "----"):
            out.append(lines[i])
            i += 1
        return "\n".join(out)

    while i < len(lines):
        head = lines[i].strip()
        i += 1
        if not head or head.startswith("#"):
            continue
        words = head.split()
        if words[0] == "statement":
            sql = block(False)
            rc, _, err = psql(sql)
            if words[1] == "ok":
                if rc != 0:
                    print(f"{path}: statement failed: {sql!r}: {err.strip()}")
                    bad += 1
                continue
            code = words[2] if len(words) > 2 else None
            if code == "0A000":
                continue
            if rc == 0:
                print(f"{path}: expected an error: {sql!r}")
                bad += 1
            elif code and code not in err:
                print(f"{path}: expected {code}, got {err.strip()!r}: {sql!r}")
                bad += 1
        elif words[0] == "query":
            sql = block(True)
            i += 1
            expected = block(False).split("\n") if i <= len(lines) else []
            mode = words[2] if len(words) > 2 else "nosort"
            rc, out, err = psql(sql)
            if rc != 0:
                print(f"{path}: query failed: {sql!r}: {err.strip()}")
                bad += 1
                continue
            rows = [r.split("\t") for r in out.strip("\n").split("\n")] if out.strip("\n") else []
            rows = [["NULL" if v == "" else v for v in r] for r in rows]
            if mode == "rowsort":
                rows.sort()
            got = [v for r in rows for v in r]
            if mode == "valuesort":
                got.sort()
                expected = sorted(expected)
            if got != expected:
                print(f"{path}: {sql!r}\n  expected {expected}\n  got      {got}")
                bad += 1
    return bad


def main():
    total = 0
    for path in sys.argv[1:]:
        if path.rsplit("/", 1)[-1] in SESSION_SCRIPTS:
            print(f"skipping {path}: needs one session")
            continue
        total += check(path)
    print(f"mismatches: {total}")
    return 1 if total else 0


if __name__ == "__main__":
    sys.exit(main())
