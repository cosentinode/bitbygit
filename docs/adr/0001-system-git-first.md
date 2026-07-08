# ADR 0001: Use system Git first

Status: accepted

## Context

`bitbygit` needs to inspect and modify real user repositories. Users already
have Git configuration, credential helpers, commit signing, hooks, SSH config,
and hosting-provider behavior set up around the system `git` executable.

The project could use a Git library, but that would require reimplementing or
bridging more of the user's real Git environment early in the project.

## Decision

Use system `git` for MVP repository operations.

Commands must be invoked without an untrusted shell. The implementation should
build argument arrays from typed operations and parse stable command output when
available, such as `git status --porcelain=v2`.

## Consequences

Benefits:

- respects existing user Git configuration and credentials
- preserves hooks, signing, SSH, and credential helper behavior where compatible
  with operation guardrails
- avoids premature dependency on a lower-level Git library
- keeps early behavior close to what users expect from the command line

Costs:

- command output parsing must be tested carefully
- process execution errors need good diagnostics
- some operations may vary across Git versions
- guarded operations may intentionally use plumbing commands when porcelain
  behavior can mutate state after confirmation

## Security Notes

Using system Git does not mean executing arbitrary shell. Prompt text and user
input must be converted into typed operations first. Process execution must use
argument arrays, not interpolated shell strings.
