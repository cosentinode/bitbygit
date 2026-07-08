# Guardrails

Guardrails are the main product difference between `bitbygit` and a raw Git
terminal workflow. The default behavior should be safe, explicit, and easy to
explain.

## Safety Rules

- Never execute arbitrary prompt text as shell.
- Use typed operations and argument arrays for all process execution.
- Preview every multi-step operation before execution.
- Confirm operations based on risk level.
- Stop a multi-step operation after the first failed step unless continuation is
  explicitly safe.
- Treat missing, invalid, or unreadable config as safe defaults.
- Do not store GitHub tokens, Git credentials, SSH keys, or credential helper
  output.
- Do not persist raw stdout or stderr by default. Audit entries should store
  sanitized metadata and redacted diagnostics only when needed.

## Risk Levels

Low-risk operations can run after normal user selection:

- refresh status
- view diff
- fetch
- stage or unstage selected paths
- switch active repository

Medium-risk operations require a visible plan and confirmation:

- commit
- push current branch
- push new branch and set upstream
- pull that Git reports can fast-forward without creating a merge commit or
  rebasing
- create branch
- checkout branch with a clean working tree
- open pull request

Fast-forward pull detection should be implementation-specific, such as fetch
plus an ancestry check or an explicit `--ff-only` plan. The UI should not assume
a configured pull strategy is safe without checking the current branch state.

Pull request creation must preview provider, remote, head branch, base branch,
title, and target URL before execution.

High-risk operations require explicit confirmation and must explain the reason:

- merge
- rebase
- pull with rebase
- pull that would create a merge commit
- pull from a diverged branch where Git cannot fast-forward cleanly
- amend commit
- abort merge or rebase
- delete branch
- soft or mixed reset that moves a ref without discarding file content
- stash pop or apply

Blocked-by-default operations require a later dedicated design before support:

- force push
- hard reset
- reset operations that discard file content
- deleting untracked files
- deleting a repository from disk
- running custom user shell commands

If force push is ever added, it must prefer `--force-with-lease`, show the
remote and expected ref, and require a separate policy opt-in.

## Protected Branches

The default protected branch patterns are:

- `main`
- `master`
- `develop`
- `release/*`

Protected branch behavior:

- committing directly to a protected branch requires confirmation
- pushing a protected branch requires confirmation
- rebasing a protected branch is high risk
- deleting a protected branch is blocked by default
- force pushing a protected branch is blocked by default

Config may add protected patterns, but missing config must not weaken these
defaults.

## Dirty Working Tree Rules

Branch-changing operations must inspect the working tree first.

- Checkout is blocked when local changes may be overwritten.
- Merge and rebase require a clean working tree unless Git can prove the
  operation is safe.
- Pull with rebase requires a clean working tree.
- Commit requires staged changes.
- Guarded commit execution must commit the confirmed staged tree rather than any
  later index mutation.
- Push warns when the branch has no upstream or is diverged.

## Conflict Recovery

When Git reports an active merge or rebase, `bitbygit` should enter conflict
mode for that repository.

Conflict mode should:

- show the in-progress operation prominently
- list conflicted files before ordinary changed files
- block unrelated high-risk operations
- allow explicit continue, abort, and skip actions where Git supports them
- audit conflict recovery actions

Conflict recovery actions are high risk because they move repository state.

## Confirmation Copy

Confirmation text should be specific. Avoid generic prompts like `Are you sure?`.

Good examples:

- `Push branch feature/auth to origin and set upstream?`
- `Rebase current branch feature/auth onto origin/develop?`
- `Abort the active rebase in /path/to/repo?`
