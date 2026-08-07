# Contributing — Memento RS

## Branch strategy: feature-branch-chain

The MVP launch change (`mvp-launch`) is delivered as 12 SDD batches, each
mapped to one reviewable PR in a chain:

- `main` — protected default branch. Only the tracker merges here.
- `feature/mvp-launch` — tracker branch for the `mvp-launch` change. It
  accumulates the final integration; PR #1 targets it.
- `feature/mvp-launch-batch-<N>` — one branch per SDD batch. PR #N targets
  the previous PR's branch (PR #1 → tracker, PR #2 → batch-1, ...), keeping
  each review diff focused on a single batch slice.

Each batch carries its own verification (per-batch focused test commands)
and its own rollback boundary.

## Commits

- Conventional commits only: `feat:`, `fix:`, `chore:`, `docs:`, `ci:`,
  `refactor:`, `test:`, with an optional scope.
- One work unit per commit; tests and docs ship with the code they belong to.
- No AI attribution (`Co-Authored-By` or similar) in commit bodies.

_Stub — expand as conventions settle._
