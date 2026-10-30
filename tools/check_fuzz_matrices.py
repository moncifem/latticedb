#!/usr/bin/env python3
"""Fail when a fuzz target in fuzz/Cargo.toml is missing from a scheduled fuzz matrix or has no seeds."""
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MATRICES = ('nightly.yml', 'weekly.yml', 'fuzz-24h.yml')
# `trivial` is the build smoke target; it has no decoder behind it and is not fuzzed on a schedule.
EXEMPT = frozenset({'trivial'})
TARGET_LINE = re.compile(r'^\s*target:\s*\[([^\]]*)\]\s*(?:#.*)?$', re.MULTILINE)


def cargo_targets(text):
    names = [entry['name'] for entry in tomllib.loads(text).get('bin', [])]
    if len(names) != len(set(names)):
        raise ValueError('duplicate fuzz bin name in fuzz/Cargo.toml')
    return set(names) - EXEMPT


def matrix_targets(text, label):
    lines = TARGET_LINE.findall(text)
    if len(lines) != 1:
        raise ValueError(f'{label}: expected exactly one inline `target: [...]` matrix, found {len(lines)}')
    names = [item.strip().strip('\'"') for item in lines[0].split(',') if item.strip()]
    if len(names) != len(set(names)):
        raise ValueError(f'{label}: duplicate target in matrix')
    return set(names)


def problems(cargo_text, workflows, seeded=None):
    """Return one message per mismatch; `workflows` maps a label to the workflow file text.

    `seeded` is the set of target names whose `fuzz/seeds/<target>/` directory holds at least one
    file; when it is given, every scheduled target must be in it. The scheduled jobs pass that
    directory to cargo-fuzz, so a target without seeds starts from an empty corpus every run.
    """
    expected = cargo_targets(cargo_text)
    found = []
    if seeded is not None:
        for name in sorted(expected - seeded):
            found.append(f'fuzz/seeds/{name}: no seed files for fuzz target `{name}`')
    for label, text in workflows.items():
        listed = matrix_targets(text, label)
        for name in sorted(expected - listed):
            found.append(f'{label}: fuzz target `{name}` is not in the matrix')
        for name in sorted(listed - expected - EXEMPT):
            found.append(f'{label}: matrix lists `{name}`, which is not a bin in fuzz/Cargo.toml')
    return found


def seeded_targets(seeds_dir):
    """Names of the directories under `seeds_dir` that hold at least one regular file."""
    return {d.name for d in seeds_dir.iterdir() if d.is_dir() and any(f.is_file() for f in d.iterdir())}


def main():
    cargo = (ROOT / 'fuzz/Cargo.toml').read_text()
    workflows = {name: (ROOT / '.github/workflows' / name).read_text() for name in MATRICES}
    found = problems(cargo, workflows, seeded_targets(ROOT / 'fuzz/seeds'))
    for message in found:
        print(message, file=sys.stderr)
    if found:
        return 1
    print(f'all {len(cargo_targets(cargo))} fuzz targets are in {", ".join(MATRICES)} and have seeds')
    return 0


if __name__ == '__main__':
    sys.exit(main())
