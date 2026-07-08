# Architecture

`bitbygit` is a terminal UI around typed Git and GitHub operations. Its central
design constraint is that user intent is converted into explicit operation plans
before anything runs.

## Principles

- Keep the implementation simple enough to inspect.
- Keep security decisions explicit and testable.
- Keep boundaries scalable so new workflows do not require rewrites.

## Planned Runtime Flow

1. User input comes from keyboard, mouse, or the prompt box.
2. Input is converted into a typed operation request.
3. The planner validates repository state and creates a typed operation plan.
4. The UI previews the plan and requests confirmation when needed.
5. The executor runs typed commands with argument arrays, not shell strings.
6. The app records the result in audit history and refreshes repository state.

## Initial Workspace

- `crates/bitbygit`: CLI binary entry point.
- `crates/bitbygit-core`: shared domain model and constants.

Future phases should add crates only when the boundary is useful in practice.
Likely future crates include `bitbygit-git`, `bitbygit-tui`, `bitbygit-gh`, and
`bitbygit-store`.

## Git Boundary

The MVP uses system `git` instead of a Git library. This preserves user Git
configuration, credential helpers, signing, hooks, and SSH setup.

Git commands must be executed through typed wrappers. Prompt text and other user
input must never be interpolated into shell commands.

## GitHub Boundary

The MVP should use the `gh` CLI for GitHub-specific workflows such as pull
request creation. `bitbygit` should not store GitHub tokens.

## Guardrail Boundary

Risky operations should flow through the same plan, preview, confirm, execute,
and audit lifecycle regardless of whether they start from the UI or prompt.
