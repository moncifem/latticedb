#!/usr/bin/env python3
"""Checks sqllogictest scripts against a real PostgreSQL.

Usage: slt_reference.py SCRIPT...   (connection settings come from the usual PG* variables)

Every `statement ok` must succeed, every `statement error CODE` must fail with that SQLSTATE and
every `query` must return the expected values. Records that expect `0A000` are skipped: they are
constructs PostgreSQL accepts and Lattice rejects. Each statement runs in its own connection, so
scripts that depend on one session (BEGIN ... COMMIT) cannot be checked here and are listed in
SESSION_SCRIPTS. A record preceded by a comment line `# differs-from-postgres` is skipped: it states what
Lattice answers by design where PostgreSQL answers otherwise (for example binary string order
against the database collation). Exits non-zero on any mismatch, when no script is given or checked
(an unmatched glob arrives as its literal text or as nothing), or when the server fails a `select 1`
preflight.
"""
import re
import subprocess
import sys

SESSION_SCRIPTS = {"transactions.test", "transactions_visibility.test"}
# psql exits 1 for a fatal error, 2 when the connection to the server failed and 3 for an error in a
# script. Only 2 (or a missing binary) means the server was never reached; with `-c`, a rejected
# statement exits 1 and prints `ERROR:  <SQLSTATE>` under VERBOSITY=sqlstate.
CONNECTION_FAILURE = 2
SQLSTATE = re.compile(r"^ERROR:\s+([0-9A-Z]{5})\s*$", re.M)


class ServerUnreachable(Exception):
    pass


def psql(sql):
    try:
        p = subprocess.run(
            ["psql", "-qAt", "-F", "\t", "-v", "VERBOSITY=sqlstate", "-c", sql],
            capture_output=True, text=True,
        )
    except OSError as e:
        raise ServerUnreachable(f"cannot run psql: {e}") from e
    if p.returncode == CONNECTION_FAILURE:
        raise ServerUnreachable(p.stderr.strip() or "psql could not connect")
    return p.returncode, p.stdout, p.stderr


def preflight():
    """The server must answer `select 1` before any record counts as evidence."""
    rc, out, err = psql("select 1")
    if rc != 0 or out.strip() != "1":
        raise ServerUnreachable(f"select 1 failed (exit {rc}): {err.strip() or out.strip()}")


def check(path):
    lines = open(path).read().split("\n")
    i = 0
    bad = 0
    rc, _, err = psql("drop schema public cascade; create schema public;")
    if rc != 0:
        print(f"{path}: cannot reset the schema: {err.strip()}")
        return 1
    skip_next = False

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
        if head.startswith("#"):
            if head.startswith("# differs-from-postgres"):
                skip_next = True
            continue
        if not head:
            continue
        words = head.split()
        skip = skip_next
        skip_next = False
        if words[0] == "statement":
            sql = block(False)
            if skip or words[1:3] == ["error", "0A000"]:
                continue
            rc, _, err = psql(sql)
            if words[1] == "ok":
                if rc != 0:
                    print(f"{path}: statement failed: {sql!r}: {err.strip()}")
                    bad += 1
                continue
            code = words[2] if len(words) > 2 else None
            if rc == 0:
                print(f"{path}: expected an error: {sql!r}")
                bad += 1
            else:
                # rc is non-zero and the server was reached (exit 2 raised above), so this is a
                # query error. A stated code must be the SQLSTATE psql reported.
                got = SQLSTATE.search(err)
                if got is None:
                    print(f"{path}: expected an error with a SQLSTATE, got {err.strip()!r}: {sql!r}")
                    bad += 1
                elif code and got.group(1) != code:
                    print(f"{path}: expected {code}, got {got.group(1)}: {sql!r}")
                    bad += 1
        elif words[0] == "query":
            sql = block(True)
            i += 1
            body = block(False) if i <= len(lines) else ""
            expected = body.split("\n") if body else []
            if skip:
                continue
            mode = words[2] if len(words) > 2 else "nosort"
            rc, out, err = psql(sql)
            if rc != 0:
                print(f"{path}: query failed: {sql!r}: {err.strip()}")
                bad += 1
                continue
            # One line per row; a row whose only value is NULL is an empty line, so drop just the
            # final newline instead of stripping every trailing one.
            rows = [r.split("\t") for r in out.split("\n")[:-1]] if out else []
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
    paths = sys.argv[1:]
    if not paths:
        print("no scripts matched: an empty set proves nothing")
        return 1
    try:
        preflight()
    except ServerUnreachable as e:
        print(f"reference server not usable: {e}")
        return 1
    total = 0
    checked = 0
    for path in paths:
        if path.rsplit("/", 1)[-1] in SESSION_SCRIPTS:
            print(f"skipping {path}: needs one session")
            continue
        try:
            total += check(path)
        except ServerUnreachable as e:
            print(f"{path}: reference server lost: {e}")
            return 1
        checked += 1
    if checked == 0:
        print("no script was checked")
        return 1
    print(f"checked {checked} scripts, mismatches: {total}")
    return 1 if total else 0


if __name__ == "__main__":
    sys.exit(main())
