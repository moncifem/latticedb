#!/usr/bin/env python3
"""Fail when a fuzz target in fuzz/Cargo.toml is missing from a scheduled fuzz matrix, has no seeds or
has no libFuzzer limits in fuzz/settings.toml, or when a workflow runs `fuzz run` without those limits.

`flags <target>` prints the `-timeout=`, `-rss_limit_mb=` and `-max_len=` arguments the workflows pass.
"""
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MATRICES = ('nightly.yml', 'weekly.yml', 'fuzz-24h.yml')
# `trivial` is the build smoke target; it has no decoder behind it and is not fuzzed on a schedule.
EXEMPT = frozenset({'trivial'})
# Largest max_len an entry may ask for: a bigger value slows every input for no gain.
MAX_LEN_CEILING = 4 * 1024 * 1024
# The shortest scheduled run (nightly, 600 s) must be able to report a hang.
TIMEOUT_CEILING = 600
FLAGS_COMMAND = 'check_fuzz_matrices.py flags'
FLAGS_VARIABLE = re.compile(r'\$\{?FUZZ_FLAGS\}?')
FUZZ_RUN = re.compile(r'fuzz run\b')
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


def settings_problems(expected, settings):
    """One message per target without usable limits, and per unusable default."""
    found = []
    defaults = settings.get('defaults', {})
    timeout = defaults.get('timeout_secs')
    if not isinstance(timeout, int) or isinstance(timeout, bool) or not 1 <= timeout <= TIMEOUT_CEILING:
        found.append(f'fuzz/settings.toml: [defaults] timeout_secs must be an integer from 1 to {TIMEOUT_CEILING}')
    rss = defaults.get('rss_limit_mb')
    if not isinstance(rss, int) or isinstance(rss, bool) or rss < 1:
        found.append('fuzz/settings.toml: [defaults] rss_limit_mb must be a positive integer')
    targets = settings.get('targets', {})
    for name in sorted(expected - set(targets)):
        found.append(f'fuzz/settings.toml: no [targets.{name}] entry for fuzz target `{name}`')
    for name in sorted(set(targets) - expected):
        found.append(f'fuzz/settings.toml: [targets.{name}] is not a fuzz target in fuzz/Cargo.toml')
    for name in sorted(set(targets) & expected):
        entry = targets[name]
        max_len = entry.get('max_len')
        if not isinstance(max_len, int) or isinstance(max_len, bool) or not 1 <= max_len <= MAX_LEN_CEILING:
            found.append(f'fuzz/settings.toml: [targets.{name}] max_len must be an integer from 1 to {MAX_LEN_CEILING}')
        basis = entry.get('basis')
        if not isinstance(basis, str) or not basis.strip():
            found.append(f'fuzz/settings.toml: [targets.{name}] needs a `basis` that says where max_len comes from')
    return found


def command_after(text, start):
    """The shell command that begins at `start`: its line and every backslash continuation."""
    lines = text[start:].split('\n')
    joined = []
    for line in lines:
        joined.append(line.rstrip('\\'))
        if not line.endswith('\\'):
            break
    return ' '.join(joined)


def without_comments(text):
    """Drop whole-line and trailing ` #` comments, so a mention of $FUZZ_FLAGS in a comment proves nothing."""
    return '\n'.join(re.sub(r'(^|\s)#.*$', '', line) for line in text.split('\n'))


def workflow_flag_problems(label, text):
    """Every `fuzz run` command must pass $FUZZ_FLAGS, and the workflow must build it from the settings."""
    text = without_comments(text)
    runs = [m.start() for m in FUZZ_RUN.finditer(text)]
    if not runs:
        return []
    found = []
    if FLAGS_COMMAND not in text:
        found.append(f'{label}: runs `fuzz run` but never builds the limits with `{FLAGS_COMMAND}`')
    for start in runs:
        if not FLAGS_VARIABLE.search(command_after(text, start)):
            line = text.count('\n', 0, start) + 1
            found.append(f'{label}:{line}: `fuzz run` does not pass $FUZZ_FLAGS (-timeout, -rss_limit_mb, -max_len)')
    return found


def flags_for(settings, target):
    """The libFuzzer arguments for `target`; raises KeyError for a target without an entry."""
    defaults = settings['defaults']
    entry = settings['targets'][target]
    return f"-timeout={defaults['timeout_secs']} -rss_limit_mb={defaults['rss_limit_mb']} -max_len={entry['max_len']}"


def problems(cargo_text, workflows, seeded=None, settings=None, other_workflows=None):
    """Return one message per mismatch; `workflows` maps a label to the workflow file text.

    `seeded` is the set of target names whose `fuzz/seeds/<target>/` directory holds at least one
    file; when it is given, every scheduled target must be in it. The scheduled jobs pass that
    directory to cargo-fuzz, so a target without seeds starts from an empty corpus every run.
    """
    expected = cargo_targets(cargo_text)
    found = []
    if settings is not None:
        found.extend(settings_problems(expected, settings))
    if seeded is not None:
        for name in sorted(expected - seeded):
            found.append(f'fuzz/seeds/{name}: no seed files for fuzz target `{name}`')
    for label, text in workflows.items():
        listed = matrix_targets(text, label)
        for name in sorted(expected - listed):
            found.append(f'{label}: fuzz target `{name}` is not in the matrix')
        for name in sorted(listed - expected - EXEMPT):
            found.append(f'{label}: matrix lists `{name}`, which is not a bin in fuzz/Cargo.toml')
        if settings is not None:
            found.extend(workflow_flag_problems(label, text))
    if settings is not None:
        for label, text in (other_workflows or {}).items():
            if label not in workflows:
                found.extend(workflow_flag_problems(label, text))
    return found


def seeded_targets(seeds_dir):
    """Names of the directories under `seeds_dir` that hold at least one regular file."""
    return {d.name for d in seeds_dir.iterdir() if d.is_dir() and any(f.is_file() for f in d.iterdir())}


def load_settings():
    return tomllib.loads((ROOT / 'fuzz/settings.toml').read_text())


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if argv and argv[0] == 'flags':
        if len(argv) != 2:
            print('usage: check_fuzz_matrices.py flags <target>', file=sys.stderr)
            return 2
        try:
            print(flags_for(load_settings(), argv[1]))
        except KeyError:
            print(f'fuzz/settings.toml has no limits for `{argv[1]}`', file=sys.stderr)
            return 1
        return 0
    cargo = (ROOT / 'fuzz/Cargo.toml').read_text()
    workflows = {name: (ROOT / '.github/workflows' / name).read_text() for name in MATRICES}
    others = {p.name: p.read_text() for p in sorted((ROOT / '.github/workflows').glob('*.yml'))
              if FUZZ_RUN.search(without_comments(p.read_text()))}
    found = problems(cargo, workflows, seeded_targets(ROOT / 'fuzz/seeds'), load_settings(), others)
    for message in found:
        print(message, file=sys.stderr)
    if found:
        return 1
    print(f'all {len(cargo_targets(cargo))} fuzz targets are in {", ".join(MATRICES)} and have seeds and limits')
    return 0


if __name__ == '__main__':
    sys.exit(main())
