# tools

Policy files read by xtask, such as `allow-banned.toml`.

`merge-pr.sh` merges PRs serially and safely (see `docs/merging.md`); `test_merge_pr.sh` tests it
against a fake `gh`. `branch-protection.json` is proposed configuration, not proof of live protection. The helper reads
live strict/admin/check-context protection and refuses real updates/merges until the owner applies it.
