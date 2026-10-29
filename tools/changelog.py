#!/usr/bin/env python3
"""Changelog fragments: validate them in the PR gate and compile them for a release.

Every pull request adds one file to `changelog.d/` instead of editing CHANGELOG.md, so two pull
requests that both record a change never touch the same lines. Convention and rationale:
docs/agent-workflow.md ("Changelog fragments") and changelog.d/README.md.

Fragment name: `<number>-<slug>.<kind>.md` where `<number>` is the pull request or issue number,
`<slug>` is lowercase words joined by `-`, and `<kind>` is one of KINDS. The file holds the entry
text as Markdown, without a leading bullet.

Commands:
  check    validate the fragments in the tree and, with --pr, the fragments the PR adds
  compile  turn the fragments into a CHANGELOG.md section for a release (dry run by default)

The pure functions have no I/O and are unit tested in test_changelog.py.
"""

import argparse
import datetime
import json
import os
import re
import subprocess
import sys

FRAGMENT_DIR = "changelog.d"
NO_CHANGELOG_LABEL = "no-changelog"
# Authors that never write fragments (dependency bumps carry their own release notes).
EXEMPT_AUTHORS = ("dependabot[bot]",)
# Keep a Changelog order. Entries from fragments land under these headings.
KINDS = ("added", "changed", "deprecated", "removed", "fixed", "security")
MAX_FRAGMENT_CHARS = 2000
NAME_RE = re.compile(
    r"\A(?P<number>[0-9]+)-(?P<slug>[a-z0-9]+(?:-[a-z0-9]+)*)\.(?P<kind>[a-z]+)\.md\Z"
)
EMAIL_RE = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+\.[A-Za-z]{2,}")
VERSION_RE = re.compile(r"\A[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.]+)?\Z")
SECTION_RE = re.compile(r"^## \[(?P<name>[^\]]+)\]", re.MULTILINE)
HEADING_RE = re.compile(r"^### (?P<name>.+?)\s*$", re.MULTILINE)


def parse_name(filename):
    """Return (number, slug, kind) for a fragment file name, or None when it is malformed."""
    m = NAME_RE.match(filename)
    if not m or m.group("kind") not in KINDS:
        return None
    return int(m.group("number")), m.group("slug"), m.group("kind")


def check_text(text):
    """Return a list of problems with a fragment's text (empty when it is fine)."""
    problems = []
    if not text.strip():
        return ["is empty"]
    if len(text) > MAX_FRAGMENT_CHARS:
        problems.append(f"is longer than {MAX_FRAGMENT_CHARS} characters; keep entries short")
    if not text.endswith("\n"):
        problems.append("must end with a newline")
    if text.endswith("\n\n"):
        problems.append("must end with a single newline")
    if "\r" in text:
        problems.append("must use LF line endings")
    first = text.lstrip("\n").lstrip()
    if re.match(r"[-*+] ", first):
        problems.append("must not start with a list bullet; the compiler adds it")
    for n, line in enumerate(text.splitlines(), 1):
        if line.startswith("#"):
            problems.append(f"line {n}: headings are not allowed; the kind comes from the file name")
        if line != line.rstrip():
            problems.append(f"line {n}: trailing whitespace")
    if EMAIL_RE.search(text):
        problems.append("contains an email address")
    if re.search(r"co-authored-by|generated with", text, re.IGNORECASE):
        problems.append("contains an attribution trailer")
    return problems


def check_fragments(files):
    """Validate {filename: text}. Returns a list of 'filename: problem' strings."""
    errors = []
    for name in sorted(files):
        if name == "README.md":
            continue
        if parse_name(name) is None:
            errors.append(
                f"{name}: name must be <number>-<slug>.<kind>.md with kind one of "
                f"{', '.join(KINDS)}"
            )
            continue
        errors.extend(f"{name}: {p}" for p in check_text(files[name]))
    return errors


def fragments_added(changed):
    """Fragment paths among [(status, path)] that a PR adds or renames into changelog.d."""
    out = []
    for status, path in changed:
        if status not in ("added", "renamed"):
            continue
        head, _, name = path.rpartition("/")
        if head == FRAGMENT_DIR and name != "README.md":
            out.append(path)
    return sorted(out)


def check_pr(added, labels, author=""):
    """Rules for a PR: exactly one new fragment unless it is labelled no-changelog."""
    if NO_CHANGELOG_LABEL in set(labels):
        if added:
            return [f"has the `{NO_CHANGELOG_LABEL}` label but adds {', '.join(added)}; drop one of them"]
        return []
    if author in EXEMPT_AUTHORS:
        return []
    if len(added) == 1:
        return []
    if not added:
        return [
            f"adds no fragment. Add one file {FRAGMENT_DIR}/<number>-<slug>.<kind>.md, or ask "
            f"the owner for the `{NO_CHANGELOG_LABEL}` label when the change needs no entry"
        ]
    return [f"adds {len(added)} fragments ({', '.join(added)}); one pull request adds one"]


def sort_key(filename):
    number, slug, kind = parse_name(filename)
    return number, slug


def render_entry(text):
    """Turn fragment text into a Markdown bullet with a hanging indent for continuation lines."""
    lines = text.rstrip("\n").split("\n")
    out = ["- " + lines[0]]
    out.extend(("  " + ln) if ln else "" for ln in lines[1:])
    return "\n".join(out)


def split_unreleased(changelog):
    """Return (head, unreleased_body, rest): text before the section, its body, text from the next section."""
    m = re.search(r"^## \[Unreleased\][^\n]*\n", changelog, re.MULTILINE)
    if not m:
        raise ValueError("CHANGELOG.md has no [Unreleased] section")
    nxt = SECTION_RE.search(changelog, m.end())
    end = nxt.start() if nxt else len(changelog)
    return changelog[: m.start()], changelog[m.end() : end], changelog[end:]


def split_headings(body):
    """Split a section body into (preamble, [(heading, text)]) at `### ` headings."""
    ms = list(HEADING_RE.finditer(body))
    if not ms:
        return body, []
    parts = []
    for i, m in enumerate(ms):
        stop = ms[i + 1].start() if i + 1 < len(ms) else len(body)
        parts.append((m.group("name"), body[m.end() : stop].strip("\n")))
    return body[: ms[0].start()], parts


def build_section(version, date, legacy_body, fragments):
    """Compose the release section.

    `legacy_body` is whatever was written under [Unreleased] by hand (moved verbatim, as the old
    release step did). `fragments` is {filename: text}; each entry is appended to the heading for
    its kind, in (number, slug) order, after the legacy entries of that heading.
    """
    preamble, parts = split_headings(legacy_body)
    sections = {name: text for name, text in parts}
    order = [name for name, _ in parts]
    by_kind = {k: [] for k in KINDS}
    for name in sorted(fragments, key=sort_key):
        by_kind[parse_name(name)[2]].append(render_entry(fragments[name]))
    for kind in KINDS:
        if not by_kind[kind]:
            continue
        heading = kind.capitalize()
        entries = "\n\n".join(by_kind[kind])
        if heading in sections:
            sections[heading] = (sections[heading] + "\n\n" + entries).strip("\n")
        else:
            sections[heading] = entries
            order.append(heading)
    canonical = [k.capitalize() for k in KINDS]
    order.sort(key=lambda h: canonical.index(h) if h in canonical else len(canonical))
    out = [f"## [{version}] - {date}", ""]
    if preamble.strip():
        out += [preamble.strip("\n"), ""]
    for heading in order:
        out += [f"### {heading}", "", sections[heading], ""]
    return "\n".join(out).rstrip("\n") + "\n"


def compile_changelog(changelog, version, date, fragments):
    """Return the new CHANGELOG.md text: an empty [Unreleased] followed by the new section."""
    if not VERSION_RE.match(version):
        raise ValueError(f"not a version: {version!r}")
    if re.search(rf"^## \[{re.escape(version)}\]", changelog, re.MULTILINE):
        raise ValueError(f"CHANGELOG.md already has a section for {version}")
    head, body, rest = split_unreleased(changelog)
    if not body.strip() and not fragments:
        raise ValueError("nothing to release: [Unreleased] is empty and there are no fragments")
    section = build_section(version, date, body, fragments)
    return f"{head}## [Unreleased]\n\n{section}\n{rest}"


def read_fragments(directory):
    files = {}
    if not os.path.isdir(directory):
        return files
    for name in sorted(os.listdir(directory)):
        if name == "README.md" or name.startswith("."):
            continue
        with open(os.path.join(directory, name), encoding="utf-8", newline="") as f:
            files[name] = f.read()
    return files


def _gh(args):
    return subprocess.run(
        ["gh", *args], capture_output=True, text=True, check=True
    ).stdout


def pr_changed(repo, number):
    """[(status, path)] for a PR, from the API (renames report the new path)."""
    pages = json.loads(_gh(["api", "--paginate", "--slurp", f"repos/{repo}/pulls/{number}/files"]))
    return [(item["status"], item["filename"]) for page in pages for item in page]


def cmd_check(args):
    files = read_fragments(args.dir)
    errors = check_fragments(files)
    if args.pr:
        added = fragments_added(pr_changed(args.repo, args.pr))
        errors += [f"pull request {p}" for p in check_pr(added, json.loads(args.labels_json), args.author)]
    for e in errors:
        print(f"::error title=Changelog fragment::{e}")
    if errors:
        print(f"{len(errors)} problem(s). Convention: {FRAGMENT_DIR}/README.md")
        return 1
    print(f"changelog fragments ok ({len(files)} in the tree)")
    return 0


def cmd_compile(args):
    date = args.date or datetime.date.today().isoformat()
    files = read_fragments(args.dir)
    errors = check_fragments(files)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    with open(args.changelog, encoding="utf-8") as f:
        old = f.read()
    try:
        new = compile_changelog(old, args.version, date, files)
    except ValueError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    if args.write:
        with open(args.changelog, "w", encoding="utf-8") as f:
            f.write(new)
        for name in files:
            os.remove(os.path.join(args.dir, name))
        print(f"wrote {args.changelog}; removed {len(files)} fragment(s). Review the diff, then commit.")
    else:
        sys.stdout.write(new)
        print("dry run: nothing written; pass --write to apply", file=sys.stderr)
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dir", default=FRAGMENT_DIR)
    sub = parser.add_subparsers(dest="cmd", required=True)
    check = sub.add_parser("check", help="validate fragments (and a PR's fragment count)")
    check.add_argument("--repo")
    check.add_argument("--pr", type=int)
    check.add_argument("--labels-json", default="[]")
    check.add_argument("--author", default="")
    check.set_defaults(func=cmd_check)
    comp = sub.add_parser("compile", help="build the release section from fragments")
    comp.add_argument("--version", required=True)
    comp.add_argument("--date")
    comp.add_argument("--changelog", default="CHANGELOG.md")
    comp.add_argument("--write", action="store_true", help="rewrite CHANGELOG.md and delete the fragments")
    comp.set_defaults(func=cmd_compile)
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
