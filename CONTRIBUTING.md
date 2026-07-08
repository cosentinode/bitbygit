# Contributing

Work in `bitbygit` should be issue-driven and small enough to review safely.

## Workflow


1. Pick an open GitHub issue.
2. Branch from `develop` with a focused name, such as
   `issue-2-phase-0-foundation`.
3. Keep the change scoped to the issue.
4. Run format, lint, test, and build checks locally.
5. Open a pull request to `develop`.

## Engineering Standards

- Keep code simple and direct.
- Prefer typed operations over stringly-typed command construction.
- Never execute user prompt text as shell.
- Do not store GitHub tokens or Git credentials.
- Add tests when behavior changes.
- Keep docs aligned with architecture and phase issues.

## Review Standard

Reviews should prioritize correctness, safety, maintainability, and whether the
change preserves the product guardrails.
