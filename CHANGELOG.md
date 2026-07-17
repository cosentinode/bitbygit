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
- Added Phase 10 deterministic prompt parsing for typed Git operation requests
  as part of [#12](https://github.com/cosentinode/bitbygit/issues/12), with
  golden parser tests, manual planner parity, and safe non-provider multi-step
  prompts; `open pr` prompt execution remains Phase 11 GitHub integration work
  in [#13](https://github.com/cosentinode/bitbygit/issues/13).
- Added strict typed TOML configuration loading with complete safe-default
  fallback and redacted path-aware diagnostics.
- Applied configured pull request base defaults after explicit prompt targets
  and before provider defaults, with provider validation and visible safe config
  diagnostics in the TUI.
