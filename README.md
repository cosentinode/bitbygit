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

The repository has merged through Phase 10 deterministic prompt parser work as
part of [#12](https://github.com/cosentinode/bitbygit/issues/12), including
golden parser tests, manual planner parity, and safe non-provider multi-step
prompts. Pull request creation from `open pr` remains Phase 11 work in
[#13](https://github.com/cosentinode/bitbygit/issues/13). See
[`docs/mvp-phases.md`](docs/mvp-phases.md) for the implementation roadmap.

## Configuration

User policy is defined in `config.toml`; see
[`docs/configuration.md`](docs/configuration.md) for path resolution, the strict
schema, and safe fallback behavior.

## Installation

See [`docs/installation.md`](docs/installation.md) for supported release
archives, checksum verification, PATH setup, and source build instructions.
Package-manager distribution is not currently available.

## Development

Prerequisites:

- Rust 1.85 or newer.
- Git 2.42 or newer available on `PATH` (required for guarded conflict recovery).
- `gh` is optional and required only for GitHub-specific workflows.

Run local checks:

```sh
bash scripts/test-ci-workflow.sh
bash scripts/test-release-workflow.sh
bash scripts/test-installation-docs.sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked --workspace
```

The Bash installation validator runs on Linux and macOS without PowerShell.
Windows contributors can run the platform-specific installation validation with
`pwsh -NoProfile -File scripts/test-installation-docs.ps1`; CI runs that command
on a Windows runner. The validator uses temporary process-scoped PATH state and
never changes the persistent user PATH.

Or run:

```sh
./scripts/setup-dev.sh
```

## Branching

The default development branch is `develop`. Feature work should use a focused
branch and open a pull request back to `develop`.
