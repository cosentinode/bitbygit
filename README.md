# bitbygit

`bitbygit` is a guarded GitHub Desktop-style terminal application for managing
Git repositories from one responsive viewport.

The project is inspired by terminal tools like `lazygit` and `btop`, but the
core product goal is different: make common Git and GitHub workflows fast while
keeping risky operations visible, typed, previewed, and confirmed.

## Goals

- Switch, add, and manage multiple repositories from one terminal UI.
- View repository status, staged changes, diffs, branches, remotes, and sync
  state in one responsive viewport.
- Commit, fetch, pull, push, merge, rebase, and open pull requests with strong
  guardrails.
- Provide a command prompt that turns phrases like `commit and push and open PR`
  into a typed operation plan.
- Keep the implementation simple, secure, and scalable.

## Non-Negotiables

- Code simplicity is a must. Prefer small modules, explicit data flow, and the
  smallest correct abstraction.
- Security is a must. Prompt text is never executed as shell, secrets are not
  stored by `bitbygit`, and destructive operations are gated.
- Scalability is a must. Core boundaries should support more repositories,
  providers, workflows, and UI panels without rewrites.

## Current Status

The repository is in Phase 1 architecture and guardrails work. See
[`docs/mvp-phases.md`](docs/mvp-phases.md) for the implementation roadmap.

## Development

Prerequisites:

- Rust 1.85 or newer.
- `git` available on `PATH`.
- `gh` is optional for future GitHub workflows.

Run local checks:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

Or run:

```sh
./scripts/setup-dev.sh
```

## Branching

The default development branch is `develop`. Feature work should use a focused
branch and open a pull request back to `develop`.
