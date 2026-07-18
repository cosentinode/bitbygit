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

- merged

Must have:

- terminal app loop
- responsive panels for repos, status, details, history or queue, and prompt
- keyboard navigation
- mouse selection where practical

Success condition:

- the app has a usable one-viewport shell that can host real Git workflows

## Phase 5: Status, staging, and diff views

Status:

- merged

Must have:

- staged, unstaged, untracked, renamed, deleted, and conflicted file display
- selected file diff panel
- stage and unstage actions

Success condition:

- users can inspect and prepare a commit from the TUI

## Phase 6: Guarded commit workflow

Status:

- merged

Must have:

- commit staged changes
- validate empty staged state
- show commit plan before execution
- commit the confirmed staged tree and surface Git/guardrail failures clearly

Success condition:

- users can create commits while preserving guardrails and auditability

## Phase 7: Fetch, pull, and push workflows

Status:

- merged

Must have:

- fetch default remote
- push current branch
- set upstream for new branches after confirmation
- pull and explicit pull-with-rebase
- ahead and behind state

Success condition:

- common remote sync workflows are safe and clear

## Phase 8: Branch workflows

Status:

- merged

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

Status:

- merged

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

Status:

- merged as part of [#12](https://github.com/cosentinode/bitbygit/issues/12)

Must have:

- parse simple commands such as `branches`, `checkout <branch>`,
  `branch <name>`, `merge <branch>`, `rebase <base>`, `commit -m "message"`,
  `fetch`, `push`, `pull`, `pull --rebase`, and safe non-provider sequences
  such as `commit -m "message" and push`
- reject unsupported prompts safely
- produce typed plans only
- cover parser behavior with golden tests and keep prompt/manual planner output
  aligned

Success condition:

- prompt input is useful without becoming arbitrary shell execution

## Phase 11: GitHub integration and pull request creation

Status:

- merged as part of [#13](https://github.com/cosentinode/bitbygit/issues/13)

Must have:

- detect `gh` availability and auth state
- create pull request for current branch
- confirm base branch
- surface PR URL
- add `open pr`, `open pull request`, and `commit and push and open pr` prompt
  execution

Success condition:

- users can include PR creation in a guarded multi-step workflow

## Phase 12: Conflict detection and recovery UX

Status:

- in progress under [#14](https://github.com/cosentinode/bitbygit/issues/14)
- operation-state detection and typed recovery commands merged in
  [#53](https://github.com/cosentinode/bitbygit/issues/53) and
  [#54](https://github.com/cosentinode/bitbygit/issues/54)
- recovery policy and TUI actions remain open in
  [#55](https://github.com/cosentinode/bitbygit/issues/55) and
  [#56](https://github.com/cosentinode/bitbygit/issues/56)

Must have:

- merge and rebase in-progress detection
- conflicted file display
- continue, abort, and skip actions where applicable
- block unrelated risky operations during conflict states

Success condition:

- conflict states are understandable and recoverable from the TUI

## Phase 13: Config and policy controls

Status:

- implementation slices merged in
  [#59](https://github.com/cosentinode/bitbygit/issues/59),
  [#62](https://github.com/cosentinode/bitbygit/issues/62),
  [#65](https://github.com/cosentinode/bitbygit/issues/65),
  [#67](https://github.com/cosentinode/bitbygit/issues/67), and
  [#69](https://github.com/cosentinode/bitbygit/issues/69); umbrella
  [#15](https://github.com/cosentinode/bitbygit/issues/15) is closed and Phase
  13 is complete

Must have:

- config file format and location
- protected branch configuration
- confirmation policy by risk level
- default PR base branch
- disabled operations

Success condition:

- users can adjust policy without weakening safe defaults

## Phase 14: Packaging, installation, and release automation

Status:

- release automation and installation docs merged in
  [#57](https://github.com/cosentinode/bitbygit/issues/57),
  [#61](https://github.com/cosentinode/bitbygit/issues/61), and
  [#64](https://github.com/cosentinode/bitbygit/issues/64)
- the first real release remains open in
  [#81](https://github.com/cosentinode/bitbygit/issues/81), and umbrella
  [#16](https://github.com/cosentinode/bitbygit/issues/16) remains open

Must have:

- release build workflow
- binary artifacts for supported platforms
- version output
- installation docs

Success condition:

- users can install and run `bitbygit` from release artifacts

## Phase 15: Docs, onboarding, and contributor workflow

Status:

- in progress under [#17](https://github.com/cosentinode/bitbygit/issues/17);
  remaining documentation slices are tracked in
  [#60](https://github.com/cosentinode/bitbygit/issues/60),
  [#63](https://github.com/cosentinode/bitbygit/issues/63),
  [#66](https://github.com/cosentinode/bitbygit/issues/66), and
  [#68](https://github.com/cosentinode/bitbygit/issues/68)

Must have:

- expanded usage docs
- keyboard and mouse controls
- prompt examples
- guardrail philosophy
- contributor workflow

Success condition:

- future sessions and contributors can resume work from docs and issues
