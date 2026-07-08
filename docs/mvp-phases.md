# MVP Phases

This roadmap tracks the implementation order for `bitbygit`. GitHub issues are
the source of task ownership, and this document keeps the repo-local roadmap
easy to resume across sessions.

## Phase 0: Repository foundation and engineering standards

Status:

- merged

Must have:

- Rust workspace baseline
- root `README.md`, `CONTRIBUTING.md`, and `CHANGELOG.md`
- architecture, repo layout, and phase docs
- CI for format, clippy, test, and build
- development setup script

Success condition:

- the repository can be cloned, checked, and understood without manual guessing

## Phase 1: Product architecture, operation model, and guardrails

Status:

- merged

Must have:

- product model for repositories, status, branches, remotes, operations, prompts,
  plans, and audit entries
- typed operation lifecycle from request to audit result
- risk levels and confirmation policy
- protected branch behavior
- recovery model for merge and rebase conflicts

Success condition:

- risky Git workflows have a documented design before implementation

## Phase 2: Typed system Git command layer

Status:

- merged

Must have:

- no shell execution for user prompt text
- typed command inputs and outputs
- repository root, branch, remote, upstream, and status parsing
- temporary-repository integration tests

Success condition:

- Git state can be queried safely and deterministically

## Phase 3: Repository registry and local app state

Status:

- merged

Must have:

- persisted repo registry
- active and recent repository state
- operation audit storage
- invalid repository diagnostics

Success condition:

- users can add, switch, and remove repositories from the app without touching
  repository files on disk

## Phase 4: Responsive single-viewport TUI shell

Status:

- in progress

Must have:

- terminal app loop
- responsive panels for repos, status, details, history or queue, and prompt
- keyboard navigation
- mouse selection where practical

Success condition:

- the app has a usable one-viewport shell that can host real Git workflows

## Phase 5: Status, staging, and diff views

Must have:

- staged, unstaged, untracked, renamed, deleted, and conflicted file display
- selected file diff panel
- stage and unstage actions

Success condition:

- users can inspect and prepare a commit from the TUI

## Phase 6: Guarded commit workflow

Must have:

- commit staged changes
- validate empty staged state
- show commit plan before execution
- surface hooks, signing, and Git failures clearly

Success condition:

- users can create commits while preserving guardrails and auditability

## Phase 7: Fetch, pull, and push workflows

Must have:

- fetch default remote
- push current branch
- set upstream for new branches after confirmation
- pull and explicit pull-with-rebase
- ahead and behind state

Success condition:

- common remote sync workflows are safe and clear

## Phase 8: Branch workflows

Must have:

- list local and remote branches
- checkout existing branches
- create branch
- merge selected branch
- rebase onto selected base
- dirty tree checks

Success condition:

- everyday branch movement works without surprising destructive behavior

## Phase 9: Operation planner, preview, queue, execution, and audit

Must have:

- typed operation plans
- risk levels
- preview and confirmation
- ordered execution
- safe stop-on-failure behavior
- audit history

Success condition:

- manual UI actions and prompt actions share one safety-first execution model

## Phase 10: Deterministic prompt parser

Must have:

- parse simple commands such as `commit`, `commit and push`, `fetch`, `push`,
  `open pr`, and `commit and push and open PR`
- reject unsupported prompts safely
- produce typed plans only

Success condition:

- prompt input is useful without becoming arbitrary shell execution

## Phase 11: GitHub integration and pull request creation

Must have:

- detect `gh` availability and auth state
- create pull request for current branch
- confirm base branch
- surface PR URL

Success condition:

- users can include PR creation in a guarded multi-step workflow

## Phase 12: Conflict detection and recovery UX

Must have:

- merge and rebase in-progress detection
- conflicted file display
- continue, abort, and skip actions where applicable
- block unrelated risky operations during conflict states

Success condition:

- conflict states are understandable and recoverable from the TUI

## Phase 13: Config and policy controls

Must have:

- config file format and location
- protected branch configuration
- confirmation policy by risk level
- default PR base branch
- disabled operations

Success condition:

- users can adjust policy without weakening safe defaults

## Phase 14: Packaging, installation, and release automation

Must have:

- release build workflow
- binary artifacts for supported platforms
- version output
- installation docs

Success condition:

- users can install and run `bitbygit` from release artifacts

## Phase 15: Docs, onboarding, and contributor workflow

Must have:

- expanded usage docs
- keyboard and mouse controls
- prompt examples
- guardrail philosophy
- contributor workflow

Success condition:

- future sessions and contributors can resume work from docs and issues
