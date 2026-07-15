# Repository Layout

```txt
bitbygit/
  Cargo.toml
  README.md
  CONTRIBUTING.md
  CHANGELOG.md
  LICENSE

  crates/
    bitbygit/
      src/main.rs

    bitbygit-core/
      src/lib.rs

    bitbygit-git/
      src/lib.rs

    bitbygit-store/
      src/lib.rs

    bitbygit-tui/
      src/lib.rs

  docs/
    adr/
      0001-system-git-first.md
    architecture.md
    command-planner.md
    configuration.md
    guardrails.md
    mvp-phases.md
    repo-layout.md
    tui-smoke-test.md

  scripts/
    setup-dev.sh

  .github/
    workflows/
      ci.yml
```

## Notes

- Do not add a repo-local `AGENTS.md`. Orchestration guidance lives outside this
  repository in the machine-level root instructions.
- Keep generated build output in `target/` and out of Git.
- Keep local task ledgers such as `.beads/` out of Git.
