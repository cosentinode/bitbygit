# Architecture

`bitbygit` is a terminal UI around typed Git and GitHub operations. Its central
design constraint is that user intent is converted into explicit operation plans
before anything runs.

## Principles

- Keep the implementation simple enough to inspect.
- Keep security decisions explicit and testable.
- Keep boundaries scalable so new workflows do not require rewrites.

## Product Model

The core model is small and explicit:

- Repository: a registered working tree path plus derived Git identity such as
  root, current branch, upstream, remotes, and provider hints.
- Working tree status: staged, unstaged, untracked, renamed, deleted, ignored
  when requested, and conflicted paths.
- Branch: local or remote branch name, current marker, upstream relation, and
  ahead/behind counts when available.
- Remote: name, fetch URL, push URL, and inferred provider.
- Operation request: user intent from a keybinding, mouse action, menu command,
  or prompt parse.
- Operation plan: ordered typed steps with risk level, required confirmations,
  preconditions, and preview text.
- Operation result: success or failure for each step with captured diagnostics.
- Audit entry: repository, operation kind, risk level, result, timestamp, and
  sanitized command metadata.

## Runtime Boundaries

```txt
keyboard/mouse/prompt
        |
        v
TUI state and focus
        |
        v
typed operation request
        |
        v
planner and guardrails
        |
        v
preview and confirmation
        |
        v
executor
        |
        +--> system git
        +--> gh CLI for GitHub-specific workflows
        |
        v
audit and repository refresh
```

The UI may collect intent, but it must not build raw shell commands. The prompt
parser may understand text, but it must only return typed operation requests.
The executor may run processes, but only from validated operation plans.

## Operation Lifecycle

1. Parse intent from UI action or prompt text.
2. Build a typed operation request.
3. Read repository state needed for preconditions.
4. Build a typed operation plan.
5. Attach risk level, required confirmations, and preview text.
6. Show the plan in the UI.
7. Execute each step in order after required confirmation.
8. Stop on failure unless the plan explicitly marks the next step as safe.
9. Record an audit entry with sanitized metadata.
10. Refresh repository state.

## Workspace

- `crates/bitbygit`: CLI binary entry point.
- `crates/bitbygit-core`: operation model, prompt parser, configuration, policy,
  and shared constants.
- `crates/bitbygit-gh`: typed `gh` CLI boundary for repository and pull request
  operations.
- `crates/bitbygit-git`: typed system Git command boundary.
- `crates/bitbygit-store`: local registry, app state, and audit storage.
- `crates/bitbygit-tui`: responsive terminal UI, operation planner, pending
  queue, confirmation flow, and typed executor.

The initial store is a single-writer file-backed store. The TUI should route
mutations through one runtime owner; multi-process locking is a later concern
before supporting concurrent app instances.

## Git Boundary

The MVP uses system `git` instead of a Git library. This preserves user Git
configuration, credential helpers, signing, hooks, and SSH setup where
compatible with operation guardrails. See
[`adr/0001-system-git-first.md`](adr/0001-system-git-first.md).

Git commands must be executed through typed wrappers with argument arrays.
Prompt text and other user input must never be interpolated into shell commands.

## GitHub Boundary

The MVP uses the `gh` CLI for GitHub-specific workflows such as pull request
creation on GitHub.com and GitHub Enterprise. `bitbygit` does not store GitHub
tokens.

GitHub behavior stays behind a typed boundary so direct GitHub API calls or
other providers can be added later without changing the planner contract.

## Configuration and Policy Boundary

`bitbygit-core` parses strict typed TOML configuration and derives an effective
policy. Invalid startup configuration fails closed, and runtime reload failures
retain the last valid policy. Protected branches, confirmation levels, disabled
operations, prompt availability, and the default pull request base are applied
to typed plans rather than raw commands.

## Automation Boundary

`.github/workflows/ci.yml` checks the workspace and supported source-build
targets. `.github/workflows/release.yml` validates version tags, builds and
smoke-tests supported platform archives, creates checksums, and publishes an
immutable GitHub release. The automation is implemented, but the first real
tagged release remains open in
[#81](https://github.com/cosentinode/bitbygit/issues/81).

## Guardrail Boundary

Risky operations should flow through the same plan, preview, confirm, execute,
and audit lifecycle regardless of whether they start from the UI or prompt. See
[`guardrails.md`](guardrails.md).
