#!/usr/bin/env python3
"""Stage the reviewed documents for the mdBook site and generate its SUMMARY.md.

docs/ stays the single source. Nothing generated is written inside docs/: the script copies the pages
into a staging directory (default target/docs-src), rewrites their relative links for the new layout
and writes SUMMARY.md there. It exits non-zero when a relative link does not resolve.

Staged pages: the repository README (as "Overview") and CONTRIBUTING.md, every Markdown file under
docs/ and the Markdown files under formats/. Excluded: docs/site (build output of the older single-page
viewer), docs/assets (copied as files, never as pages), SUMMARY.md, formats/corpus and any file whose
first line is a "generated" HTML comment.

Link rules, applied to inline links, images, reference definitions and href/src attributes outside code:
  - external links (a scheme such as https:) and pure fragments (#anchor) are left alone;
  - a relative link to a staged page or asset is rewritten relative to the staged layout;
  - a relative link to another file or directory that exists in the repository becomes an absolute
    GitHub URL (blob or tree on the main branch);
  - anything else is an error. Fragments are not validated.

Stdlib only. Tests: tools/test_docs_summary.py.
"""
from __future__ import annotations

import argparse
import os
import posixpath
import re
import shutil
import sys
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import quote, unquote

REPO_URL = "https://github.com/moncifem/latticedb"
BRANCH = "main"

# (section title, key) in site order.
SECTIONS = [
    ("Overview", "overview"),
    ("Roadmap", "roadmap"),
    ("Architecture", "architecture"),
    ("Specification", "spec"),
    ("Gates", "gates"),
    ("Architecture decision records", "adr"),
    ("Requests for comments", "rfc"),
    ("Formats", "formats"),
    ("Policies", "policies"),
    ("Guides", "guides"),
]

TITLE_OVERRIDES = {
    "README.md": "Overview",
    "CONTRIBUTING.md": "How to contribute",
    "formats/README.md": "Formats overview",
}

ARCHITECTURE_PAGES = ["docs/ARCHITECTURE.md", "docs/ENGINEERING.md", "docs/sql-subset.md"]
POLICY_PAGES = ["docs/review-policy.md", "docs/dependency-policy.md"]
OVERVIEW_PAGES = ["README.md", "docs/glossary.md"]

EXCLUDED_DIRS = ("docs/site/", "formats/corpus/")
GENERATED_MARKER = "<!-- generated"


@dataclass(frozen=True)
class Page:
    src: str  # repository-relative, posix
    dest: str  # relative to the staging directory, posix
    title: str
    section: str


def natural_key(name: str):
    """Sort key: runs of digits compare as numbers, digits before letters, extension ignored."""
    stem = name[:-3] if name.endswith(".md") else name
    parts = re.split(r"(\d+)", stem)
    return [(0, int(p)) if p.isdigit() else (1, p.casefold()) for p in parts if p != ""]


def dest_for(src: str) -> str:
    if src == "README.md":
        return "overview.md"
    if src == "CONTRIBUTING.md":
        return "contributing.md"
    if src.startswith("docs/"):
        return src[len("docs/"):]
    return src  # formats/...


def first_heading(text: str) -> str | None:
    in_fence = False
    for line in text.splitlines():
        if re.match(r"^\s*(```|~~~)", line):
            in_fence = not in_fence
            continue
        if not in_fence and line.startswith("# "):
            return line[2:].strip()
    return None


def is_generated(text: str) -> bool:
    for line in text.splitlines():
        if line.strip():
            return line.strip().startswith(GENERATED_MARKER)
    return False


def section_for(src: str) -> str:
    if src in OVERVIEW_PAGES:
        return "overview"
    if src == "docs/roadmap.md":
        return "roadmap"
    if src in ARCHITECTURE_PAGES:
        return "architecture"
    if src.startswith("docs/spec/"):
        return "spec"
    if src.startswith("docs/gates/") or src.startswith("docs/evidence/"):
        return "gates"
    if src.startswith("docs/adr/"):
        return "adr"
    if src.startswith("docs/rfc/"):
        return "rfc"
    if src.startswith("formats/"):
        return "formats"
    if src.startswith("docs/policies/") or src in POLICY_PAGES:
        return "policies"
    return "guides"


def order_key(page_src: str):
    """Order inside a section. Pinned lists keep their listed order; directory indexes come first."""
    section = section_for(page_src)
    fixed = {
        "overview": OVERVIEW_PAGES,
        "architecture": ARCHITECTURE_PAGES,
        "policies": [p for p in POLICY_PAGES],
    }
    if section in fixed and page_src in fixed[section]:
        return (0, fixed[section].index(page_src), [])
    name = posixpath.basename(page_src)
    group = 1 if section == "gates" and page_src.startswith("docs/evidence/") else 0
    # Policy directory pages come before the pinned policy pages' successors; keep it simple: pinned
    # first, then the rest naturally sorted.
    return (1 + group, 0 if name == "README.md" else 1, natural_key(name))


def discover(root: Path) -> list[Page]:
    candidates: list[str] = []
    for top in ("README.md", "CONTRIBUTING.md"):
        if (root / top).is_file() and not (root / top).is_symlink():
            candidates.append(top)
    for base in ("docs", "formats"):
        for path in sorted((root / base).rglob("*.md")):
            if path.is_symlink() or not path.is_file():
                continue
            candidates.append(path.relative_to(root).as_posix())
    pages: list[Page] = []
    for src in candidates:
        if any(src.startswith(d) for d in EXCLUDED_DIRS):
            continue
        if posixpath.basename(src) == "SUMMARY.md":
            continue
        text = (root / src).read_text(encoding="utf-8")
        if is_generated(text):
            continue
        title = TITLE_OVERRIDES.get(src) or first_heading(text) or posixpath.basename(src)[:-3]
        if posixpath.basename(src) == "TEMPLATE.md":
            title = f"Template for {posixpath.basename(posixpath.dirname(src))}"
        pages.append(Page(src, dest_for(src), title, section_for(src)))
    order = {key: i for i, (_, key) in enumerate(SECTIONS)}
    pages.sort(key=lambda p: (order[p.section], order_key(p.src)))
    return pages


# --- links ---------------------------------------------------------------------------------------

_SCHEME = re.compile(r"^[A-Za-z][A-Za-z0-9+.\-]*:")
_INLINE = re.compile(r"(\]\(\s*)([^)\s<>]+)((?:\s+\"[^\"]*\")?\s*\))")
_INLINE_ANGLE = re.compile(r"(\]\(\s*<)([^>\n]+)(>(?:\s+\"[^\"]*\")?\s*\))")
_REFDEF = re.compile(r"^(\s{0,3}\[[^\]]+\]:\s*<?)(\S+?)(>?\s*(?:\"[^\"]*\")?\s*)$")
_ATTR = re.compile(r"""((?:href|src)\s*=\s*["'])([^"']+)(["'])""")
_CODE_SPAN = re.compile(r"(`+)(?:(?!\1).)+?\1")


def _mask_code(line: str) -> str:
    return _CODE_SPAN.sub(lambda m: " " * len(m.group(0)), line)


def rewrite_links(text: str, fix) -> str:
    """Apply fix(target) -> str to every link target outside fenced and inline code."""
    out = []
    in_fence = False
    for line in text.split("\n"):
        if re.match(r"^\s*(```|~~~)", line):
            in_fence = not in_fence
            out.append(line)
            continue
        if in_fence:
            out.append(line)
            continue
        masked = _mask_code(line)
        edits = []  # (start, end, new)
        for rx in (_INLINE, _INLINE_ANGLE, _ATTR):
            for m in rx.finditer(masked):
                edits.append((m.start(2), m.end(2), fix(line[m.start(2):m.end(2)])))
        m = _REFDEF.match(masked)
        if m:
            edits.append((m.start(2), m.end(2), fix(line[m.start(2):m.end(2)])))
        for start, end, new in sorted(edits, reverse=True):
            line = line[:start] + new + line[end:]
        out.append(line)
    return "\n".join(out)


def split_target(target: str) -> tuple[str, str]:
    path, sep, frag = target.partition("#")
    return unquote(path), (sep + frag if sep else "")


def github_url(root: Path, repo_path: str) -> str:
    kind = "tree" if (root / repo_path).is_dir() else "blob"
    return f"{REPO_URL}/{kind}/{BRANCH}/{quote(repo_path)}"


def github_url_for(repo_path: str) -> str:
    """GitHub URL for a repository path without touching the filesystem (a path without a dot is a tree)."""
    kind = "blob" if posixpath.splitext(repo_path)[1] else "tree"
    return f"{REPO_URL}/{kind}/{BRANCH}/{quote(repo_path)}"


def resolve_repo_path(src: str, path: str) -> str | None:
    """Repository-relative path of a relative link written in `src`, or None if it leaves the repo."""
    joined = posixpath.normpath(posixpath.join(posixpath.dirname(src), path))
    if joined.startswith("../") or joined == "..":
        return None
    return "" if joined == "." else joined


class LinkFixer:
    def __init__(self, root: Path, pages: list[Page], assets: set[str], errors: list[str]):
        self.root = root
        self.staged = {p.src: p.dest for p in pages}
        self.assets = assets  # repo paths of copied asset files
        self.errors = errors

    def for_page(self, page: Page):
        def fix(target: str) -> str:
            if not target or target.startswith("#") or _SCHEME.match(target) or target.startswith("//"):
                return target
            path, frag = split_target(target)
            if path.startswith("/"):
                self.errors.append(f"{page.src}: root-absolute link {target!r}")
                return target
            repo_path = resolve_repo_path(page.src, path)
            if repo_path is None:
                self.errors.append(f"{page.src}: link {target!r} leaves the repository")
                return target
            dest = self.staged.get(repo_path)
            if dest is None and repo_path in self.assets:
                dest = repo_path[len("docs/"):]
            if dest is not None:
                rel = posixpath.relpath(dest, posixpath.dirname(page.dest) or ".")
                return quote(rel) + frag
            if repo_path and (self.root / repo_path).exists():
                return github_url(self.root, repo_path) + frag
            self.errors.append(f"{page.src}: broken link {target!r} (no such file {repo_path!r})")
            return target

        return fix


# --- staging -------------------------------------------------------------------------------------

def collect_assets(root: Path) -> set[str]:
    base = root / "docs" / "assets"
    if not base.is_dir():
        return set()
    return {p.relative_to(root).as_posix() for p in sorted(base.rglob("*")) if p.is_file() and not p.is_symlink()}


def md_escape(title: str) -> str:
    return title.replace("\\", "\\\\").replace("[", "\\[").replace("]", "\\]")


def render_summary(pages: list[Page]) -> str:
    lines = ["# Summary", ""]
    for title, key in SECTIONS:
        members = [p for p in pages if p.section == key]
        if not members:
            continue
        lines.append(f"# {title}")
        lines.append("")
        for p in members:
            lines.append(f"- [{md_escape(p.title)}]({p.dest})")
        lines.append("")
    return "\n".join(lines)


def stage(root: Path, out: Path) -> list[str]:
    """Build the staging directory. Returns the list of errors (empty on success)."""
    root = root.resolve()
    out = out.resolve()
    if out == root or root not in out.parents:
        return [f"refusing to use {out} as staging directory: it must be inside {root}"]
    pages = discover(root)
    assets = collect_assets(root)
    errors: list[str] = []
    fixer = LinkFixer(root, pages, assets, errors)
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    for page in pages:
        text = (root / page.src).read_text(encoding="utf-8")
        text = rewrite_links(text, fixer.for_page(page))
        target = out / page.dest
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")
    for asset in sorted(assets):
        target = out / asset[len("docs/"):]
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(root / asset, target)
    (out / "SUMMARY.md").write_text(render_summary(pages), encoding="utf-8")
    return errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--root", default=".", help="repository root (default: current directory)")
    parser.add_argument("--out", default="target/docs-src", help="staging directory, inside the root")
    args = parser.parse_args(argv)
    root = Path(args.root).resolve()
    out = Path(args.out)
    if not out.is_absolute():
        out = root / out
    errors = stage(root, out)
    if errors:
        for e in errors:
            print(f"error: {e}", file=sys.stderr)
        print(f"{len(errors)} broken link(s)", file=sys.stderr)
        return 1
    count = sum(1 for _ in out.rglob("*.md")) - 1
    print(f"staged {count} pages in {os.path.relpath(out, root)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
