use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{self, Display, Formatter};

use serde::Deserialize;

pub const CONFIG_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct AppConfig {
    pub schema_version: u32,
    pub policy: PolicyConfig,
    pub pull_requests: PullRequestConfig,
    pub prompt: PromptConfig,
}

impl AppConfig {
    pub fn parse(contents: &str) -> Result<Self, ConfigError> {
        let config = toml::from_str::<Self>(contents).map_err(|error| {
            let (line, column) = error
                .span()
                .and_then(|span| line_column(contents, span.start))
                .unzip();
            ConfigError::InvalidSchema { line, column }
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(invalid_value(
                "schema-version",
                "only schema version 1 is supported",
            ));
        }

        self.policy.validate()?;
        if let Some(branch) = &self.pull_requests.default_base_branch
            && !valid_branch_name(branch)
        {
            return Err(invalid_value(
                "pull-requests.default-base-branch",
                "must be a valid local branch name",
            ));
        }
        Ok(())
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            policy: PolicyConfig::default(),
            pull_requests: PullRequestConfig::default(),
            prompt: PromptConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct PolicyConfig {
    pub additional_protected_branches: Vec<String>,
    pub confirmation: ConfirmationConfig,
    pub disabled_operations: Vec<OperationFamily>,
}

impl PolicyConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        let mut patterns = BTreeSet::new();
        for pattern in &self.additional_protected_branches {
            if !valid_branch_pattern(pattern) {
                return Err(invalid_value(
                    "policy.additional-protected-branches",
                    "entries must be branch names or patterns ending in /*",
                ));
            }
            if !patterns.insert(pattern) {
                return Err(invalid_value(
                    "policy.additional-protected-branches",
                    "entries must not be repeated",
                ));
            }
        }

        let mut operations = BTreeSet::new();
        for operation in &self.disabled_operations {
            if !operations.insert(operation) {
                return Err(invalid_value(
                    "policy.disabled-operations",
                    "entries must not be repeated",
                ));
            }
        }

        self.confirmation.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ConfirmationConfig {
    pub low: ConfirmationSetting,
    pub medium: ConfirmationSetting,
    pub high: ConfirmationSetting,
}

impl ConfirmationConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.low < ConfirmationSetting::NormalSelection {
            return Err(invalid_value(
                "policy.confirmation.low",
                "cannot weaken the safe default",
            ));
        }
        if self.medium < ConfirmationSetting::VisiblePlan {
            return Err(invalid_value(
                "policy.confirmation.medium",
                "cannot weaken the safe default",
            ));
        }
        if self.high < ConfirmationSetting::ExplicitConfirmation {
            return Err(invalid_value(
                "policy.confirmation.high",
                "cannot weaken the safe default",
            ));
        }
        Ok(())
    }
}

impl Default for ConfirmationConfig {
    fn default() -> Self {
        Self {
            low: ConfirmationSetting::NormalSelection,
            medium: ConfirmationSetting::VisiblePlan,
            high: ConfirmationSetting::ExplicitConfirmation,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfirmationSetting {
    NormalSelection,
    VisiblePlan,
    ExplicitConfirmation,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationFamily {
    RefreshStatus,
    ViewDiff,
    Fetch,
    Stage,
    Unstage,
    Commit,
    Push,
    Pull,
    Branches,
    Checkout,
    CreateBranch,
    Merge,
    Rebase,
    OpenPullRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct PullRequestConfig {
    pub default_base_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct PromptConfig {
    pub enabled: bool,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    InvalidSchema {
        line: Option<usize>,
        column: Option<usize>,
    },
    InvalidValue {
        field: &'static str,
        reason: &'static str,
    },
}

impl Display for ConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSchema {
                line: Some(line),
                column: Some(column),
            } => write!(
                formatter,
                "invalid TOML syntax or configuration schema at line {line}, column {column}"
            ),
            Self::InvalidSchema { .. } => {
                formatter.write_str("invalid TOML syntax or configuration schema")
            }
            Self::InvalidValue { field, reason } => {
                write!(formatter, "invalid value for {field}: {reason}")
            }
        }
    }
}

impl Error for ConfigError {}

fn invalid_value(field: &'static str, reason: &'static str) -> ConfigError {
    ConfigError::InvalidValue { field, reason }
}

fn line_column(contents: &str, offset: usize) -> Option<(usize, usize)> {
    let prefix = contents.get(..offset)?;
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix.rsplit('\n').next()?.chars().count() + 1;
    Some((line, column))
}

pub(crate) fn valid_branch_pattern(pattern: &str) -> bool {
    pattern.strip_suffix("/*").map_or_else(
        || valid_branch_name(pattern),
        |prefix| valid_branch_name(&format!("{prefix}/branch")),
    )
}

fn valid_branch_name(branch: &str) -> bool {
    !branch.is_empty()
        && branch.trim() == branch
        && branch != "@"
        && branch != "HEAD"
        && !branch.starts_with(['-', '/', '.'])
        && !branch.ends_with(['/', '.'])
        && !branch.contains("..")
        && !branch.contains("//")
        && !branch.contains("@{")
        && !branch
            .chars()
            .any(|character| character.is_control() || " ~^:?*[\\".contains(character))
        && branch.split('/').all(|component| {
            !component.is_empty() && !component.starts_with('.') && !component.ends_with(".lock")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_uses_complete_safe_defaults() -> Result<(), ConfigError> {
        assert_eq!(AppConfig::parse("")?, AppConfig::default());
        Ok(())
    }

    #[test]
    fn valid_toml_produces_typed_settings() -> Result<(), ConfigError> {
        let config = AppConfig::parse(
            r#"
schema-version = 1

[policy]
additional-protected-branches = ["production", "stable/*"]
disabled-operations = ["pull", "rebase"]

[policy.confirmation]
low = "visible-plan"
medium = "explicit-confirmation"
high = "blocked"

[pull-requests]
default-base-branch = "develop"

[prompt]
enabled = false
"#,
        )?;

        assert_eq!(
            config.policy.additional_protected_branches,
            ["production", "stable/*"]
        );
        assert_eq!(
            config.policy.disabled_operations,
            [OperationFamily::Pull, OperationFamily::Rebase]
        );
        assert_eq!(
            config.policy.confirmation.medium,
            ConfirmationSetting::ExplicitConfirmation
        );
        assert_eq!(
            config.pull_requests.default_base_branch.as_deref(),
            Some("develop")
        );
        assert!(!config.prompt.enabled);
        Ok(())
    }

    #[test]
    fn unknown_keys_and_enum_values_are_rejected_without_retaining_contents()
    -> Result<(), Box<dyn Error>> {
        let secret = "credential = \"super-secret-value\"";
        let error = match AppConfig::parse(secret) {
            Ok(_config) => return Err("unknown key must fail".into()),
            Err(error) => error,
        };
        assert!(!format!("{error:?}").contains("super-secret-value"));

        assert!(AppConfig::parse("[policy]\ndisabled-operations = [\"force-push\"]").is_err());
        assert!(AppConfig::parse("[prompt]\ncommand = \"git status\"").is_err());
        Ok(())
    }

    #[test]
    fn values_that_weaken_or_ambiguate_policy_are_rejected() {
        assert!(AppConfig::parse("[policy.confirmation]\nmedium = \"normal-selection\"").is_err());
        assert!(
            AppConfig::parse("[policy]\nadditional-protected-branches = [\"release/**\"]").is_err()
        );
        assert!(AppConfig::parse("[pull-requests]\ndefault-base-branch = \"bad branch\"").is_err());
    }

    #[test]
    fn git_invalid_branch_name_boundaries_are_rejected() {
        for branch in ["HEAD", "foo.lock/bar", "foo/bar.lock/baz"] {
            assert!(
                AppConfig::parse(&format!(
                    "[pull-requests]\ndefault-base-branch = \"{branch}\""
                ))
                .is_err()
            );
            assert!(
                AppConfig::parse(&format!(
                    "[policy]\nadditional-protected-branches = [\"{branch}\"]"
                ))
                .is_err()
            );
        }

        for pattern in ["foo.lock/*", "foo/bar.lock/*"] {
            assert!(
                AppConfig::parse(&format!(
                    "[policy]\nadditional-protected-branches = [\"{pattern}\"]"
                ))
                .is_err()
            );
        }
    }

    #[test]
    fn git_valid_branch_name_boundaries_and_wildcards_are_accepted() -> Result<(), ConfigError> {
        AppConfig::parse(
            r#"
[policy]
additional-protected-branches = ["HEAD/*", "foo.locked/*", "foo.LOCK/bar"]

[pull-requests]
default-base-branch = "foo/HEAD"
"#,
        )?;
        Ok(())
    }
}
