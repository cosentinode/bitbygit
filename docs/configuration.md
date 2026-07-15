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

## Examples

Use a repository branch other than the provider default for pull requests opened
without an explicit target:

```toml
[pull-requests]
default-base-branch = "develop"
```

For `open pr`, the target is selected in this order: an explicit prompt target
such as `open pr to release`, `default-base-branch`, then the GitHub repository's
default branch. The selected branch is checked against the upstream GitHub
repository during planning on both GitHub.com and GitHub Enterprise. If a
configured branch does not exist, the operation is blocked with guidance to fix
the setting or provide an explicit target.

Add team-specific protected branches, require stronger confirmation, disable
selected operation families, and turn off prompt input:

```toml
[policy]
additional-protected-branches = ["production", "stable/*"]
disabled-operations = ["rebase"]

[policy.confirmation]
medium = "explicit-confirmation"
high = "blocked"

[prompt]
enabled = false
```

Setting `prompt.enabled` to `false` rejects prompt submissions. It does not
alter operation policy, and the prompt configuration has no command, shell, or
hook extension points; enabled prompts still produce only built-in typed plans.

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
its path and safe error context, then `bitbygit` applies a complete fail-closed
fallback: every operation family is disabled, all confirmation levels are
blocked, prompt input is disabled, and non-policy settings use their defaults.
At runtime, a failed reload instead retains the last valid effective policy and
keeps the reload diagnostic visible in the details pane. Configuration is
reloaded before input and periodically while idle; once it is valid, the
diagnostic clears and status is refreshed if the recovered policy permits it. A
missing file silently uses the documented defaults above.

Direct status and diff reads run only when low-risk policy permits normal
selection. Disabled operations and confirmation settings of `visible-plan`,
`explicit-confirmation`, or `blocked` prevent these automatic UI reads; policy
reload remains available so the configuration can still be repaired in place.

Configuration diagnostics never retain the raw file or rejected values, and
configuration contents are never written to operation audits. Credentials and
tokens are not supported configuration keys and should not be placed in this
file.
