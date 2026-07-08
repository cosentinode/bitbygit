# Command Planner

The prompt box accepts small natural commands and converts them into typed
operation plans. It is deterministic for the MVP.

## Core Rule

Prompt text is input to a parser, not a shell. Unsupported text fails closed
with suggestions.

## Planner Pipeline

1. Normalize prompt text by trimming whitespace and matching case-insensitively.
2. Tokenize supported connectors such as `and`, `then`, and `&&`.
3. Parse each segment into a supported operation request.
4. Validate that the sequence is allowed.
5. Read repository state for preconditions.
6. Produce a typed operation plan.
7. Show the preview and required confirmations.

## Supported MVP Prompts

Initial supported commands should include:

- `fetch`
- `push`
- `pull`
- `pull --rebase`
- `commit`
- `commit -m "message"`
- `commit and push`
- `open pr`
- `commit and push and open pr`

The parser should also accept `pull rebase` and `open pull request` as aliases
when the meaning is unambiguous.

## Rejected Prompts

The parser must reject unsupported or unsafe prompts, including:

- arbitrary shell such as `rm -rf target`
- raw Git commands such as `git reset --hard HEAD~1`
- ambiguous text such as `fix everything`
- provider actions that need design such as `merge all PRs`

Rejected prompts should return suggestions, not partial execution.

## Multi-Step Plans

Multi-step prompts produce ordered plans. For example,
`commit and push and open pr` should become:

1. commit staged changes
2. push the current branch
3. open a pull request for the current branch

If step 1 fails, steps 2 and 3 must not run.

## Agent Compatibility

A future agent can help interpret user intent, but it must output the same typed
operation request format as the deterministic parser. The executor contract does
not change for agent-assisted planning.

The executor must never accept freeform shell from either the user or an agent.
