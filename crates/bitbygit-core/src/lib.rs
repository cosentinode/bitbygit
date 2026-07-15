pub mod config;
pub mod policy;
pub mod prompt_parser;

pub const APP_NAME: &str = "bitbygit";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationRequest {
    RefreshStatus,
    ViewDiff {
        path: String,
    },
    Fetch,
    StagePaths {
        paths: Vec<String>,
    },
    UnstagePaths {
        paths: Vec<String>,
    },
    StageAll,
    UnstageAll,
    Commit {
        message: String,
    },
    Push,
    Pull {
        rebase: bool,
    },
    Branches,
    Checkout {
        branch: String,
    },
    CreateBranch {
        branch: String,
        base: Option<String>,
    },
    Merge {
        branch: String,
    },
    Rebase {
        base: String,
    },
    OpenPullRequest {
        base: Option<String>,
    },
    PromptSequence {
        requests: Vec<OperationRequest>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    RefreshStatus,
    ViewDiff,
    Fetch,
    StagePaths,
    UnstagePaths,
    StageAll,
    UnstageAll,
    Commit,
    PushCurrentBranch,
    PushSetUpstream,
    PullFastForward,
    PullRebase,
    Branches,
    CheckoutBranch,
    CreateBranch,
    MergeFastForward,
    Rebase,
    OpenPullRequest,
}

impl OperationKind {
    pub const fn action_label(self) -> &'static str {
        match self {
            Self::RefreshStatus => "refresh status",
            Self::ViewDiff => "view diff",
            Self::Fetch => "fetch",
            Self::StagePaths => "stage",
            Self::UnstagePaths => "unstage",
            Self::StageAll => "stage all",
            Self::UnstageAll => "unstage all",
            Self::Commit => "commit",
            Self::PushCurrentBranch | Self::PushSetUpstream => "push",
            Self::PullFastForward => "pull",
            Self::PullRebase => "pull rebase",
            Self::Branches => "branches",
            Self::CheckoutBranch => "checkout",
            Self::CreateBranch => "create branch",
            Self::MergeFastForward => "merge",
            Self::Rebase => "rebase",
            Self::OpenPullRequest => "open pull request",
        }
    }

    pub const fn audit_operation(self) -> &'static str {
        match self {
            Self::RefreshStatus => "refresh_status",
            Self::ViewDiff => "view_diff",
            Self::Fetch => "fetch",
            Self::StagePaths => "stage_path",
            Self::UnstagePaths => "unstage_path",
            Self::StageAll => "stage_all",
            Self::UnstageAll => "unstage_all",
            Self::Commit => "commit",
            Self::PushCurrentBranch => "push",
            Self::PushSetUpstream => "push_set_upstream",
            Self::PullFastForward => "pull",
            Self::PullRebase => "pull_rebase",
            Self::Branches => "branches",
            Self::CheckoutBranch => "checkout_branch",
            Self::CreateBranch => "create_branch",
            Self::MergeFastForward => "merge_ff_only",
            Self::Rebase => "rebase",
            Self::OpenPullRequest => "open_pull_request",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    BlockedByDefault,
}

impl RiskLevel {
    pub const fn confirmation_requirement(self) -> ConfirmationRequirement {
        match self {
            Self::Low => ConfirmationRequirement::NormalSelection,
            Self::Medium => ConfirmationRequirement::VisiblePlan,
            Self::High => ConfirmationRequirement::ExplicitConfirmation,
            Self::BlockedByDefault => ConfirmationRequirement::Blocked,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfirmationRequirement {
    NormalSelection,
    VisiblePlan,
    ExplicitConfirmation,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmationMetadata {
    pub risk_level: RiskLevel,
    pub requirement: ConfirmationRequirement,
    pub prompt: String,
    pub reason: Option<String>,
}

impl ConfirmationMetadata {
    pub fn for_risk(risk_level: RiskLevel, prompt: impl Into<String>) -> Self {
        Self {
            risk_level,
            requirement: risk_level.confirmation_requirement(),
            prompt: prompt.into(),
            reason: None,
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationStep {
    pub kind: OperationKind,
    pub risk_level: RiskLevel,
    pub summary: String,
    pub details: Vec<String>,
    pub continue_on_failure: bool,
}

impl OperationStep {
    pub fn new(kind: OperationKind, risk_level: RiskLevel, summary: impl Into<String>) -> Self {
        Self {
            kind,
            risk_level,
            summary: summary.into(),
            details: Vec::new(),
            continue_on_failure: false,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.details.push(detail.into());
        self
    }

    pub fn allow_safe_continuation_after_failure(mut self) -> Self {
        self.continue_on_failure = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationPlan {
    pub request: OperationRequest,
    pub title: String,
    pub steps: Vec<OperationStep>,
    pub confirmation: ConfirmationMetadata,
}

impl OperationPlan {
    pub fn new(
        request: OperationRequest,
        title: impl Into<String>,
        steps: Vec<OperationStep>,
        confirmation_prompt: impl Into<String>,
    ) -> Self {
        let risk_level = steps
            .iter()
            .map(|step| step.risk_level)
            .max()
            .unwrap_or(RiskLevel::Low);
        Self {
            request,
            title: title.into(),
            steps,
            confirmation: ConfirmationMetadata::for_risk(risk_level, confirmation_prompt),
        }
    }

    pub fn preview_text(&self) -> String {
        let mut lines = vec![format!("{}:", self.title)];
        for step in &self.steps {
            lines.push(format!("- {}", step.summary));
            lines.extend(step.details.iter().map(|detail| format!("- {detail}")));
        }
        if let Some(reason) = &self.confirmation.reason {
            lines.push(format!("- {reason}"));
        }
        if !self.confirmation.prompt.is_empty() {
            lines.push(self.confirmation.prompt.clone());
        }
        lines.join("\n")
    }

    pub fn first_step(&self) -> Option<&OperationStep> {
        self.steps.first()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_app_identity() {
        assert_eq!(APP_NAME, "bitbygit");
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn risk_levels_match_guardrail_confirmation_policy() {
        assert_eq!(
            RiskLevel::Low.confirmation_requirement(),
            ConfirmationRequirement::NormalSelection
        );
        assert_eq!(
            RiskLevel::Medium.confirmation_requirement(),
            ConfirmationRequirement::VisiblePlan
        );
        assert_eq!(
            RiskLevel::High.confirmation_requirement(),
            ConfirmationRequirement::ExplicitConfirmation
        );
        assert_eq!(
            RiskLevel::BlockedByDefault.confirmation_requirement(),
            ConfirmationRequirement::Blocked
        );
    }

    #[test]
    fn operation_plan_preview_comes_from_steps() {
        let plan = OperationPlan::new(
            OperationRequest::Commit {
                message: "ship it".to_owned(),
            },
            "Commit plan",
            vec![
                OperationStep::new(
                    OperationKind::Commit,
                    RiskLevel::Medium,
                    "commit 2 staged file(s)",
                )
                .with_detail("message: ship it"),
            ],
            "Press y to commit or n to cancel.",
        );

        assert_eq!(plan.confirmation.risk_level, RiskLevel::Medium);
        assert_eq!(
            plan.preview_text(),
            "Commit plan:\n- commit 2 staged file(s)\n- message: ship it\nPress y to commit or n to cancel."
        );
    }
}
