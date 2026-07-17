# Repository Layout

```txt
bitbygit/
  .github/
    workflows/
      ci.yml
      release.yml

  .gitignore
  CHANGELOG.md
  CONTRIBUTING.md
  Cargo.lock
  Cargo.toml
  LICENSE
  README.md

  crates/
    bitbygit/
      Cargo.toml
      src/main.rs

    bitbygit-core/
      Cargo.toml
      src/
        config.rs
        lib.rs
        policy.rs
        prompt_parser.rs

    bitbygit-gh/
      Cargo.toml
      src/lib.rs

    bitbygit-git/
      Cargo.toml
      src/lib.rs

    bitbygit-store/
      Cargo.toml
      src/lib.rs

    bitbygit-tui/
      Cargo.toml
      src/lib.rs

  docs/
    adr/
      0001-system-git-first.md
    architecture.md
    command-planner.md
    configuration.md
    guardrails.md
    installation.md
    mvp-phases.md
    repo-layout.md
    tui-smoke-test.md

  scripts/
    setup-dev.sh
    test-installation-docs.ps1
    test-installation-docs.sh
    test-release-workflow.sh
```

## Notes

- Do not add a repo-local `AGENTS.md`. Orchestration guidance lives outside this
  repository in the machine-level root instructions.
- Keep generated build output in `target/` and out of Git.
- Keep local task ledgers such as `.beads/` out of Git.
