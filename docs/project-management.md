# How the project is tracked

Everything about planning lives on GitHub, so it is visible to anyone.

| Where | What |
|---|---|
| [Milestones](https://github.com/moncifem/latticedb/milestones) | One per delivery phase (0 to 12). Progress is the share of closed issues and pull requests. |
| [Issues](https://github.com/moncifem/latticedb/issues) | One **epic** per phase with its exit criteria as a checklist, and **work packages** as sub-issues of the epic. Each links to requirement ids in `docs/spec`. |
| [Project board](https://github.com/users/moncifem/projects/1) | All issues and pull requests with Status, Phase, Area and Kind fields. |
| [Pull requests](https://github.com/moncifem/latticedb/pulls) | Every change, small and linked to its work package with `Part of #N` or `Closes #N`. |
| [Discussions](https://github.com/moncifem/latticedb/discussions) | Questions and ideas. |
| `docs/gates/` | The honest status of each phase gate, updated as evidence lands. |

## Labels

`type: *` says what a change is (feature, bug, test, docs, ci, refactor, spike, epic). `area: *` names the crate or
directory. `phase-N` ties an item to a phase. `priority: *` and `status: *` order and flag work.
`invariant` marks changes that can affect I1 to I14, and `format-change` marks on-disk or wire changes that need
an RFC and an ADR. The labeler workflow adds `area` and `type` labels from the changed paths.

## Flow

1. A phase starts when the previous gate passes (`docs/spec/README.md`).
2. Work packages are sub-issues of the phase epic and are closed when all their requirements are verified.
3. Work happens on a branch named `phase-N/<topic>` (or `fix/`, `chore/`, `docs/`), with Conventional Commits.
4. A pull request needs green checks, links its issue, and states which invariants it can affect. Branch
   protection on `main` requires the checks and a linear history.
5. The phase epic is closed when every exit criterion is checked and the gate review is recorded in `docs/gates/`.
