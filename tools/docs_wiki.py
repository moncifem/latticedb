#!/usr/bin/env python3
"""Generate the small read-only wiki mirror from the reviewed documents.

Writes a fixed set of Markdown pages into an output directory; the wiki-sync workflow copies them into
the repository's wiki git repository. Every page starts with a banner saying where to edit. Relative
links are rewritten: links to another mirrored page become wiki links, every other link becomes an
absolute GitHub URL. A link that does not resolve is an error (exit 1).

Pages: Home (README), Roadmap, Architecture overview (the first sections of ARCHITECTURE.md),
How to contribute, Glossary, Gates and board (generated), plus _Sidebar.

Stdlib only. Tests: tools/test_docs_wiki.py.
"""
from __future__ import annotations

import argparse
import posixpath
import re
import shutil
import sys
from pathlib import Path

import docs_summary as ds

BANNER = "Generated from docs/ in the main repository, edit there."
BOARD_URL = "https://github.com/users/moncifem/projects/1"
SITE_URL = "https://moncifem.github.io/latticedb/"
ARCH_SECTIONS_KEPT = 4  # level-2 sections of ARCHITECTURE.md kept in the overview

# wiki page name -> source file (None: generated)
PAGES = {
    "Home": "README.md",
    "Roadmap": "docs/roadmap.md",
    "Architecture-overview": "docs/ARCHITECTURE.md",
    "How-to-contribute": "CONTRIBUTING.md",
    "Glossary": "docs/glossary.md",
    "Gates-and-board": None,
}
LABELS = {
    "Home": "Home",
    "Roadmap": "Roadmap",
    "Architecture-overview": "Architecture overview",
    "How-to-contribute": "How to contribute",
    "Glossary": "Glossary",
    "Gates-and-board": "Gates and board",
}


def banner(source: str) -> str:
    url = ds.github_url_for(source)
    return f"> **{BANNER}** Source: [`{source}`]({url}). Changes made here are overwritten.\n\n"


def strip_leading_images(text: str) -> str:
    """Drop lines before the first heading when they are only images (the README banner)."""
    lines = text.split("\n")
    for i, line in enumerate(lines):
        if line.startswith("# "):
            head = [x for x in lines[:i] if x.strip() and not re.match(r"^\s*!\[[^\]]*\]\([^)]*\)\s*$", x)]
            return "\n".join(head + lines[i:]) if head else "\n".join(lines[i:])
    return text


def architecture_excerpt(text: str) -> str:
    """Everything before the (ARCH_SECTIONS_KEPT + 1)th level-2 heading outside code fences."""
    out, seen, in_fence = [], 0, False
    for line in text.split("\n"):
        if re.match(r"^\s*(```|~~~)", line):
            in_fence = not in_fence
        if not in_fence and line.startswith("## "):
            seen += 1
            if seen > ARCH_SECTIONS_KEPT:
                break
        out.append(line)
    return "\n".join(out).rstrip() + "\n"


def make_fixer(root: Path, source: str, wiki_of: dict[str, str], errors: list[str]):
    def fix(target: str) -> str:
        if not target or target.startswith("#") or ds._SCHEME.match(target) or target.startswith("//"):
            return target
        path, frag = ds.split_target(target)
        if path.startswith("/"):
            errors.append(f"{source}: root-absolute link {target!r}")
            return target
        repo_path = ds.resolve_repo_path(source, path)
        if repo_path is None:
            errors.append(f"{source}: link {target!r} leaves the repository")
            return target
        if repo_path in wiki_of:
            return wiki_of[repo_path] + frag
        if repo_path and (root / repo_path).exists():
            return ds.github_url(root, repo_path) + frag
        errors.append(f"{source}: broken link {target!r} (no such file {repo_path!r})")
        return target

    return fix


def gates_page(root: Path) -> str:
    lines = [
        "# Gates and board",
        "",
        "Phase gates record what is measured and what is still open. Only the project owner signs a gate.",
        "",
        "## Gate records",
        "",
    ]
    gate_files = sorted((root / "docs" / "gates").glob("*.md"), key=lambda p: ds.natural_key(p.name))
    for path in gate_files:
        rel = path.relative_to(root).as_posix()
        title = ds.first_heading(path.read_text(encoding="utf-8")) or path.stem
        lines.append(f"- [{ds.md_escape(title)}]({ds.github_url(root, rel)})")
    lines += [
        "",
        "## Tracking",
        "",
        f"- [Project board]({BOARD_URL})",
        f"- [Milestones]({ds.REPO_URL}/milestones)",
        f"- [Open issues]({ds.REPO_URL}/issues)",
        f"- [Open pull requests]({ds.REPO_URL}/pulls)",
        "",
        "## Documentation site",
        "",
        f"The full documents are published as a site at {SITE_URL} once the repository owner has enabled "
        "GitHub Pages; until then read them in the repository under "
        f"[`docs/`]({ds.github_url(root, 'docs')}).",
        "",
    ]
    return "\n".join(lines)


def generate(root: Path) -> tuple[dict[str, str], list[str]]:
    """Return ({file name: content}, errors)."""
    errors: list[str] = []
    wiki_of = {src: name for name, src in PAGES.items() if src}
    files: dict[str, str] = {}
    for name, src in PAGES.items():
        if src is None:
            body = gates_page(root)
            files[f"{name}.md"] = banner("docs/gates") + body
            continue
        text = (root / src).read_text(encoding="utf-8")
        if name == "Home":
            text = strip_leading_images(text)
        if name == "Architecture-overview":
            text = architecture_excerpt(text)
            text += (
                f"\nThis is the opening of the design document. Read the whole of it in "
                f"[`{src}`]({ds.github_url(root, src)}).\n"
            )
        text = ds.rewrite_links(text, make_fixer(root, src, wiki_of, errors))
        files[f"{name}.md"] = banner(src) + text.rstrip("\n") + "\n"
    side = ["**Lattice**", ""]
    for name in PAGES:
        side.append(f"- [{LABELS[name]}]({name})")
    side += ["", f"[{BANNER[:-1]}]({ds.github_url(root, 'docs')})", ""]
    files["_Sidebar.md"] = "\n".join(side)
    return files, errors


def write(out: Path, files: dict[str, str]) -> None:
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    for name, content in files.items():
        (out / name).write_text(content, encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--root", default=".")
    parser.add_argument("--out", required=True, help="directory to (re)create with the generated pages")
    args = parser.parse_args(argv)
    root = Path(args.root).resolve()
    files, errors = generate(root)
    if errors:
        for e in errors:
            print(f"error: {e}", file=sys.stderr)
        return 1
    write(Path(args.out), files)
    print(f"wrote {len(files)} wiki pages to {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
