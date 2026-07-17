# Changelog

All notable changes to `bitbygit` will be documented in this file.

The format follows the spirit of Keep a Changelog, and this project uses
semantic versioning once releases begin.

## Unreleased

- Established the initial repository foundation.
- Documented the product architecture, guardrail policy, command planner, and
  system Git ADR.
- Added the initial typed system Git command layer with porcelain v2 status
  parsing and temporary-repository tests.
- Added the initial local store for registered repositories, active/recent state,
  and audit entries.
- Added the initial responsive terminal UI shell with keyboard and mouse focus
  handling.
- Added typed staging/diff operations and wired the TUI status/details panels to
  changed files in the current repository.
- Added a guarded prompt-driven commit plan for staged changes using typed Git
  commit execution and audit entries.
- Added typed fetch, pull, pull-rebase, push, and push-with-upstream workflows
  with confirmations for mutating sync operations and ahead/behind visibility.
- Added typed branch listing, checkout, branch creation, fast-forward merge, and
  rebase workflows with dirty-tree guardrails and confirmation plans.
- Added Phase 9 operation planning, typed execution, queued operation preview
  and confirmation, and repo-aware audit visibility.
- Added Phase 10 deterministic prompt parsing for typed Git and GitHub operation
  requests, with golden parser tests, manual planner parity, safe multi-step
  prompts, and rejection of unsupported or shell-like input.
- Added a typed `gh` CLI boundary and guarded pull request planning and execution
  for GitHub.com and GitHub Enterprise, including `open pr` in prompt sequences.
- Added merge/rebase operation-state detection and typed continue, abort, and
  rebase-skip Git commands. Recovery planning and TUI actions remain open in
  [#55](https://github.com/cosentinode/bitbygit/issues/55) and
  [#56](https://github.com/cosentinode/bitbygit/issues/56).
- Added strict typed TOML configuration loading with complete safe-default
  fallback and redacted path-aware diagnostics.
- Added effective policy enforcement for protected branches, confirmation
  levels, disabled operations, and prompt availability.
- Applied configured pull request base defaults after explicit prompt targets
  and before provider defaults, with provider validation and visible safe config
  diagnostics in the TUI.
- Added CI and tagged-release workflows that build, smoke-test, package, and
  checksum supported Linux, macOS, and Windows artifacts, plus validated binary
  and source installation documentation. The first real release is still open
  in [#81](https://github.com/cosentinode/bitbygit/issues/81).
