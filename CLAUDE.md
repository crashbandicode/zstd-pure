# CLAUDE.md — zstd-pure

This project has a full operating guide in **[AGENTS.md](AGENTS.md)** — read it
before changing anything. It is the source of truth for the non-negotiable
invariants (`#![forbid(unsafe_code)]`, `no_std + alloc`, `thiserror`-only,
MSRV 1.81), the no-regression gates, the ironclad-test bar for the "stable" flag,
how to chunk large tasks across context compactions, and the code layout. Keep
both it and `AGENTSSUMMARY.md` current.

## Worktree-per-feature (parallel agents)

Do each feature in its **own `git worktree` on its own branch** so multiple agents
can work different features in parallel without colliding on `main` or each other's
checkouts:

- `git worktree add ../zstd-pure-<feature> -b wip/<feature>` — create it.
- Do all of that feature's work and commits inside that worktree.
- `git worktree remove ../zstd-pure-<feature>` once the branch is merged.
- Never run two features out of the same checkout.
- The no-regression gates and ironclad-test bar (see AGENTS.md §1, §5) apply per
  worktree before merge.

## Shell & git conventions

- Shell is PowerShell (`pwsh`), **one logical command per call**; `python3`;
  `rg --color=never`.
- Commit each green chunk; **do NOT push without explicit permission.**
- Multi-line commit messages via `git commit -F <tempfile>`.

See the global `~/CLAUDE.md` for environment-wide PowerShell/permissions defaults.
