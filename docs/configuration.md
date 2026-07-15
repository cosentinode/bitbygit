# Configuration

`bitbygit` reads `config.toml` from its resolved configuration directory. It
does not create the file automatically. The directory is selected in this
order:

1. `$BITBYGIT_CONFIG_DIR`
2. `$XDG_CONFIG_HOME/bitbygit`
3. `$APPDATA/bitbygit`
4. `$HOME/.config/bitbygit`

For example, the usual Linux path is `~/.config/bitbygit/config.toml`. Setting
`BITBYGIT_CONFIG_DIR=/srv/bitbygit` selects
`/srv/bitbygit/config.toml`.

## Schema

All sections and keys are optional. Omitted values use the safe defaults shown
below.

```toml
schema-version = 1

[policy]
# These are added to the built-in main, master, develop, and release/* rules.
additional-protected-branches = []
disabled-operations = []

[policy.confirmation]
low = "normal-selection"
medium = "visible-plan"
high = "explicit-confirmation"

[pull-requests]
# Omit this key to use the provider repository's default branch.
# default-base-branch = "develop"

[prompt]
enabled = true
```

Protected branch entries may be exact branch names or a prefix ending in `/*`,
such as `stable/*`. They are additive: configuration cannot remove the built-in
protected branch rules.

Confirmation values are ordered from least to most restrictive:
`normal-selection`, `visible-plan`, `explicit-confirmation`, and `blocked`.
Each risk level may be made more restrictive but not less restrictive than its
documented default.

`disabled-operations` accepts these stable operation family names:

- `refresh-status`
- `view-diff`
- `fetch`
- `stage`
- `unstage`
- `commit`
- `push`
- `pull`
- `branches`
- `checkout`
- `create-branch`
- `merge`
- `rebase`
- `open-pull-request`

## Failure Behavior

Unknown keys, unknown operation names, unsupported schema versions, malformed
branch names or patterns, and values that weaken confirmation defaults make the
entire file invalid. An invalid or unreadable file produces a diagnostic with
its path and safe error context, then `bitbygit` applies the complete default
configuration. A missing file silently uses those defaults.

Configuration diagnostics never retain the raw file or rejected values, and
configuration contents are never written to operation audits. Credentials and
tokens are not supported configuration keys and should not be placed in this
file.
