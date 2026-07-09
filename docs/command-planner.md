# Command Planner

The prompt box accepts small natural commands and converts them into typed
operation plans. It is deterministic for the MVP.

## Core Rule

Prompt text is input to a parser, not a shell. Unsupported text fails closed
with suggestions.

## Planner Pipeline

1. Normalize prompt text by trimming whitespace and matching commands
   case-insensitively without changing quoted values.
2. Parse quoted strings before connector tokenization.
3. Tokenize supported connectors such as `and`, `then`, and `&&` outside quoted
   strings.
4. Parse each segment into a supported operation request.
5. Validate that the sequence is allowed.
6. Read repository state for preconditions.
7. Produce a typed operation plan.
8. Show the preview and required confirmations.

## Phase 10 Supported Prompts

Completed Phase 10 parser support includes deterministic, typed plans for:

- `fetch`
- `push`
- `pull`
- `pull --rebase`
- `branches`
- `checkout <branch>`
- `branch <name>`
- `branch <name> from <base>`
- `merge <branch>`
- `rebase <base>`
- `commit <message>`
- `commit -m "message"`
- `commit -m "message" and push`

The parser also accepts `pull rebase` as an alias when the meaning is
unambiguous.

`open pr`, `open pull request`, and `commit and push and open pr` are Phase 11
GitHub integration work, not completed Phase 10 behavior. They depend on
[#13](https://github.com/cosentinode/bitbygit/issues/13). When implemented,
those prompts must produce plans that preview provider, remote, head branch,
base branch, title, and target URL before calling a GitHub operation.

## Quoted Strings

Quoted strings are atomic values. Connectors inside a quoted string are part of
that value, not plan separators.

For example, `commit -m "fix auth and routing" and push` should produce two
steps:

1. commit with message `fix auth and routing`
2. push the current branch

Unclosed quotes should fail closed with a parse error and no execution.

## Rejected Prompts

The parser must reject unsupported or unsafe prompts, including:

- arbitrary shell such as `rm -rf target`
- raw Git commands such as `git reset --hard HEAD~1`
- ambiguous text such as `fix everything`
- provider actions that need design such as `merge all PRs`

Rejected prompts should return suggestions, not partial execution.

## Multi-Step Plans

Multi-step prompts produce ordered plans. For example,
`commit -m "sync docs" and push` should become:

1. commit staged changes with message `sync docs`
2. push the current branch

If step 1 fails, step 2 must not run. Provider-backed sequences such as
`commit and push and open pr` must wait for the Phase 11 GitHub integration in
[#13](https://github.com/cosentinode/bitbygit/issues/13).

## Agent Compatibility

A future agent can help interpret user intent, but it must output the same typed
operation request format as the deterministic parser. The executor contract does
not change for agent-assisted planning.

The executor must never accept freeform shell from either the user or an agent.
