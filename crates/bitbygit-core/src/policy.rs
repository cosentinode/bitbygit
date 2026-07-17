use crate::config::{
    AppConfig, ConfirmationConfig, ConfirmationSetting, OperationFamily, valid_branch_pattern,
};
use crate::{ConfirmationRequirement, OperationKind, RiskLevel};

const BUILT_IN_PROTECTED_BRANCHES: [&str; 4] = ["main", "master", "develop", "release/*"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectivePolicy {
    additional_protected_branches: Vec<String>,
    confirmation: ConfirmationConfig,
    disabled_operations: Vec<OperationFamily>,
    prompt_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyEvaluation {
    pub requirement: ConfirmationRequirement,
    pub reasons: Vec<String>,
}

impl EffectivePolicy {
    pub fn new(config: &AppConfig) -> Self {
        Self {
            additional_protected_branches: config.policy.additional_protected_branches.clone(),
            confirmation: config.policy.confirmation.clone(),
            disabled_operations: config.policy.disabled_operations.clone(),
            prompt_enabled: config.prompt.enabled,
        }
    }

    pub fn safe_fallback() -> Self {
        Self::new(&AppConfig::safe_fallback())
    }

    pub fn is_protected_branch(&self, branch: &str) -> bool {
        BUILT_IN_PROTECTED_BRANCHES
            .iter()
            .copied()
            .chain(
                self.additional_protected_branches
                    .iter()
                    .map(String::as_str),
            )
            .any(|pattern| branch_matches(pattern, branch))
    }

    pub fn confirmation_requirement(&self, risk_level: RiskLevel) -> ConfirmationRequirement {
        let setting = match risk_level {
            RiskLevel::Low => self
                .confirmation
                .low
                .max(ConfirmationSetting::NormalSelection),
            RiskLevel::Medium => self
                .confirmation
                .medium
                .max(ConfirmationSetting::VisiblePlan),
            RiskLevel::High => self
                .confirmation
                .high
                .max(ConfirmationSetting::ExplicitConfirmation),
            RiskLevel::BlockedByDefault => ConfirmationSetting::Blocked,
        };
        confirmation_requirement(setting)
    }

    pub fn evaluate_confirmation(
        &self,
        risk_level: RiskLevel,
        operation: OperationKind,
        branch: Option<&str>,
    ) -> PolicyEvaluation {
        let mut requirement = self.confirmation_requirement(risk_level);
        let mut reasons = vec![format!(
            "{} risk policy requires {} confirmation",
            risk_label(risk_level),
            confirmation_label(requirement)
        )];

        if let Some(branch) = branch {
            if self.is_protected_branch(branch) {
                if let Some(minimum) = protected_branch_requirement(operation) {
                    requirement = requirement.max(minimum);
                    reasons.push(format!(
                        "protected branch {branch}: {} requires at least {} confirmation",
                        operation.action_label(),
                        confirmation_label(minimum)
                    ));
                }
            }
        }

        if self.is_operation_disabled(operation) {
            requirement = ConfirmationRequirement::Blocked;
            reasons.push(format!(
                "{} is disabled by policy",
                operation.action_label()
            ));
        }

        PolicyEvaluation {
            requirement,
            reasons,
        }
    }

    pub fn is_operation_disabled(&self, operation: OperationKind) -> bool {
        self.disabled_operations
            .contains(&operation_family(operation))
    }

    pub const fn prompt_enabled(&self) -> bool {
        self.prompt_enabled
    }
}

impl Default for EffectivePolicy {
    fn default() -> Self {
        Self::new(&AppConfig::default())
    }
}

pub const fn operation_family(operation: OperationKind) -> OperationFamily {
    match operation {
        OperationKind::RefreshStatus => OperationFamily::RefreshStatus,
        OperationKind::ViewDiff => OperationFamily::ViewDiff,
        OperationKind::Fetch => OperationFamily::Fetch,
        OperationKind::StagePaths | OperationKind::StageAll => OperationFamily::Stage,
        OperationKind::UnstagePaths | OperationKind::UnstageAll => OperationFamily::Unstage,
        OperationKind::Commit => OperationFamily::Commit,
        OperationKind::PushCurrentBranch | OperationKind::PushSetUpstream => OperationFamily::Push,
        OperationKind::PullFastForward | OperationKind::PullRebase => OperationFamily::Pull,
        OperationKind::Branches => OperationFamily::Branches,
        OperationKind::CheckoutBranch => OperationFamily::Checkout,
        OperationKind::CreateBranch => OperationFamily::CreateBranch,
        OperationKind::MergeFastForward => OperationFamily::Merge,
        OperationKind::Rebase => OperationFamily::Rebase,
        OperationKind::OpenPullRequest => OperationFamily::OpenPullRequest,
    }
}

const fn confirmation_requirement(setting: ConfirmationSetting) -> ConfirmationRequirement {
    match setting {
        ConfirmationSetting::NormalSelection => ConfirmationRequirement::NormalSelection,
        ConfirmationSetting::VisiblePlan => ConfirmationRequirement::VisiblePlan,
        ConfirmationSetting::ExplicitConfirmation => ConfirmationRequirement::ExplicitConfirmation,
        ConfirmationSetting::Blocked => ConfirmationRequirement::Blocked,
    }
}

const fn protected_branch_requirement(operation: OperationKind) -> Option<ConfirmationRequirement> {
    match operation {
        OperationKind::Commit
        | OperationKind::PushCurrentBranch
        | OperationKind::PushSetUpstream => Some(ConfirmationRequirement::VisiblePlan),
        OperationKind::PullRebase | OperationKind::Rebase => {
            Some(ConfirmationRequirement::ExplicitConfirmation)
        }
        OperationKind::RefreshStatus
        | OperationKind::ViewDiff
        | OperationKind::Fetch
        | OperationKind::StagePaths
        | OperationKind::UnstagePaths
        | OperationKind::StageAll
        | OperationKind::UnstageAll
        | OperationKind::PullFastForward
        | OperationKind::Branches
        | OperationKind::CheckoutBranch
        | OperationKind::CreateBranch
        | OperationKind::MergeFastForward
        | OperationKind::OpenPullRequest => None,
    }
}

const fn risk_label(risk_level: RiskLevel) -> &'static str {
    match risk_level {
        RiskLevel::Low => "low",
        RiskLevel::Medium => "medium",
        RiskLevel::High => "high",
        RiskLevel::BlockedByDefault => "blocked-by-default",
    }
}

const fn confirmation_label(requirement: ConfirmationRequirement) -> &'static str {
    match requirement {
        ConfirmationRequirement::NormalSelection => "normal-selection",
        ConfirmationRequirement::VisiblePlan => "visible-plan",
        ConfirmationRequirement::ExplicitConfirmation => "explicit",
        ConfirmationRequirement::Blocked => "blocked",
    }
}

fn branch_matches(pattern: &str, branch: &str) -> bool {
    if !valid_branch_pattern(pattern) {
        return false;
    }

    pattern.strip_suffix('*').map_or_else(
        || pattern == branch,
        |prefix| {
            branch
                .strip_prefix(prefix)
                .is_some_and(|suffix| !suffix.is_empty())
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_protected_branches_use_exact_and_prefix_boundaries() {
        let policy = EffectivePolicy::default();

        for branch in [
            "main",
            "master",
            "develop",
            "release/1.0",
            "release/1.0/hotfix",
        ] {
            assert!(policy.is_protected_branch(branch), "{branch}");
        }
        for branch in [
            "mainline",
            "masters",
            "development",
            "release",
            "release/",
            "releases/1.0",
        ] {
            assert!(!policy.is_protected_branch(branch), "{branch}");
        }
    }

    #[test]
    fn configured_protected_branches_are_additive_and_malformed_patterns_are_ignored() {
        let mut config = AppConfig::default();
        config.policy.additional_protected_branches = vec![
            "production".to_owned(),
            "stable/*".to_owned(),
            "*".to_owned(),
            "release/**".to_owned(),
            "/*".to_owned(),
        ];
        let policy = EffectivePolicy::new(&config);

        for branch in ["main", "release/1.0", "production", "stable/1.0"] {
            assert!(policy.is_protected_branch(branch), "{branch}");
        }
        for branch in ["feature/auth", "stable", "stable/"] {
            assert!(!policy.is_protected_branch(branch), "{branch}");
        }
    }

    #[test]
    fn confirmation_settings_only_escalate_risk_requirements() {
        let mut config = AppConfig::default();
        config.policy.confirmation.low = ConfirmationSetting::VisiblePlan;
        config.policy.confirmation.medium = ConfirmationSetting::ExplicitConfirmation;
        config.policy.confirmation.high = ConfirmationSetting::Blocked;
        let policy = EffectivePolicy::new(&config);

        assert_eq!(
            policy.confirmation_requirement(RiskLevel::Low),
            ConfirmationRequirement::VisiblePlan
        );
        assert_eq!(
            policy.confirmation_requirement(RiskLevel::Medium),
            ConfirmationRequirement::ExplicitConfirmation
        );
        assert_eq!(
            policy.confirmation_requirement(RiskLevel::High),
            ConfirmationRequirement::Blocked
        );
        assert_eq!(
            policy.confirmation_requirement(RiskLevel::BlockedByDefault),
            ConfirmationRequirement::Blocked
        );
    }

    #[test]
    fn protected_branches_add_operation_specific_reasons_and_minimums() {
        let mut config = AppConfig::default();
        config.policy.additional_protected_branches = vec!["production".to_owned()];
        let policy = EffectivePolicy::new(&config);

        for branch in ["main", "production"] {
            let commit =
                policy.evaluate_confirmation(RiskLevel::Low, OperationKind::Commit, Some(branch));
            assert_eq!(commit.requirement, ConfirmationRequirement::VisiblePlan);
            assert!(commit.reasons.iter().any(|reason| reason.contains(branch)));

            let rebase = policy.evaluate_confirmation(
                RiskLevel::Medium,
                OperationKind::Rebase,
                Some(branch),
            );
            assert_eq!(
                rebase.requirement,
                ConfirmationRequirement::ExplicitConfirmation
            );
            assert!(rebase.reasons.iter().any(|reason| reason.contains(branch)));
        }

        let feature = policy.evaluate_confirmation(
            RiskLevel::Low,
            OperationKind::Commit,
            Some("feature/policy"),
        );
        assert_eq!(
            feature.requirement,
            ConfirmationRequirement::NormalSelection
        );
        assert_eq!(feature.reasons.len(), 1);
    }

    #[test]
    fn evaluator_defensively_clamps_unvalidated_confirmation_settings() {
        let mut config = AppConfig::default();
        config.policy.confirmation.medium = ConfirmationSetting::NormalSelection;
        config.policy.confirmation.high = ConfirmationSetting::VisiblePlan;
        let policy = EffectivePolicy::new(&config);

        assert_eq!(
            policy.confirmation_requirement(RiskLevel::Medium),
            ConfirmationRequirement::VisiblePlan
        );
        assert_eq!(
            policy.confirmation_requirement(RiskLevel::High),
            ConfirmationRequirement::ExplicitConfirmation
        );
    }

    #[test]
    fn disabled_families_cover_every_internal_operation_variant() {
        let operations = [
            (OperationKind::RefreshStatus, OperationFamily::RefreshStatus),
            (OperationKind::ViewDiff, OperationFamily::ViewDiff),
            (OperationKind::Fetch, OperationFamily::Fetch),
            (OperationKind::StagePaths, OperationFamily::Stage),
            (OperationKind::StageAll, OperationFamily::Stage),
            (OperationKind::UnstagePaths, OperationFamily::Unstage),
            (OperationKind::UnstageAll, OperationFamily::Unstage),
            (OperationKind::Commit, OperationFamily::Commit),
            (OperationKind::PushCurrentBranch, OperationFamily::Push),
            (OperationKind::PushSetUpstream, OperationFamily::Push),
            (OperationKind::PullFastForward, OperationFamily::Pull),
            (OperationKind::PullRebase, OperationFamily::Pull),
            (OperationKind::Branches, OperationFamily::Branches),
            (OperationKind::CheckoutBranch, OperationFamily::Checkout),
            (OperationKind::CreateBranch, OperationFamily::CreateBranch),
            (OperationKind::MergeFastForward, OperationFamily::Merge),
            (OperationKind::Rebase, OperationFamily::Rebase),
            (
                OperationKind::OpenPullRequest,
                OperationFamily::OpenPullRequest,
            ),
        ];

        for (operation, family) in operations {
            let mut config = AppConfig::default();
            config.policy.disabled_operations = vec![family];
            let policy = EffectivePolicy::new(&config);

            assert_eq!(operation_family(operation), family);
            assert!(policy.is_operation_disabled(operation), "{operation:?}");
            let evaluation = policy.evaluate_confirmation(RiskLevel::Low, operation, None);
            assert_eq!(
                evaluation.requirement,
                ConfirmationRequirement::Blocked,
                "{operation:?}"
            );
            assert!(
                evaluation
                    .reasons
                    .iter()
                    .any(|reason| reason.contains("disabled by policy")),
                "{operation:?}"
            );
        }
    }

    #[test]
    fn prompt_policy_only_exposes_enablement() {
        let mut config = AppConfig::default();
        assert!(EffectivePolicy::new(&config).prompt_enabled());

        config.prompt.enabled = false;
        assert!(!EffectivePolicy::new(&config).prompt_enabled());
    }

    #[test]
    fn safe_fallback_blocks_every_operation_and_prompt() {
        let policy = EffectivePolicy::safe_fallback();
        let operations = [
            OperationKind::RefreshStatus,
            OperationKind::ViewDiff,
            OperationKind::Fetch,
            OperationKind::StagePaths,
            OperationKind::StageAll,
            OperationKind::UnstagePaths,
            OperationKind::UnstageAll,
            OperationKind::Commit,
            OperationKind::PushCurrentBranch,
            OperationKind::PushSetUpstream,
            OperationKind::PullFastForward,
            OperationKind::PullRebase,
            OperationKind::Branches,
            OperationKind::CheckoutBranch,
            OperationKind::CreateBranch,
            OperationKind::MergeFastForward,
            OperationKind::Rebase,
            OperationKind::OpenPullRequest,
        ];

        assert!(!policy.prompt_enabled());
        for risk_level in [RiskLevel::Low, RiskLevel::Medium, RiskLevel::High] {
            assert_eq!(
                policy.confirmation_requirement(risk_level),
                ConfirmationRequirement::Blocked
            );
        }
        for operation in operations {
            assert!(policy.is_operation_disabled(operation), "{operation:?}");
        }
    }
}
