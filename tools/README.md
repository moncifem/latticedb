# tools

Policy files read by xtask, such as `allow-banned.toml`.

`changelog.py` validates and compiles the fragments in `changelog.d/` (see `changelog.d/README.md`).

`merge-pr.sh` merges PRs serially and safely (see `docs/merging.md`); `test_merge_pr.sh` tests it
against a fake `gh`. `branch-protection.json` is proposed configuration, not proof of live protection. The helper reads
live strict/admin/check-context protection and refuses real updates/merges until the owner applies it.

`docs_summary.py` stages the documents for the mdBook site and checks their relative links; `docs_wiki.py` generates the
wiki mirror pages (see `docs/site-setup.md`). Tests: `python3 -m unittest discover -s tools -p 'test_docs_*.py'`.

`merge_queue.py` is the serial merge queue for batches: update, wait for all checks, re-run failures once,
merge or park, with a JSON status file and a log; `rebase_pr.py` rebases PR branches in a private worktree,
resolving only prose list conflicts. `merge-pr.sh` stays the tool for one or a few PRs. Both are described in
`docs/merging.md`. Tests use fake `gh` and `cargo` scripts and synthetic git repositories, never GitHub:
`python3 -m unittest discover -s tools -p 'test_merge_queue.py'` and `-p 'test_rebase_pr.py'`.
