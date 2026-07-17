use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::string::FromUtf8Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use fs2::FileExt;
use sha2::{Digest, Sha256};

#[cfg(windows)]
use command_group::{CommandGroup, GroupChild};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(unix)]
use std::process::Child;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(unix)]
use rustix::process::{Pid, Signal, kill_process_group};
#[cfg(unix)]
use rustix::{
    fs::{Mode, OFlags, open},
    io::Errno,
};

const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const EMPTY_TREE_OID: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const SSH_OPTIONS: &str = "-oBatchMode=yes -oNumberOfPasswordPrompts=0 -oKbdInteractiveAuthentication=no -oStrictHostKeyChecking=yes";
const COMMIT_HOOKS: &[&str] = &[
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
];
const RECOVERY_PLAN_TIMEOUT: Duration = Duration::from_secs(10);
const RECOVERY_START_TIMEOUT: Duration = Duration::from_secs(60);
const RECOVERY_OUTPUT_LIMIT: usize = 256 * 1024;
const RECOVERY_DIAGNOSTIC_LIMIT: usize = 64 * 1024;
const RECOVERY_DIAGNOSTIC_CAPTURE_LIMIT: usize = 64 * 1024 * 1024;
const RECOVERY_STATE_ENTRY_LIMIT: usize = 100_000;
const RECOVERY_UNTRACKED_DATA_LIMIT: u64 = 64 * 1024 * 1024;
const RECOVERY_REBASE_TODO_LIMIT: u64 = 1024 * 1024;
const RECOVERY_MINIMUM_GIT_VERSION: (u64, u64) = (2, 42);
const RECOVERY_LOCK_DIRECTORY: &str = "recovery-locks";
const RECOVERY_HOOKS: &[&str] = &[
    "applypatch-msg",
    "commit-msg",
    "fsmonitor-watchman",
    "p4-changelist",
    "p4-post-changelist",
    "p4-pre-submit",
    "p4-prepare-changelist",
    "post-applypatch",
    "post-checkout",
    "post-commit",
    "post-index-change",
    "post-merge",
    "post-receive",
    "post-rewrite",
    "post-update",
    "pre-applypatch",
    "pre-auto-gc",
    "pre-commit",
    "pre-merge-commit",
    "pre-push",
    "pre-rebase",
    "pre-receive",
    "prepare-commit-msg",
    "proc-receive",
    "push-to-checkout",
    "reference-transaction",
    "sendemail-validate",
    "update",
];
const RECOVERY_CONTROL_PATHS: &[&str] = &[
    "HEAD",
    "index",
    "ORIG_HEAD",
    "MERGE_HEAD",
    "MERGE_MSG",
    "MERGE_MODE",
    "MERGE_AUTOSTASH",
    "MERGE_RR",
    "AUTO_MERGE",
    "SQUASH_MSG",
    "REBASE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_HEAD",
    "rebase-apply",
    "rebase-merge",
    "sequencer",
    "rr-cache",
    "info/attributes",
    "info/sparse-checkout",
];
static NEXT_STAGED_FETCH_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct Git {
    cwd: PathBuf,
    ssh_executable: Option<PathBuf>,
    recovery_plan_timeout: Duration,
    recovery_start_timeout: Duration,
    recovery_output_limit: usize,
    isolated_test_config: bool,
    #[cfg(test)]
    test_global_config: Option<PathBuf>,
    #[cfg(all(test, unix))]
    test_git_exec_path: Option<PathBuf>,
    #[cfg(test)]
    recovery_data_dir: Option<PathBuf>,
}

impl Git {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            ssh_executable: None,
            recovery_plan_timeout: RECOVERY_PLAN_TIMEOUT,
            recovery_start_timeout: RECOVERY_START_TIMEOUT,
            recovery_output_limit: RECOVERY_OUTPUT_LIMIT,
            isolated_test_config: cfg!(test),
            #[cfg(test)]
            test_global_config: None,
            #[cfg(all(test, unix))]
            test_git_exec_path: None,
            #[cfg(test)]
            recovery_data_dir: None,
        }
    }

    pub fn with_ssh_executable(
        cwd: impl Into<PathBuf>,
        ssh_executable: impl Into<PathBuf>,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            ssh_executable: Some(ssh_executable.into()),
            recovery_plan_timeout: RECOVERY_PLAN_TIMEOUT,
            recovery_start_timeout: RECOVERY_START_TIMEOUT,
            recovery_output_limit: RECOVERY_OUTPUT_LIMIT,
            isolated_test_config: cfg!(test),
            #[cfg(test)]
            test_global_config: None,
            #[cfg(all(test, unix))]
            test_git_exec_path: None,
            #[cfg(test)]
            recovery_data_dir: None,
        }
    }

    #[cfg(test)]
    fn with_recovery_limits(
        cwd: impl Into<PathBuf>,
        plan_timeout: Duration,
        start_timeout: Duration,
        output_limit: usize,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            ssh_executable: None,
            recovery_plan_timeout: plan_timeout,
            recovery_start_timeout: start_timeout,
            recovery_output_limit: output_limit,
            isolated_test_config: true,
            test_global_config: None,
            #[cfg(unix)]
            test_git_exec_path: None,
            recovery_data_dir: None,
        }
    }

    #[doc(hidden)]
    pub fn with_isolated_test_config(mut self) -> Self {
        self.isolated_test_config = true;
        self
    }

    #[cfg(test)]
    fn with_test_global_config(mut self, path: impl Into<PathBuf>) -> Self {
        self.isolated_test_config = true;
        self.test_global_config = Some(path.into());
        self
    }

    #[cfg(all(test, unix))]
    fn with_test_git_exec_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.test_git_exec_path = Some(path.into());
        self
    }

    #[cfg(test)]
    fn with_recovery_data_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.recovery_data_dir = Some(path.into());
        self
    }

    pub fn repository(&self) -> Result<Repository, GitError> {
        let root = self.repo_root()?;
        let remotes = self.remotes()?;
        let status = self.status()?;
        let branch = status.branch.clone();

        Ok(Repository {
            root,
            branch,
            remotes,
            status,
        })
    }

    pub fn repo_root(&self) -> Result<PathBuf, GitError> {
        let output = self.run_raw(["rev-parse", "--show-toplevel"])?;
        Ok(path_from_bytes(strip_byte_line_ending(&output.stdout)))
    }

    pub fn branch_state(&self) -> Result<BranchState, GitError> {
        let status = self.status()?;
        Ok(status.branch)
    }

    pub fn remotes(&self) -> Result<Vec<Remote>, GitError> {
        let output = self.run(["remote", "-v"])?;
        Ok(parse_remotes(&output.stdout))
    }

    pub fn upstream(&self) -> Result<Option<String>, GitError> {
        Ok(self.status()?.branch.upstream)
    }

    pub fn fetch_default_remote(&self) -> Result<GitOutput, GitError> {
        self.run(["fetch"])
    }

    pub fn branches(&self) -> Result<Vec<BranchInfo>, GitError> {
        let output = self.run_args(vec![
            "for-each-ref".to_owned(),
            "--format=%(refname)%00%(objectname)%00%(upstream:short)%00%(HEAD)".to_owned(),
            "refs/heads".to_owned(),
            "refs/remotes".to_owned(),
        ])?;
        parse_branches(&output.stdout)
    }

    pub fn branch_target(&self, name: &str) -> Result<Option<BranchTarget>, GitError> {
        let matches = self
            .branches()?
            .into_iter()
            .filter(|branch| branch.name == name)
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return Err(GitError::Blocked {
                message: format!("branch name {name} is ambiguous between local and remote refs"),
            });
        }
        Ok(matches.into_iter().next().map(|branch| BranchTarget {
            name: branch.name,
            reference: branch.reference,
            oid: branch.oid,
            kind: branch.kind,
        }))
    }

    pub fn checkout_branch(
        &self,
        branch: &BranchTarget,
        expected_target: &HeadTarget,
    ) -> Result<GitOutput, GitError> {
        self.ensure_clean_worktree("checkout")?;
        self.ensure_head_target_unchanged(expected_target, "checkout")?;
        self.ensure_branch_target_unchanged(branch, "checkout")?;
        match branch.kind {
            BranchKind::Local => self.run_args(vec![
                "switch".to_owned(),
                "--".to_owned(),
                branch.name.clone(),
            ]),
            BranchKind::Remote => {
                self.ensure_remote_checkout_target_available(branch)?;
                self.run_args(vec![
                    "switch".to_owned(),
                    "--track".to_owned(),
                    "--".to_owned(),
                    branch.name.clone(),
                ])
            }
        }
    }

    pub fn create_branch(
        &self,
        branch: &str,
        base: Option<&BranchTarget>,
        expected_target: &HeadTarget,
    ) -> Result<GitOutput, GitError> {
        self.ensure_valid_new_branch_name(branch)?;
        self.ensure_clean_worktree("create branch")?;
        self.ensure_head_target_unchanged(expected_target, "create branch")?;
        if base.is_none() && expected_target.oid.is_none() {
            return Err(GitError::Blocked {
                message: "create branch is blocked because the current branch has no commit"
                    .to_owned(),
            });
        }
        if self.branch_target(branch)?.is_some() {
            return Err(GitError::Blocked {
                message: format!("create branch is blocked because {branch} already exists"),
            });
        }
        if let Some(base) = base {
            self.ensure_branch_target_unchanged(base, "create branch")?;
        }
        let start = base
            .map(|base| base.oid.clone())
            .or_else(|| expected_target.oid.clone())
            .ok_or_else(|| GitError::Blocked {
                message: "create branch is blocked because the current branch has no commit"
                    .to_owned(),
            })?;
        self.run_args(vec![
            "switch".to_owned(),
            "-c".to_owned(),
            branch.to_owned(),
            "--".to_owned(),
            start,
        ])
    }

    pub fn merge_ff_only(
        &self,
        branch: &BranchTarget,
        expected_target: &HeadTarget,
    ) -> Result<GitOutput, GitError> {
        self.ensure_clean_worktree("merge")?;
        self.ensure_head_target_unchanged(expected_target, "merge")?;
        self.ensure_branch_target_unchanged(branch, "merge")?;
        let Some(current_oid) = &expected_target.oid else {
            return Err(GitError::Blocked {
                message: "merge is blocked because the current branch has no commit".to_owned(),
            });
        };
        self.ensure_ancestor(current_oid, &branch.oid, "merge")?;
        self.run_args(vec![
            "merge".to_owned(),
            "--ff-only".to_owned(),
            branch.oid.clone(),
        ])
    }

    pub fn rebase_onto(
        &self,
        base: &BranchTarget,
        expected_target: &HeadTarget,
    ) -> Result<GitOutput, GitError> {
        self.ensure_clean_worktree("rebase")?;
        self.ensure_head_target_unchanged(expected_target, "rebase")?;
        self.ensure_branch_target_unchanged(base, "rebase")?;
        self.run_args(vec!["rebase".to_owned(), base.oid.clone()])
    }

    pub fn recover(
        &self,
        operation: RepositoryOperation,
        action: RecoveryAction,
    ) -> Result<GitOutput, GitError> {
        ensure_recovery_execution_supported(operation, action)?;
        let deadline = Instant::now() + self.recovery_start_timeout;
        self.ensure_recovery_git_version_until(deadline)?;
        self.validate_recovery_until(operation, action, deadline)?;
        self.ensure_recovery_process_configuration_safe_until(deadline)?;
        let recovery_lock = self.acquire_recovery_lock_until(operation, action, deadline)?;
        self.run_recovery_args_until_with(operation, action, deadline, || {
            let repository_root = self.canonical_repository_root_until(deadline)?;
            recovery_lock.ensure_identity(&repository_root, operation, action)?;
            self.validate_recovery_until(operation, action, deadline)?;
            self.recovery_state_fenced_for_until(operation, action, deadline, || {
                let repository_root = self.canonical_repository_root_until(deadline)?;
                recovery_lock.ensure_identity(&repository_root, operation, action)
            })?;
            Ok(())
        })
    }

    pub fn recovery_state(&self) -> Result<RecoveryState, GitError> {
        ensure_recovery_planning_supported()?;
        let deadline = Instant::now() + self.recovery_plan_timeout;
        self.ensure_recovery_git_version_until(deadline)?;
        self.ensure_recovery_process_configuration_safe_until(deadline)?;
        self.recovery_state_until(deadline)
    }

    pub fn prepare_recovery(
        &self,
        operation: RepositoryOperation,
        action: RecoveryAction,
    ) -> Result<RecoveryState, GitError> {
        ensure_recovery_planning_supported()?;
        let deadline = Instant::now() + self.recovery_plan_timeout;
        self.ensure_recovery_git_version_until(deadline)?;
        self.ensure_recovery_process_configuration_safe_until(deadline)?;
        self.validate_recovery_until(operation, action, deadline)?;
        self.recovery_state_fenced_for_until(operation, action, deadline, || Ok(()))
    }

    pub fn validate_recovery(
        &self,
        operation: RepositoryOperation,
        action: RecoveryAction,
    ) -> Result<(), GitError> {
        ensure_recovery_planning_supported()?;
        let deadline = Instant::now() + self.recovery_plan_timeout;
        self.ensure_recovery_git_version_until(deadline)?;
        self.ensure_recovery_process_configuration_safe_until(deadline)?;
        self.validate_recovery_until(operation, action, deadline)
    }

    pub fn recover_exact(
        &self,
        operation: RepositoryOperation,
        action: RecoveryAction,
        expected_state: &RecoveryState,
    ) -> Result<GitOutput, GitError> {
        self.recover_exact_with(operation, action, expected_state, || Ok(()))
    }

    fn recover_exact_with<F>(
        &self,
        operation: RepositoryOperation,
        action: RecoveryAction,
        expected_state: &RecoveryState,
        before_spawn_check: F,
    ) -> Result<GitOutput, GitError>
    where
        F: FnOnce() -> Result<(), GitError>,
    {
        ensure_recovery_execution_supported(operation, action)?;
        let deadline = Instant::now() + self.recovery_start_timeout;
        self.ensure_recovery_git_version_until(deadline)?;
        self.validate_recovery_action(operation, action)?;
        self.ensure_recovery_process_configuration_safe_until(deadline)?;
        let recovery_lock = self.acquire_recovery_lock_until(operation, action, deadline)?;
        self.run_recovery_args_until_with(operation, action, deadline, || {
            let repository_root = self.canonical_repository_root_until(deadline)?;
            recovery_lock.ensure_identity(&repository_root, operation, action)?;
            let current_state =
                match self.recovery_state_fenced_for_until(operation, action, deadline, || {
                    before_spawn_check()?;
                    let repository_root = self.canonical_repository_root_until(deadline)?;
                    recovery_lock.ensure_identity(&repository_root, operation, action)
                }) {
                    Ok(state) => state,
                    Err(GitError::Blocked { message })
                        if message.contains("changed while it was fingerprinted") =>
                    {
                        return Err(recovery_state_changed(operation, action));
                    }
                    Err(error) => return Err(error),
                };
            if current_state != *expected_state {
                return Err(recovery_state_changed(operation, action));
            }
            Ok(())
        })
    }

    #[cfg(test)]
    fn ensure_recovery_state(
        &self,
        expected_state: &RecoveryState,
        operation: RepositoryOperation,
        action: RecoveryAction,
        deadline: Instant,
    ) -> Result<(), GitError> {
        if self.recovery_state_fenced_for_until(operation, action, deadline, || Ok(()))?
            == *expected_state
        {
            return Ok(());
        }
        Err(recovery_state_changed(operation, action))
    }

    fn validate_recovery_until(
        &self,
        operation: RepositoryOperation,
        action: RecoveryAction,
        deadline: Instant,
    ) -> Result<(), GitError> {
        self.validate_recovery_action(operation, action)?;
        let active = self.repository_operation_until(deadline)?;
        match active {
            Some(active) if active == operation => {}
            Some(active) => {
                return Err(GitError::Blocked {
                    message: format!(
                        "{} {} is blocked because a {} operation is active",
                        operation.label(),
                        action.label(),
                        active.label()
                    ),
                });
            }
            None => {
                return Err(GitError::Blocked {
                    message: format!(
                        "{} {} is blocked because no {} operation is active",
                        operation.label(),
                        action.label(),
                        operation.label()
                    ),
                });
            }
        }
        if action == RecoveryAction::Continue && self.has_unresolved_conflicts_until(deadline)? {
            return Err(GitError::Blocked {
                message: format!(
                    "{} continue is blocked while unresolved conflicts are present",
                    operation.label()
                ),
            });
        }
        Ok(())
    }

    fn validate_recovery_action(
        &self,
        operation: RepositoryOperation,
        action: RecoveryAction,
    ) -> Result<(), GitError> {
        if operation == RepositoryOperation::Merge && action == RecoveryAction::Skip {
            return Err(GitError::Blocked {
                message: "merge skip is blocked because Git does not support it".to_owned(),
            });
        }
        Ok(())
    }

    pub fn push_current_branch(
        &self,
        remote: &str,
        branch: &str,
        source_oid: &str,
        expected_remote_oid: Option<&str>,
    ) -> Result<GitOutput, GitError> {
        self.ensure_push_is_fast_forward(source_oid, expected_remote_oid)?;
        let mut args = vec![
            "push".to_owned(),
            force_with_lease_arg(branch, expected_remote_oid),
        ];
        args.extend([
            "--".to_owned(),
            remote.to_owned(),
            format!("{source_oid}:refs/heads/{branch}"),
        ]);
        self.run_args(args)
    }

    pub fn push_current_branch_set_upstream(
        &self,
        remote: &str,
        branch: &str,
        source_oid: &str,
        expected_remote_oid: Option<&str>,
    ) -> Result<GitOutput, GitError> {
        let push = self.push_current_branch(remote, branch, source_oid, expected_remote_oid)?;
        let upstream = self.run_args(vec![
            "branch".to_owned(),
            format!("--set-upstream-to={}", remote_tracking_ref(remote, branch)),
            branch.to_owned(),
        ])?;
        Ok(combine_outputs(push, upstream))
    }

    fn ensure_push_is_fast_forward(
        &self,
        source_oid: &str,
        expected_remote_oid: Option<&str>,
    ) -> Result<(), GitError> {
        let Some(expected_remote_oid) = expected_remote_oid else {
            return Ok(());
        };
        match self.run_args(vec![
            "merge-base".to_owned(),
            "--is-ancestor".to_owned(),
            expected_remote_oid.to_owned(),
            source_oid.to_owned(),
        ]) {
            Ok(_output) => Ok(()),
            Err(GitError::GitFailed { status, .. }) if status.code() == Some(1) => {
                Err(GitError::Blocked {
                    message: "push is blocked because it would not fast-forward the planned remote"
                        .to_owned(),
                })
            }
            Err(error) => Err(error),
        }
    }

    pub fn remote_head_oid(&self, remote: &str, branch: &str) -> Result<Option<String>, GitError> {
        self.head_oid_at(remote, branch)
    }

    pub fn remote_url_head_oid(&self, url: &str, branch: &str) -> Result<Option<String>, GitError> {
        self.head_oid_at(url, branch)
    }

    fn head_oid_at(&self, target: &str, branch: &str) -> Result<Option<String>, GitError> {
        let output = self.run_args(vec![
            "ls-remote".to_owned(),
            "--heads".to_owned(),
            "--".to_owned(),
            target.to_owned(),
            format!("refs/heads/{branch}"),
        ])?;
        Ok(output
            .stdout
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().next())
            .map(ToOwned::to_owned))
    }

    pub fn remote_push_urls(&self, remote: &str) -> Result<Vec<String>, GitError> {
        let output = self.run_args(vec![
            "remote".to_owned(),
            "get-url".to_owned(),
            "--push".to_owned(),
            "--all".to_owned(),
            "--".to_owned(),
            remote.to_owned(),
        ])?;
        Ok(output
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    pub fn pull(&self) -> Result<GitOutput, GitError> {
        self.run(["pull", "--ff-only"])
    }

    pub fn pull_ff_only_from(
        &self,
        remote: &str,
        branch: &str,
        expected_upstream_oid: Option<&str>,
        expected_target: &HeadTarget,
    ) -> Result<GitOutput, GitError> {
        self.ensure_remote_tracking_unchanged(remote, branch, expected_upstream_oid, "pull")?;
        self.ensure_head_target_unchanged(expected_target, "pull")?;
        let merge_target = expected_upstream_oid.ok_or_else(|| GitError::Blocked {
            message: "pull is blocked because the upstream ref is unavailable".to_owned(),
        })?;
        self.run_args(vec![
            "merge".to_owned(),
            "--ff-only".to_owned(),
            merge_target.to_owned(),
        ])
    }

    pub fn pull_rebase(&self) -> Result<GitOutput, GitError> {
        self.run(["pull", "--rebase"])
    }

    pub fn pull_rebase_from(
        &self,
        remote: &str,
        branch: &str,
        expected_upstream_oid: Option<&str>,
        expected_target: &HeadTarget,
    ) -> Result<GitOutput, GitError> {
        self.ensure_remote_tracking_unchanged(
            remote,
            branch,
            expected_upstream_oid,
            "pull rebase",
        )?;
        self.ensure_head_target_unchanged(expected_target, "pull rebase")?;
        let rebase_target = expected_upstream_oid.ok_or_else(|| GitError::Blocked {
            message: "pull rebase is blocked because the upstream ref is unavailable".to_owned(),
        })?;
        self.run_args(vec!["rebase".to_owned(), rebase_target.to_owned()])
    }

    pub fn upstream_push_target(&self, branch: &str) -> Result<Option<(String, String)>, GitError> {
        let remote = self.config_value(["config", "--get", &format!("branch.{branch}.remote")])?;
        let merge = self.config_value(["config", "--get", &format!("branch.{branch}.merge")])?;
        let (Some(remote), Some(merge)) = (remote, merge) else {
            return Ok(None);
        };
        let branch = merge
            .strip_prefix("refs/heads/")
            .unwrap_or(merge.as_str())
            .to_owned();
        Ok(Some((remote, branch)))
    }

    pub fn push_target(&self, branch: &str) -> Result<Option<(String, String)>, GitError> {
        let output = self.run_args(vec![
            "for-each-ref".to_owned(),
            "--format=%(push:remotename)%00%(push:short)".to_owned(),
            "--count=1".to_owned(),
            "--".to_owned(),
            format!("refs/heads/{branch}"),
        ])?;
        let Some((remote, target)) = output.stdout.trim().split_once('\0') else {
            return self.upstream_push_target(branch);
        };
        if !remote.is_empty() && target.is_empty() {
            let push_default = self.config_value(["config", "--get", "push.default"])?;
            let upstream = self.upstream_push_target(branch)?;
            if push_default.as_deref().unwrap_or("simple") == "simple" {
                return Ok(upstream.map(|(upstream_remote, upstream_branch)| {
                    let push_branch = if upstream_remote == remote {
                        upstream_branch
                    } else {
                        branch.to_owned()
                    };
                    (remote.to_owned(), push_branch)
                }));
            }
            return Ok(None);
        }
        let Some(push_branch) = target.strip_prefix(&format!("{remote}/")) else {
            return self.upstream_push_target(branch);
        };
        if remote.is_empty() || push_branch.is_empty() {
            return self.upstream_push_target(branch);
        }
        Ok(Some((remote.to_owned(), push_branch.to_owned())))
    }

    pub fn typed_push_target(
        &self,
        branch: &str,
        upstream: Option<(&str, &str)>,
        default_remote: Option<&str>,
    ) -> Result<Option<(String, String)>, GitError> {
        let remote = self
            .config_value(["config", "--get", &format!("branch.{branch}.pushRemote")])?
            .or(self.config_value(["config", "--get", "remote.pushDefault"])?)
            .or_else(|| upstream.map(|(remote, _)| remote.to_owned()))
            .or_else(|| default_remote.map(ToOwned::to_owned));
        let Some(remote) = remote else {
            return Ok(None);
        };
        if remote.is_empty() {
            return Ok(None);
        }
        let Some((upstream_remote, upstream_branch)) = upstream else {
            return Ok(Some((remote, branch.to_owned())));
        };
        let push_default = self
            .config_value(["config", "--get", "push.default"])?
            .unwrap_or_else(|| "simple".to_owned());
        let target = match push_default.as_str() {
            "simple" if remote != upstream_remote => (remote, branch.to_owned()),
            "simple" | "upstream" | "tracking" => {
                (upstream_remote.to_owned(), upstream_branch.to_owned())
            }
            "current" | "matching" => (remote, branch.to_owned()),
            "nothing" => return Ok(None),
            _ => {
                return Err(GitError::Blocked {
                    message: format!("push.default has unsupported value {push_default}"),
                });
            }
        };
        Ok(Some(target))
    }

    pub fn remote_tracking_oid(
        &self,
        remote: &str,
        branch: &str,
    ) -> Result<Option<String>, GitError> {
        self.ref_oid(&remote_tracking_ref(remote, branch))
    }

    pub fn fetch_remote_branch_for_plan(
        &self,
        remote: &str,
        branch: &str,
    ) -> Result<String, GitError> {
        let fetch_id = NEXT_STAGED_FETCH_ID.fetch_add(1, Ordering::Relaxed);
        let staged_ref = format!("refs/bitbygit/fetch/{}-{fetch_id}", std::process::id());
        let fetch = self.run_args(vec![
            "fetch".to_owned(),
            "--no-write-fetch-head".to_owned(),
            "--no-tags".to_owned(),
            "--refmap=".to_owned(),
            "--".to_owned(),
            remote.to_owned(),
            format!("+refs/heads/{branch}:{staged_ref}"),
        ]);
        if let Err(error) = fetch {
            let _cleanup =
                self.run_args(vec!["update-ref".to_owned(), "-d".to_owned(), staged_ref]);
            return Err(error);
        }

        let oid = self.ref_oid(&staged_ref).and_then(|oid| {
            oid.ok_or_else(|| GitError::Parse {
                message: "staged fetch did not produce a branch target".to_owned(),
            })
        });
        self.run_args(vec!["update-ref".to_owned(), "-d".to_owned(), staged_ref])?;
        oid
    }

    pub fn ahead_behind(&self, local: &str, upstream: &str) -> Result<(u32, u32), GitError> {
        let output = self.run_args(vec![
            "rev-list".to_owned(),
            "--left-right".to_owned(),
            "--count".to_owned(),
            format!("{local}...{upstream}"),
        ])?;
        let mut counts = output.stdout.split_whitespace();
        let ahead = counts
            .next()
            .ok_or_else(|| parse_error("missing ahead count"))?
            .parse()
            .map_err(|_| parse_error("ahead count is not a number"))?;
        let behind = counts
            .next()
            .ok_or_else(|| parse_error("missing behind count"))?
            .parse()
            .map_err(|_| parse_error("behind count is not a number"))?;
        if counts.next().is_some() {
            return Err(parse_error("unexpected extra ahead/behind count"));
        }
        Ok((ahead, behind))
    }

    pub fn publish_remote_tracking(
        &self,
        remote: &str,
        branch: &str,
        oid: &str,
        expected_oid: Option<&str>,
    ) -> Result<GitOutput, GitError> {
        self.run_args(vec![
            "update-ref".to_owned(),
            "-m".to_owned(),
            "bitbygit pull".to_owned(),
            remote_tracking_ref(remote, branch),
            oid.to_owned(),
            expected_oid.unwrap_or(ZERO_OID).to_owned(),
        ])
    }

    pub fn fetch_remote_branch(&self, remote: &str, branch: &str) -> Result<GitOutput, GitError> {
        self.run_args(vec![
            "fetch".to_owned(),
            "--".to_owned(),
            remote.to_owned(),
            format!(
                "+refs/heads/{branch}:{}",
                remote_tracking_ref(remote, branch)
            ),
        ])
    }

    fn ensure_remote_tracking_unchanged(
        &self,
        remote: &str,
        branch: &str,
        expected_oid: Option<&str>,
        operation: &str,
    ) -> Result<(), GitError> {
        if self.remote_tracking_oid(remote, branch)?.as_deref() != expected_oid {
            return Err(GitError::Blocked {
                message: format!(
                    "{operation} is blocked because the remote changed since the plan was shown"
                ),
            });
        }
        Ok(())
    }

    fn ensure_head_target_unchanged(
        &self,
        expected_target: &HeadTarget,
        operation: &str,
    ) -> Result<(), GitError> {
        if self.head_target()? != *expected_target {
            return Err(GitError::Blocked {
                message: format!(
                    "{operation} is blocked because the branch target changed since the plan was shown"
                ),
            });
        }
        Ok(())
    }

    pub fn ensure_clean_worktree(&self, operation: &str) -> Result<(), GitError> {
        if let Some(in_progress) = self.in_progress_operation()? {
            return Err(GitError::Blocked {
                message: format!(
                    "{operation} is blocked because a {in_progress} operation is in progress"
                ),
            });
        }
        let status = self.status()?;
        if !status.conflicted_files().is_empty() {
            return Err(GitError::Blocked {
                message: format!("{operation} is blocked while conflicts are present"),
            });
        }
        if !status.is_clean() {
            return Err(GitError::Blocked {
                message: format!("{operation} is blocked because the working tree is not clean"),
            });
        }
        Ok(())
    }

    fn in_progress_operation(&self) -> Result<Option<&'static str>, GitError> {
        if let Some(operation) = self.repository_operation()? {
            return Ok(Some(operation.label()));
        }
        for (operation, marker) in [
            ("am", "rebase-apply/applying"),
            ("cherry-pick", "CHERRY_PICK_HEAD"),
            ("revert", "REVERT_HEAD"),
        ] {
            if self.git_path(marker)?.exists() {
                return Ok(Some(operation));
            }
        }
        Ok(None)
    }

    fn repository_operation(&self) -> Result<Option<RepositoryOperation>, GitError> {
        let rebase_apply = self.git_path("rebase-apply")?;
        if self.git_path("rebase-merge")?.exists()
            || (rebase_apply.exists() && !rebase_apply.join("applying").exists())
        {
            return Ok(Some(RepositoryOperation::Rebase));
        }
        if self.git_path("MERGE_HEAD")?.exists() {
            return Ok(Some(RepositoryOperation::Merge));
        }
        Ok(None)
    }

    fn git_path(&self, path: &str) -> Result<PathBuf, GitError> {
        let output = self.run_args(vec![
            "rev-parse".to_owned(),
            "--git-path".to_owned(),
            path.to_owned(),
        ])?;
        let path = PathBuf::from(output.stdout.trim());
        Ok(if path.is_absolute() {
            path
        } else {
            self.cwd.join(path)
        })
    }

    fn ensure_branch_target_unchanged(
        &self,
        expected_branch: &BranchTarget,
        operation: &str,
    ) -> Result<(), GitError> {
        match self.branch_target(&expected_branch.name)? {
            Some(branch) if branch == *expected_branch => Ok(()),
            Some(_branch) => Err(GitError::Blocked {
                message: format!(
                    "{operation} is blocked because {} changed since the plan was shown",
                    expected_branch.name
                ),
            }),
            None => Err(GitError::Blocked {
                message: format!(
                    "{operation} is blocked because {} no longer exists",
                    expected_branch.name
                ),
            }),
        }
    }

    pub fn ensure_remote_checkout_target_available(
        &self,
        branch: &BranchTarget,
    ) -> Result<(), GitError> {
        if branch.kind != BranchKind::Remote {
            return Ok(());
        }
        let Some(local_name) = local_name_for_remote_branch(&branch.name) else {
            return Err(GitError::Blocked {
                message: format!(
                    "checkout is blocked because remote branch {} has no local branch name",
                    branch.name
                ),
            });
        };
        if matches!(self.branch_target(local_name)?, Some(existing) if existing.kind == BranchKind::Local)
        {
            return Err(GitError::Blocked {
                message: format!(
                    "checkout is blocked because local branch {local_name} already exists; checkout {local_name} instead"
                ),
            });
        }
        Ok(())
    }

    fn ensure_valid_new_branch_name(&self, branch: &str) -> Result<(), GitError> {
        if branch.starts_with('-') || branch.trim() != branch || branch.is_empty() {
            return Err(GitError::Blocked {
                message: "create branch is blocked because the branch name is invalid".to_owned(),
            });
        }
        match self.run_args(vec![
            "check-ref-format".to_owned(),
            "--branch".to_owned(),
            branch.to_owned(),
        ]) {
            Ok(_output) => Ok(()),
            Err(GitError::GitFailed { status, .. }) if status.code() == Some(1) => {
                Err(GitError::Blocked {
                    message: "create branch is blocked because the branch name is invalid"
                        .to_owned(),
                })
            }
            Err(error) => Err(error),
        }
    }

    fn ensure_ancestor(
        &self,
        ancestor: &str,
        descendant: &str,
        operation: &str,
    ) -> Result<(), GitError> {
        match self.run_args(vec![
            "merge-base".to_owned(),
            "--is-ancestor".to_owned(),
            ancestor.to_owned(),
            descendant.to_owned(),
        ]) {
            Ok(_output) => Ok(()),
            Err(GitError::GitFailed { status, .. }) if status.code() == Some(1) => {
                Err(GitError::Blocked {
                    message: format!(
                        "{operation} is blocked because it cannot fast-forward cleanly"
                    ),
                })
            }
            Err(error) => Err(error),
        }
    }

    fn ref_oid(&self, reference: &str) -> Result<Option<String>, GitError> {
        match self.run_args(vec![
            "rev-parse".to_owned(),
            "--verify".to_owned(),
            reference.to_owned(),
        ]) {
            Ok(output) => Ok(Some(output.stdout.trim().to_owned())),
            Err(GitError::GitFailed { status, .. }) if status.code() == Some(1) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn status(&self) -> Result<WorktreeStatus, GitError> {
        let output = self.run_raw(["status", "--porcelain=v2", "--branch", "-z"])?;
        let mut status = parse_status_bytes(&output.stdout)?;
        status.operation = self.repository_operation()?;
        Ok(status)
    }

    pub fn stage_path(&self, path: &Path) -> Result<GitOutput, GitError> {
        self.run_path_args(["add"], Some(path), true)
    }

    pub fn stage_paths(&self, paths: &[PathBuf]) -> Result<GitOutput, GitError> {
        self.run_paths_args(["add"], paths, true)
    }

    pub fn unstage_path(&self, path: &Path) -> Result<GitOutput, GitError> {
        if self.is_unborn()? {
            return self.run_path_args(
                ["rm", "--cached", "--ignore-unmatch", "-r"],
                Some(path),
                true,
            );
        }
        self.run_path_args(["restore", "--staged"], Some(path), true)
    }

    pub fn unstage_paths(&self, paths: &[PathBuf]) -> Result<GitOutput, GitError> {
        if self.is_unborn()? {
            return self.run_paths_args(["rm", "--cached", "--ignore-unmatch", "-r"], paths, true);
        }
        self.run_paths_args(["restore", "--staged"], paths, true)
    }

    pub fn stage_all(&self) -> Result<GitOutput, GitError> {
        self.run(["add", "--all"])
    }

    pub fn unstage_all(&self) -> Result<GitOutput, GitError> {
        if self.is_unborn()? {
            return self.run(["rm", "--cached", "--ignore-unmatch", "-r", ":/"]);
        }
        self.run(["restore", "--staged", ":/"])
    }

    pub fn commit(&self, message: &str) -> Result<GitOutput, GitError> {
        let staged_tree = self.staged_tree()?;
        let target = self.head_target()?;
        self.commit_staged_tree(message, &staged_tree, &target)
    }

    pub fn commit_staged_tree(
        &self,
        message: &str,
        staged_tree: &str,
        target: &HeadTarget,
    ) -> Result<GitOutput, GitError> {
        if self.head_target()? != *target {
            return Err(GitError::Blocked {
                message: "guarded commit is blocked because the target ref changed".to_owned(),
            });
        }
        self.ensure_staged_tree_is_not_empty_commit(staged_tree, target)?;
        self.ensure_guarded_commit_supported()?;
        let parent = target.oid.clone();
        let mut commit_args = vec!["commit-tree".to_owned(), staged_tree.to_owned()];
        if let Some(parent) = &parent {
            commit_args.push("-p".to_owned());
            commit_args.push(parent.clone());
        }
        commit_args.push("-m".to_owned());
        commit_args.push(message.to_owned());

        let commit_output = self.run_args(commit_args)?;
        let commit_id = commit_output.stdout.trim().to_owned();
        if commit_id.is_empty() {
            return Err(GitError::Parse {
                message: "git commit-tree did not return a commit id".to_owned(),
            });
        }

        let mut update_args = vec![
            "update-ref".to_owned(),
            "-m".to_owned(),
            "bitbygit commit".to_owned(),
            target.reference.as_deref().unwrap_or("HEAD").to_owned(),
            commit_id.clone(),
        ];
        update_args.push(parent.unwrap_or_else(|| ZERO_OID.to_owned()));
        let update_output = self.run_args(update_args)?;
        let short_id = commit_id.chars().take(12).collect::<String>();
        let stderr = [commit_output.stderr.trim(), update_output.stderr.trim()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n");

        Ok(GitOutput {
            status: update_output.status,
            stdout: format!("[{short_id}] {message}\n"),
            stderr,
        })
    }

    pub fn staged_tree(&self) -> Result<String, GitError> {
        Ok(self.run(["write-tree"])?.stdout.trim().to_owned())
    }

    pub fn diff_path(&self, path: &Path, staged: bool) -> Result<String, GitError> {
        self.diff_paths(&[path.to_path_buf()], staged)
    }

    pub fn diff_paths(&self, paths: &[PathBuf], staged: bool) -> Result<String, GitError> {
        let args = if staged {
            vec![
                OsString::from("diff"),
                OsString::from("--cached"),
                OsString::from("--"),
            ]
        } else {
            vec![OsString::from("diff"), OsString::from("--")]
        };
        let output = self.run_os_paths(args, paths, true)?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn run<const N: usize>(&self, args: [&str; N]) -> Result<GitOutput, GitError> {
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        self.run_args(args)
    }

    fn is_unborn(&self) -> Result<bool, GitError> {
        Ok(self.branch_state()?.unborn)
    }

    pub fn head_commit(&self) -> Result<Option<String>, GitError> {
        if self.is_unborn()? {
            return Ok(None);
        }
        Ok(Some(
            self.run(["rev-parse", "--verify", "HEAD"])?
                .stdout
                .trim()
                .to_owned(),
        ))
    }

    pub fn head_target(&self) -> Result<HeadTarget, GitError> {
        Ok(HeadTarget {
            oid: self.head_commit()?,
            reference: self.symbolic_head()?,
        })
    }

    fn symbolic_head(&self) -> Result<Option<String>, GitError> {
        match self.run(["symbolic-ref", "--quiet", "HEAD"]) {
            Ok(output) => Ok(Some(output.stdout.trim().to_owned())),
            Err(GitError::GitFailed { status, .. }) if status.code() == Some(1) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn ensure_staged_tree_is_not_empty_commit(
        &self,
        staged_tree: &str,
        target: &HeadTarget,
    ) -> Result<(), GitError> {
        let parent_tree = match &target.oid {
            Some(parent) => self
                .run_args(vec!["rev-parse".to_owned(), format!("{parent}^{{tree}}")])?
                .stdout
                .trim()
                .to_owned(),
            None => EMPTY_TREE_OID.to_owned(),
        };

        if parent_tree == staged_tree {
            return Err(GitError::Blocked {
                message: "guarded commit is blocked because there are no staged changes".to_owned(),
            });
        }

        Ok(())
    }

    fn ensure_guarded_commit_supported(&self) -> Result<(), GitError> {
        if self.config_bool("commit.gpgsign")? {
            return Err(GitError::Blocked {
                message: "guarded commit is blocked because commit.gpgsign is enabled".to_owned(),
            });
        }

        let hooks = COMMIT_HOOKS
            .iter()
            .filter_map(|hook| match self.hook_path(hook) {
                Ok(path) if hook_is_enabled(&path) => Some(Ok((*hook).to_owned())),
                Ok(_path) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !hooks.is_empty() {
            return Err(GitError::Blocked {
                message: format!(
                    "guarded commit is blocked because commit hook(s) are configured: {}",
                    hooks.join(", ")
                ),
            });
        }

        Ok(())
    }

    fn config_bool(&self, key: &str) -> Result<bool, GitError> {
        Ok(matches!(
            self.config_value(["config", "--bool", "--get", key])?
                .as_deref(),
            Some("true") | Some("yes") | Some("on") | Some("1")
        ))
    }

    fn config_value<const N: usize>(&self, args: [&str; N]) -> Result<Option<String>, GitError> {
        match self.run_args(args.iter().map(|arg| (*arg).to_owned()).collect()) {
            Ok(output) => Ok(Some(output.stdout.trim().to_owned())),
            Err(GitError::GitFailed { status, .. }) if status.code() == Some(1) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn hook_path(&self, hook: &str) -> Result<PathBuf, GitError> {
        if let Some(hooks_path) =
            self.config_value(["config", "--path", "--get", "core.hooksPath"])?
        {
            let path = PathBuf::from(hooks_path);
            return Ok(if path.is_absolute() {
                path.join(hook)
            } else {
                self.repo_root()?.join(path).join(hook)
            });
        }

        let output = self.run_args(vec![
            "rev-parse".to_owned(),
            "--git-path".to_owned(),
            format!("hooks/{hook}"),
        ])?;
        let path = PathBuf::from(output.stdout.trim());
        Ok(if path.is_absolute() {
            path
        } else {
            self.cwd.join(path)
        })
    }

    fn recovery_state_until(&self, deadline: Instant) -> Result<RecoveryState, GitError> {
        self.recovery_state_fenced_until(deadline, || Ok(()))
    }

    fn recovery_state_fenced_until<F>(
        &self,
        deadline: Instant,
        after_index: F,
    ) -> Result<RecoveryState, GitError>
    where
        F: FnOnce() -> Result<(), GitError>,
    {
        self.recovery_state_fenced_until_with(None, deadline, after_index)
    }

    fn recovery_state_fenced_for_until<F>(
        &self,
        operation: RepositoryOperation,
        action: RecoveryAction,
        deadline: Instant,
        after_index: F,
    ) -> Result<RecoveryState, GitError>
    where
        F: FnOnce() -> Result<(), GitError>,
    {
        self.recovery_state_fenced_until_with(Some((operation, action)), deadline, after_index)
    }

    fn recovery_state_fenced_until_with<F>(
        &self,
        recovery: Option<(RepositoryOperation, RecoveryAction)>,
        deadline: Instant,
        after_index: F,
    ) -> Result<RecoveryState, GitError>
    where
        F: FnOnce() -> Result<(), GitError>,
    {
        let mut fence = RecoveryInputFence::default();
        let mut after_index = Some(after_index);
        self.recovery_state_until_with(recovery, deadline, Some(&mut fence), &mut after_index)
    }

    fn recovery_state_until_with<F>(
        &self,
        recovery: Option<(RepositoryOperation, RecoveryAction)>,
        deadline: Instant,
        mut fence: Option<&mut RecoveryInputFence>,
        after_index: &mut Option<F>,
    ) -> Result<RecoveryState, GitError>
    where
        F: FnOnce() -> Result<(), GitError>,
    {
        let mut hasher = Sha256::new();
        self.hash_recovery_process_configuration_until(
            deadline,
            &mut hasher,
            fence.as_deref_mut(),
        )?;
        for args in [
            vec![
                "status".to_owned(),
                "--porcelain=v2".to_owned(),
                "--branch".to_owned(),
                "--untracked-files=all".to_owned(),
                "-z".to_owned(),
            ],
            vec![
                "diff".to_owned(),
                "--binary".to_owned(),
                "--no-ext-diff".to_owned(),
                "--no-textconv".to_owned(),
                "--full-index".to_owned(),
                "--".to_owned(),
            ],
            vec![
                "diff".to_owned(),
                "--cached".to_owned(),
                "--binary".to_owned(),
                "--no-ext-diff".to_owned(),
                "--no-textconv".to_owned(),
                "--full-index".to_owned(),
                "--".to_owned(),
            ],
            vec![
                "for-each-ref".to_owned(),
                "--sort=refname".to_owned(),
                "--format=%(refname)%00%(objectname)%00%(symref)%00".to_owned(),
            ],
            vec![
                "config".to_owned(),
                "--null".to_owned(),
                "--list".to_owned(),
                "--show-origin".to_owned(),
                "--show-scope".to_owned(),
            ],
        ] {
            let output = self.run_bounded_git_digest(args.clone(), deadline)?;
            if !output.status.success() {
                return Err(output.git_error(args));
            }
            hash_field(&mut hasher, &output.stdout.digest);
            if let Some(fence) = fence.as_deref_mut() {
                fence.probes.push(RecoveryProbe {
                    args,
                    digest: output.stdout.digest,
                    status: output.status.code(),
                    safety: RecoveryProbeSafety::None,
                });
            }
        }

        let mut entries = 0;
        for relative in RECOVERY_CONTROL_PATHS {
            let path = self.git_path_until(relative, deadline)?;
            hash_recovery_path_fenced(
                &mut hasher,
                relative.as_bytes(),
                &path,
                deadline,
                &mut entries,
                fence.as_deref_mut(),
            )?;
        }
        let hooks =
            self.recovery_hooks_path_fenced_until(deadline, &mut hasher, fence.as_deref_mut())?;
        hash_recovery_path_fenced(
            &mut hasher,
            b"hooks",
            &hooks,
            deadline,
            &mut entries,
            fence.as_deref_mut(),
        )?;
        for variable in ["GIT_ATTR_SYSTEM", "GIT_ATTR_GLOBAL"] {
            if let Some(path) = self.git_var_path_until(variable, deadline)? {
                hash_recovery_path_fenced(
                    &mut hasher,
                    variable.as_bytes(),
                    &path,
                    deadline,
                    &mut entries,
                    fence.as_deref_mut(),
                )?;
            }
        }
        let root = self.repo_root_until(deadline)?;
        let mut untracked_bytes_remaining = Some(RECOVERY_UNTRACKED_DATA_LIMIT);
        for relative in self.untracked_worktree_paths_until(deadline)? {
            let path = root.join(&relative);
            let mut label = b"untracked-worktree/".to_vec();
            label.extend_from_slice(relative.as_os_str().as_encoded_bytes());
            let mut context = RecoveryHashContext {
                deadline,
                entries: &mut entries,
                follow_symlinks: false,
                byte_budget: &mut untracked_bytes_remaining,
                byte_budget_error: "recovery planning is blocked because untracked worktree data exceeds the 64 MiB fingerprint limit",
                fence: fence.as_deref_mut(),
            };
            hash_recovery_path_inner(&mut hasher, &label, &path, 0, &mut context, &mut |_, _| {
                Ok(())
            })?;
        }
        if let Some((operation, action)) = recovery {
            hash_field(&mut hasher, operation.label().as_bytes());
            hash_field(&mut hasher, action.label().as_bytes());
            self.ensure_no_ignored_recovery_collisions_until(
                operation,
                action,
                deadline,
                &mut hasher,
                fence.as_deref_mut(),
            )?;
        }
        for relative in self.worktree_attributes_until(deadline)? {
            let mut label = b"worktree-attributes/".to_vec();
            label.extend_from_slice(relative.as_os_str().as_encoded_bytes());
            hash_recovery_path_fenced(
                &mut hasher,
                &label,
                &root.join(relative),
                deadline,
                &mut entries,
                fence.as_deref_mut(),
            )?;
        }

        if let Some(fence) = fence {
            fence.validate(self, deadline)?;
            if let Some(after_index) = after_index.take() {
                after_index()?;
            }
            fence.validate(self, deadline)?;
        }

        Ok(RecoveryState {
            fingerprint: hasher.finalize().into(),
        })
    }

    fn hash_recovery_process_configuration_until(
        &self,
        deadline: Instant,
        hasher: &mut Sha256,
        mut fence: Option<&mut RecoveryInputFence>,
    ) -> Result<(), GitError> {
        for key in ["core.fsmonitor", "commit.gpgsign", "tag.gpgsign"] {
            let args = vec!["config".to_owned(), "--get".to_owned(), key.to_owned()];
            let output = self.run_bounded_git(args.clone(), deadline, true)?;
            let safety = RecoveryProbeSafety::DisabledConfig(key);
            ensure_recovery_probe_observation_safe(safety, &args, &output)?;
            hash_recovery_probe(hasher, &output);
            if let Some(fence) = fence.as_deref_mut() {
                fence.probes.push(RecoveryProbe {
                    args,
                    digest: output.stdout.digest,
                    status: output.status.code(),
                    safety,
                });
            }
        }

        let args = vec![
            "config".to_owned(),
            "--name-only".to_owned(),
            "--null".to_owned(),
            "--list".to_owned(),
        ];
        let output = self.run_bounded_git(args.clone(), deadline, true)?;
        let safety = RecoveryProbeSafety::ExternalDrivers;
        self.ensure_recovery_external_drivers_safe_until(&args, &output, deadline)?;
        hash_recovery_probe(hasher, &output);
        if let Some(fence) = fence {
            fence.probes.push(RecoveryProbe {
                args,
                digest: output.stdout.digest,
                status: output.status.code(),
                safety,
            });
        }
        Ok(())
    }

    fn ensure_recovery_process_configuration_safe_until(
        &self,
        deadline: Instant,
    ) -> Result<(), GitError> {
        if self.bounded_config_is_enabled("core.fsmonitor", deadline)? {
            return Err(GitError::Blocked {
                message: "recovery is blocked because core.fsmonitor may start an uncontained process; disable it and preview recovery again, or run Git manually"
                    .to_owned(),
            });
        }
        for key in ["commit.gpgsign", "tag.gpgsign"] {
            if self.bounded_config_is_enabled(key, deadline)? {
                return Err(GitError::Blocked {
                    message: format!(
                        "recovery is blocked because {key} may start an uncontained signing process; disable it and preview recovery again, or run Git manually"
                    ),
                });
            }
        }
        for relative in ["rebase-merge/gpg_sign_opt", "rebase-apply/gpg_sign_opt"] {
            let path = self.git_path_until(relative, deadline)?;
            if read_bounded_optional_file(&path, RECOVERY_REBASE_TODO_LIMIT, deadline)?
                .is_some_and(|contents| !contents.trim().is_empty())
            {
                return Err(GitError::Blocked {
                    message: "recovery is blocked because the active rebase requests commit signing with --gpg-sign, which may start an uncontained signer; abort and restart the rebase without signing, or run Git manually"
                        .to_owned(),
                });
            }
        }
        for relative in [
            "rebase-merge/strategy",
            "rebase-apply/strategy",
            "rebase-merge/strategy_opts",
            "rebase-apply/strategy_opts",
        ] {
            let path = self.git_path_until(relative, deadline)?;
            let Some(value) =
                read_bounded_optional_file(&path, RECOVERY_REBASE_TODO_LIMIT, deadline)?
            else {
                continue;
            };
            if !value.trim().is_empty() {
                return Err(GitError::Blocked {
                    message: format!(
                        "recovery is blocked because the active rebase persists an explicit merge strategy or strategy option in {relative}, which may start an uncontained executable; abort and restart without custom strategy settings, or run Git manually"
                    ),
                });
            }
        }
        let args = vec![
            "config".to_owned(),
            "--name-only".to_owned(),
            "--null".to_owned(),
            "--list".to_owned(),
        ];
        let output = self.run_bounded_git(args.clone(), deadline, true)?;
        self.ensure_recovery_external_drivers_safe_until(&args, &output, deadline)?;

        let hooks = self.recovery_hooks_path_until(deadline)?;
        let enabled_hooks = RECOVERY_HOOKS
            .iter()
            .filter(|hook| hook_is_enabled(&hooks.join(hook)))
            .copied()
            .collect::<Vec<_>>();
        if !enabled_hooks.is_empty() {
            return Err(GitError::Blocked {
                message: format!(
                    "recovery is blocked because executable Git hooks may start uncontained processes: {}; disable them and preview recovery again, or run Git manually",
                    enabled_hooks.join(", ")
                ),
            });
        }

        for relative in [
            "rebase-merge/git-rebase-todo",
            "rebase-apply/git-rebase-todo",
        ] {
            let path = self.git_path_until(relative, deadline)?;
            let Some(contents) =
                read_bounded_optional_file(&path, RECOVERY_REBASE_TODO_LIMIT, deadline)?
            else {
                continue;
            };
            if contents
                .lines()
                .any(|line| matches!(line.split_ascii_whitespace().next(), Some("exec" | "x")))
            {
                return Err(GitError::Blocked {
                    message: "recovery is blocked because the remaining rebase plan contains an exec command that may start uncontained processes; remove it and preview recovery again, or run Git manually"
                        .to_owned(),
                });
            }
        }
        Ok(())
    }

    fn ensure_recovery_external_drivers_safe_until(
        &self,
        args: &[String],
        output: &BoundedCommandOutput,
        deadline: Instant,
    ) -> Result<(), GitError> {
        if !output.status.success() {
            return Err(output.clone().git_error(args.to_vec()));
        }
        if output.stdout.truncated {
            return Err(GitError::Blocked {
                message: "recovery is blocked because Git configuration exceeds the bounded diagnostic limit, so external drivers cannot be ruled out"
                    .to_owned(),
            });
        }
        let mut names = Vec::new();
        let mut filter_keys = BTreeMap::<String, Vec<String>>::new();
        for name in output.stdout.bytes.split(|byte| *byte == 0) {
            if !is_recovery_external_driver_config(name) {
                continue;
            }
            let display = String::from_utf8_lossy(name).into_owned();
            if let Some(driver) = recovery_filter_driver(name) {
                filter_keys.entry(driver).or_default().push(display);
            } else {
                names.push(display);
            }
        }
        if !filter_keys.is_empty() {
            let configured = filter_keys.keys().cloned().collect();
            for driver in self.active_recovery_filters_until(&configured, deadline)? {
                if let Some(keys) = filter_keys.get(&driver) {
                    names.extend(keys.iter().cloned());
                }
            }
        }
        if !names.is_empty() {
            return Err(GitError::Blocked {
                message: format!(
                    "recovery is blocked because configured external Git drivers are active or may be activated by a recovery target tree and start uncontained processes: {}; disable the applicable attributes or remove the drivers from every Git config scope, preview recovery again, or run Git manually",
                    names.join(", ")
                ),
            });
        }
        Ok(())
    }

    fn active_recovery_filters_until(
        &self,
        configured: &BTreeSet<String>,
        deadline: Instant,
    ) -> Result<BTreeSet<String>, GitError> {
        let mut sources = BTreeSet::from([None]);
        match self.repository_operation_until(deadline)? {
            Some(RepositoryOperation::Merge) => {
                sources.insert(Some("ORIG_HEAD".to_owned()));
            }
            Some(RepositoryOperation::Rebase) => {
                sources.insert(self.recovery_overwrite_target_until(
                    RepositoryOperation::Rebase,
                    RecoveryAction::Abort,
                    deadline,
                )?);
                sources.extend(
                    self.remaining_rebase_commit_oids_until(deadline)?
                        .into_iter()
                        .map(Some),
                );
            }
            None => {}
        }

        let mut active = BTreeSet::new();
        for source in sources {
            let list_args = match source.as_deref() {
                Some(source) => vec![
                    "ls-tree".to_owned(),
                    "-r".to_owned(),
                    "--name-only".to_owned(),
                    "-z".to_owned(),
                    source.to_owned(),
                    "--".to_owned(),
                ],
                None => vec![
                    "ls-files".to_owned(),
                    "--cached".to_owned(),
                    "-z".to_owned(),
                    "--".to_owned(),
                ],
            };
            let paths = self.run_bounded_git(list_args.clone(), deadline, true)?;
            if !paths.status.success() {
                return Err(paths.git_error(list_args));
            }
            if paths.stdout.truncated {
                return Err(GitError::Blocked {
                    message: "recovery is blocked because paths requiring attribute inspection exceed the bounded output limit"
                        .to_owned(),
                });
            }
            let paths = paths
                .stdout
                .bytes
                .split(|byte| *byte == 0)
                .filter(|path| !path.is_empty())
                .collect::<Vec<_>>();
            for chunk in paths.chunks(128) {
                let mut args = vec!["check-attr".to_owned(), "-z".to_owned()];
                if let Some(source) = source.as_deref() {
                    args.push(format!("--source={source}"));
                }
                args.extend(["filter".to_owned(), "--".to_owned()]);
                for path in chunk {
                    let path = std::str::from_utf8(path).map_err(|_| GitError::Blocked {
                        message: "recovery is blocked because an attribute path is not valid UTF-8"
                            .to_owned(),
                    })?;
                    args.push(path.to_owned());
                }
                let attributes = self.run_bounded_git(args.clone(), deadline, true)?;
                if !attributes.status.success() {
                    return Err(attributes.git_error(args));
                }
                if attributes.stdout.truncated {
                    return Err(GitError::Blocked {
                        message: "recovery is blocked because effective attributes exceed the bounded output limit"
                            .to_owned(),
                    });
                }
                let fields = attributes
                    .stdout
                    .bytes
                    .split(|byte| *byte == 0)
                    .filter(|field| !field.is_empty())
                    .collect::<Vec<_>>();
                let mut triples = fields.chunks_exact(3);
                for triple in &mut triples {
                    let driver = String::from_utf8_lossy(triple[2]).to_ascii_lowercase();
                    if configured.contains(&driver) {
                        active.insert(driver);
                    }
                }
                if !triples.remainder().is_empty() {
                    return Err(GitError::Blocked {
                        message:
                            "recovery is blocked because effective attributes could not be parsed"
                                .to_owned(),
                    });
                }
            }
        }
        Ok(active)
    }

    fn bounded_config_is_enabled(&self, key: &str, deadline: Instant) -> Result<bool, GitError> {
        let args = vec!["config".to_owned(), "--get".to_owned(), key.to_owned()];
        let output = self.run_bounded_git(args.clone(), deadline, true)?;
        if output.status.code() == Some(1) {
            return Ok(false);
        }
        if !output.status.success() {
            return Err(output.git_error(args));
        }
        if output.stdout.truncated {
            return Err(GitError::Blocked {
                message: format!("recovery is blocked because {key} is too long"),
            });
        }
        let value = String::from_utf8_lossy(strip_byte_line_ending(&output.stdout.bytes));
        Ok(!matches!(value.trim(), "" | "0" | "false" | "no" | "off"))
    }

    fn untracked_worktree_paths_until(&self, deadline: Instant) -> Result<Vec<PathBuf>, GitError> {
        self.other_worktree_paths_until(false, deadline)
    }

    fn other_worktree_paths_until(
        &self,
        ignored: bool,
        deadline: Instant,
    ) -> Result<Vec<PathBuf>, GitError> {
        let kind = if ignored { "ignored" } else { "untracked" };
        let args = self.other_worktree_paths_args(ignored);
        let output = self.run_bounded_git(args.clone(), deadline, true)?;
        if !output.status.success() {
            return Err(output.git_error(args));
        }
        if output.stdout.truncated {
            return Err(GitError::Blocked {
                message: format!(
                    "recovery planning is blocked because {kind} worktree paths exceed the bounded output limit"
                ),
            });
        }
        let mut paths = BTreeSet::new();
        for path in output.stdout.bytes.split(|byte| *byte == 0) {
            if path.is_empty() {
                continue;
            }
            let path = path_from_bytes(path);
            if path.is_absolute()
                || path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return Err(GitError::Blocked {
                    message: format!(
                        "recovery planning is blocked by an invalid {kind} worktree path"
                    ),
                });
            }
            paths.insert(path);
        }
        Ok(paths.into_iter().collect())
    }

    fn other_worktree_paths_args(&self, ignored: bool) -> Vec<String> {
        let mut args = vec!["ls-files".to_owned(), "--others".to_owned()];
        if ignored {
            args.push("--ignored".to_owned());
        }
        args.extend([
            "--exclude-standard".to_owned(),
            "--full-name".to_owned(),
            "-z".to_owned(),
            "--".to_owned(),
        ]);
        args
    }

    fn ensure_no_ignored_recovery_collisions_until(
        &self,
        operation: RepositoryOperation,
        action: RecoveryAction,
        deadline: Instant,
        hasher: &mut Sha256,
        mut fence: Option<&mut RecoveryInputFence>,
    ) -> Result<(), GitError> {
        let Some(target) = self.recovery_overwrite_target_until(operation, action, deadline)?
        else {
            return Ok(());
        };
        let changed_args = if action == RecoveryAction::Abort {
            vec![
                "ls-tree".to_owned(),
                "-r".to_owned(),
                "--name-only".to_owned(),
                "-z".to_owned(),
                target,
                "--".to_owned(),
            ]
        } else {
            vec![
                "diff".to_owned(),
                "--name-only".to_owned(),
                "--no-renames".to_owned(),
                "--no-ext-diff".to_owned(),
                "--no-textconv".to_owned(),
                "-z".to_owned(),
                "HEAD".to_owned(),
                target,
                "--".to_owned(),
            ]
        };
        let changed = self.run_bounded_git(changed_args.clone(), deadline, true)?;
        if !changed.status.success() {
            return Err(changed.git_error(changed_args));
        }
        if changed.stdout.truncated {
            return Err(GitError::Blocked {
                message: "recovery planning is blocked because recovery target paths exceed the bounded output limit"
                    .to_owned(),
            });
        }
        hash_field(hasher, &changed.stdout.digest);
        if let Some(fence) = fence.as_deref_mut() {
            fence.probes.push(RecoveryProbe {
                args: changed_args,
                digest: changed.stdout.digest,
                status: changed.status.code(),
                safety: RecoveryProbeSafety::None,
            });
        }

        let mut candidates = BTreeSet::new();
        for path in changed.stdout.bytes.split(|byte| *byte == 0) {
            if path.is_empty() {
                continue;
            }
            let path = path_from_bytes(path);
            if path.is_absolute()
                || path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return Err(GitError::Blocked {
                    message: "recovery planning is blocked by an invalid recovery target path"
                        .to_owned(),
                });
            }
            candidates.insert(path);
        }

        if operation == RepositoryOperation::Rebase && action != RecoveryAction::Abort {
            let remaining_oids = match fence.as_deref() {
                Some(fence) => fence.remaining_rebase_commit_oids.clone(),
                None => self.remaining_rebase_commit_oids_until(deadline)?,
            };
            for oid in remaining_oids {
                let args = vec![
                    "show".to_owned(),
                    "--format=".to_owned(),
                    "--name-only".to_owned(),
                    "--no-renames".to_owned(),
                    "--no-ext-diff".to_owned(),
                    "--no-textconv".to_owned(),
                    "-z".to_owned(),
                    oid,
                    "--".to_owned(),
                ];
                let output = self.run_bounded_git(args.clone(), deadline, true)?;
                if !output.status.success() {
                    return Err(output.git_error(args));
                }
                if output.stdout.truncated {
                    return Err(GitError::Blocked {
                        message: "recovery planning is blocked because an intermediate rebase commit touches too many paths"
                            .to_owned(),
                    });
                }
                hash_recovery_probe(hasher, &output);
                if let Some(fence) = fence.as_deref_mut() {
                    fence.probes.push(RecoveryProbe {
                        args,
                        digest: output.stdout.digest,
                        status: output.status.code(),
                        safety: RecoveryProbeSafety::None,
                    });
                }
                for path in output.stdout.bytes.split(|byte| *byte == 0) {
                    if !path.is_empty() {
                        candidates.insert(checked_recovery_target_path(path)?);
                    }
                }
            }
        }

        let mut collisions = BTreeSet::new();
        for chunk in candidates.into_iter().collect::<Vec<_>>().chunks(128) {
            let mut args = vec![
                "ls-files".to_owned(),
                "--others".to_owned(),
                "--ignored".to_owned(),
                "--exclude-standard".to_owned(),
                "--full-name".to_owned(),
                "-z".to_owned(),
                "--".to_owned(),
            ];
            for path in chunk {
                let Some(path) = path.to_str() else {
                    return Err(GitError::Blocked {
                        message: "recovery planning is blocked because a recovery target path is not valid UTF-8"
                            .to_owned(),
                    });
                };
                args.push(format!(":(top,literal){path}"));
            }
            let output = self.run_bounded_git(args.clone(), deadline, true)?;
            if !output.status.success() {
                return Err(output.git_error(args));
            }
            if output.stdout.truncated {
                return Err(GitError::Blocked {
                    message: "recovery planning is blocked because relevant ignored paths exceed the bounded output limit"
                        .to_owned(),
                });
            }
            hash_field(hasher, &output.stdout.digest);
            if let Some(fence) = fence.as_deref_mut() {
                fence.probes.push(RecoveryProbe {
                    args,
                    digest: output.stdout.digest,
                    status: output.status.code(),
                    safety: RecoveryProbeSafety::None,
                });
            }
            for path in output.stdout.bytes.split(|byte| *byte == 0) {
                if !path.is_empty() {
                    collisions.insert(path_from_bytes(path));
                }
            }
        }

        if let Some(path) = collisions.first() {
            return Err(GitError::Blocked {
                message: format!(
                    "{} {} is blocked because ignored worktree path {} would be overwritten; move or remove the path, preview recovery again, or run Git manually",
                    operation.label(),
                    action.label(),
                    path.display()
                ),
            });
        }
        Ok(())
    }

    fn remaining_rebase_commit_oids_until(
        &self,
        deadline: Instant,
    ) -> Result<BTreeSet<String>, GitError> {
        for relative in [
            "rebase-merge/git-rebase-todo",
            "rebase-apply/git-rebase-todo",
        ] {
            let path = self.git_path_until(relative, deadline)?;
            let Some(contents) =
                read_bounded_optional_file(&path, RECOVERY_REBASE_TODO_LIMIT, deadline)?
            else {
                continue;
            };
            return parse_remaining_rebase_commit_oids(relative, &contents);
        }
        Ok(BTreeSet::new())
    }

    fn recovery_overwrite_target_until(
        &self,
        operation: RepositoryOperation,
        action: RecoveryAction,
        deadline: Instant,
    ) -> Result<Option<String>, GitError> {
        match (operation, action) {
            (RepositoryOperation::Merge, RecoveryAction::Continue) => Ok(None),
            (RepositoryOperation::Merge, RecoveryAction::Abort) => Ok(Some("ORIG_HEAD".to_owned())),
            (RepositoryOperation::Merge, RecoveryAction::Skip) => Ok(None),
            (RepositoryOperation::Rebase, _) => {
                for relative in ["rebase-merge/orig-head", "rebase-apply/orig-head"] {
                    let path = self.git_path_until(relative, deadline)?;
                    let Some(value) =
                        read_bounded_optional_file(&path, RECOVERY_REBASE_TODO_LIMIT, deadline)?
                    else {
                        continue;
                    };
                    let oid = value.trim();
                    if matches!(oid.len(), 40 | 64)
                        && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
                    {
                        return Ok(Some(oid.to_owned()));
                    }
                    return Err(GitError::Blocked {
                        message: format!(
                            "recovery planning is blocked because {relative} does not contain a valid object ID"
                        ),
                    });
                }
                Err(GitError::Blocked {
                    message: "recovery planning is blocked because the original rebase target is unavailable"
                        .to_owned(),
                })
            }
        }
    }

    fn ensure_recovery_git_version_until(&self, deadline: Instant) -> Result<(), GitError> {
        let args = vec!["version".to_owned()];
        let output = self.run_bounded_git(args.clone(), deadline, true)?;
        if !output.status.success() {
            return Err(output.git_error(args));
        }
        ensure_recovery_git_version(strip_byte_line_ending(&output.stdout.bytes))
    }

    fn repository_operation_until(
        &self,
        deadline: Instant,
    ) -> Result<Option<RepositoryOperation>, GitError> {
        let rebase_apply = self.git_path_until("rebase-apply", deadline)?;
        if self.git_path_until("rebase-merge", deadline)?.exists()
            || (rebase_apply.exists() && !rebase_apply.join("applying").exists())
        {
            return Ok(Some(RepositoryOperation::Rebase));
        }
        if self.git_path_until("MERGE_HEAD", deadline)?.exists() {
            return Ok(Some(RepositoryOperation::Merge));
        }
        Ok(None)
    }

    fn has_unresolved_conflicts_until(&self, deadline: Instant) -> Result<bool, GitError> {
        let args = vec![
            "diff".to_owned(),
            "--quiet".to_owned(),
            "--diff-filter=U".to_owned(),
            "--".to_owned(),
        ];
        let output = self.run_bounded_git(args.clone(), deadline, true)?;
        match output.status.code() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(output.git_error(args)),
        }
    }

    fn git_path_until(&self, path: &str, deadline: Instant) -> Result<PathBuf, GitError> {
        let args = vec![
            "rev-parse".to_owned(),
            "--path-format=absolute".to_owned(),
            "--git-path".to_owned(),
            path.to_owned(),
        ];
        let output = self.run_bounded_git(args.clone(), deadline, true)?;
        if !output.status.success() {
            return Err(output.git_error(args));
        }
        if output.stdout.truncated {
            return Err(GitError::Blocked {
                message: format!(
                    "recovery planning is blocked because Git path {path} is too long"
                ),
            });
        }
        Ok(path_from_bytes(strip_byte_line_ending(
            &output.stdout.bytes,
        )))
    }

    fn recovery_hooks_path_until(&self, deadline: Instant) -> Result<PathBuf, GitError> {
        let args = vec![
            "config".to_owned(),
            "--path".to_owned(),
            "--get".to_owned(),
            "core.hooksPath".to_owned(),
        ];
        let output = self.run_bounded_git(args.clone(), deadline, true)?;
        if output.status.success() {
            if output.stdout.truncated {
                return Err(GitError::Blocked {
                    message: "recovery planning is blocked because core.hooksPath is too long"
                        .to_owned(),
                });
            }
            let path = path_from_bytes(strip_byte_line_ending(&output.stdout.bytes));
            return Ok(if path.is_absolute() {
                path
            } else {
                let root = self.repo_root_until(deadline)?;
                root.join(path)
            });
        }
        if output.status.code() == Some(1) {
            return self.git_path_until("hooks", deadline);
        }
        Err(output.git_error(args))
    }

    fn recovery_hooks_path_fenced_until(
        &self,
        deadline: Instant,
        hasher: &mut Sha256,
        fence: Option<&mut RecoveryInputFence>,
    ) -> Result<PathBuf, GitError> {
        let args = vec![
            "config".to_owned(),
            "--path".to_owned(),
            "--get".to_owned(),
            "core.hooksPath".to_owned(),
        ];
        let output = self.run_bounded_git(args.clone(), deadline, true)?;
        if output.stdout.truncated {
            return Err(GitError::Blocked {
                message: "recovery planning is blocked because core.hooksPath is too long"
                    .to_owned(),
            });
        }
        if !output.status.success() && output.status.code() != Some(1) {
            return Err(output.git_error(args));
        }
        hash_recovery_probe(hasher, &output);
        if let Some(fence) = fence {
            fence.probes.push(RecoveryProbe {
                args,
                digest: output.stdout.digest,
                status: output.status.code(),
                safety: RecoveryProbeSafety::None,
            });
        }
        if output.status.success() {
            let path = path_from_bytes(strip_byte_line_ending(&output.stdout.bytes));
            return Ok(if path.is_absolute() {
                path
            } else {
                self.repo_root_until(deadline)?.join(path)
            });
        }
        self.git_path_until("hooks", deadline)
    }

    fn git_var_path_until(
        &self,
        variable: &str,
        deadline: Instant,
    ) -> Result<Option<PathBuf>, GitError> {
        let args = vec!["var".to_owned(), variable.to_owned()];
        let output = self.run_bounded_git(args.clone(), deadline, true)?;
        if output.status.code() == Some(1) {
            return Ok(None);
        }
        if !output.status.success() {
            return Err(output.git_error(args));
        }
        if output.stdout.truncated {
            return Err(GitError::Blocked {
                message: format!("recovery planning is blocked because {variable} is too long"),
            });
        }
        let path = path_from_bytes(strip_byte_line_ending(&output.stdout.bytes));
        if !path.is_absolute() {
            return Err(GitError::Blocked {
                message: format!(
                    "recovery planning is blocked because {variable} is not an absolute path"
                ),
            });
        }
        Ok(Some(path))
    }

    fn worktree_attributes_until(&self, deadline: Instant) -> Result<Vec<PathBuf>, GitError> {
        let mut paths = BTreeSet::new();
        for selectors in [
            &["--cached", "--others", "--exclude-standard"][..],
            &["--others", "--ignored", "--exclude-standard"][..],
        ] {
            let mut args = vec!["ls-files".to_owned()];
            args.extend(selectors.iter().map(|selector| (*selector).to_owned()));
            args.extend([
                "-z".to_owned(),
                "--".to_owned(),
                ":(glob)**/.gitattributes".to_owned(),
            ]);
            let output = self.run_bounded_git(args.clone(), deadline, true)?;
            if !output.status.success() {
                return Err(output.git_error(args));
            }
            if output.stdout.truncated {
                return Err(GitError::Blocked {
                    message: "recovery planning is blocked because attribute paths exceed the bounded output limit"
                        .to_owned(),
                });
            }
            for path in output.stdout.bytes.split(|byte| *byte == 0) {
                if path.is_empty() {
                    continue;
                }
                let path = path_from_bytes(path);
                if path.is_absolute()
                    || path
                        .components()
                        .any(|component| matches!(component, std::path::Component::ParentDir))
                {
                    return Err(GitError::Blocked {
                        message: "recovery planning is blocked by an invalid attribute path"
                            .to_owned(),
                    });
                }
                paths.insert(path);
            }
        }
        Ok(paths.into_iter().collect())
    }

    fn repo_root_until(&self, deadline: Instant) -> Result<PathBuf, GitError> {
        let args = vec!["rev-parse".to_owned(), "--show-toplevel".to_owned()];
        let output = self.run_bounded_git(args.clone(), deadline, true)?;
        if !output.status.success() {
            return Err(output.git_error(args));
        }
        if output.stdout.truncated {
            return Err(GitError::Blocked {
                message: "recovery planning is blocked because the repository path is too long"
                    .to_owned(),
            });
        }
        Ok(path_from_bytes(strip_byte_line_ending(
            &output.stdout.bytes,
        )))
    }

    fn acquire_recovery_lock_until(
        &self,
        operation: RepositoryOperation,
        action: RecoveryAction,
        deadline: Instant,
    ) -> Result<RecoveryLock, GitError> {
        let repository_root = self.canonical_repository_root_until(deadline)?;
        let path = self.recovery_lock_path(operation, action, &repository_root)?;
        let directory_path = path
            .parent()
            .ok_or_else(|| GitError::Blocked {
                message: "recovery is blocked because the BitByGit recovery lock directory is unavailable"
                    .to_owned(),
            })?
            .to_owned();
        let directory = open_recovery_lock_directory(&directory_path, operation, action)?;
        let guard_path = path.with_extension("guard");
        let guard = open_recovery_lock_file(&guard_path, operation, action)?;
        guard
            .try_lock_exclusive()
            .map_err(|source| recovery_lock_error(operation, action, source))?;
        let file = open_recovery_lock_file(&path, operation, action)?;
        file.try_lock_exclusive()
            .map_err(|source| recovery_lock_error(operation, action, source))?;
        let lock = RecoveryLock {
            path,
            file,
            guard_path,
            guard,
            directory_path,
            directory,
            repository_root,
        };
        lock.ensure_identity(&lock.repository_root, operation, action)?;
        Ok(lock)
    }

    fn canonical_repository_root_until(&self, deadline: Instant) -> Result<PathBuf, GitError> {
        let root = self.repo_root_until(deadline)?;
        fs::canonicalize(&root).map_err(|source| GitError::Io {
            args: vec!["canonicalize recovery repository identity".to_owned()],
            source,
        })
    }

    fn recovery_lock_path(
        &self,
        operation: RepositoryOperation,
        action: RecoveryAction,
        repository_root: &Path,
    ) -> Result<PathBuf, GitError> {
        #[cfg(test)]
        let data_dir = self
            .recovery_data_dir
            .clone()
            .or_else(|| Some(self.cwd.join(".git").join("bitbygit-test-data")));
        #[cfg(not(test))]
        let data_dir = resolve_recovery_data_dir(|name| env::var_os(name).map(PathBuf::from));
        let data_dir = data_dir.ok_or_else(|| GitError::Blocked {
            message:
                "recovery is blocked because BitByGit's application data directory is unavailable"
                    .to_owned(),
        })?;
        let lock_dir = data_dir.join(RECOVERY_LOCK_DIRECTORY);
        fs::create_dir_all(&lock_dir)
            .map_err(|source| recovery_lock_io(operation, action, source))?;
        let lock_dir = fs::canonicalize(&lock_dir)
            .map_err(|source| recovery_lock_io(operation, action, source))?;
        if !lock_dir.is_dir() {
            return Err(GitError::Blocked {
                message:
                    "recovery is blocked because BitByGit's recovery lock path is not a directory"
                        .to_owned(),
            });
        }
        let key = recovery_repository_key(repository_root);
        debug_assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()));
        Ok(lock_dir.join(format!("{key}.lock")))
    }

    fn run_args(&self, args: Vec<String>) -> Result<GitOutput, GitError> {
        self.run_args_with_editor(args, false)
    }

    fn run_recovery_args_until_with<F>(
        &self,
        operation: RepositoryOperation,
        action: RecoveryAction,
        deadline: Instant,
        before_spawn: F,
    ) -> Result<GitOutput, GitError>
    where
        F: FnOnce() -> Result<(), GitError>,
    {
        let args = vec![
            operation.label().to_owned(),
            format!("--{}", action.label()),
        ];
        let output = self.run_bounded_git_with(args.clone(), deadline, false, before_spawn)?;
        let stdout = output.stdout.lossy_text();
        let stderr = output.stderr.lossy_text();
        if !output.status.success() {
            return Err(GitError::GitFailed {
                args,
                status: output.status,
                stdout,
                stderr,
            });
        }
        Ok(GitOutput {
            status: output.status,
            stdout,
            stderr,
        })
    }

    fn run_bounded_git(
        &self,
        args: Vec<String>,
        deadline: Instant,
        optional_locks: bool,
    ) -> Result<BoundedCommandOutput, GitError> {
        self.run_bounded_git_with(args, deadline, optional_locks, || Ok(()))
    }

    fn run_bounded_git_digest(
        &self,
        args: Vec<String>,
        deadline: Instant,
    ) -> Result<BoundedCommandOutput, GitError> {
        self.run_bounded_git_with_policy(args, deadline, true, BoundedCommandPolicy::Digest, || {
            Ok(())
        })
    }

    fn run_bounded_git_with<F>(
        &self,
        args: Vec<String>,
        deadline: Instant,
        optional_locks: bool,
        before_spawn: F,
    ) -> Result<BoundedCommandOutput, GitError>
    where
        F: FnOnce() -> Result<(), GitError>,
    {
        let policy = if optional_locks {
            BoundedCommandPolicy::Diagnostic
        } else {
            BoundedCommandPolicy::RecoveryExecution
        };
        self.run_bounded_git_with_policy(args, deadline, optional_locks, policy, before_spawn)
    }

    fn run_bounded_git_with_policy<F>(
        &self,
        args: Vec<String>,
        deadline: Instant,
        optional_locks: bool,
        policy: BoundedCommandPolicy,
        before_spawn: F,
    ) -> Result<BoundedCommandOutput, GitError>
    where
        F: FnOnce() -> Result<(), GitError>,
    {
        let mut command = Command::new("git");
        command
            .current_dir(&self.cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("SSH_ASKPASS", "")
            .env("SSH_ASKPASS_REQUIRE", "never")
            .env("GIT_EDITOR", "true")
            .env("GIT_SEQUENCE_EDITOR", "true");
        if self.isolated_test_config {
            let global_config = self.cwd.join(".bitbygit-test-global-config");
            #[cfg(test)]
            let global_config = self.test_global_config.clone().unwrap_or(global_config);
            command
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", global_config);
        }
        #[cfg(all(test, unix))]
        if let Some(path) = &self.test_git_exec_path {
            command.env("GIT_EXEC_PATH", path);
        }
        if optional_locks {
            command.env("GIT_OPTIONAL_LOCKS", "0");
            if args.first().is_none_or(|arg| arg != "config") {
                command.args(["-c", "core.fsmonitor=false"]);
            }
        } else {
            command.args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"]);
        }
        command.args(&args);
        if self.ssh_executable.is_some() || env::var_os("GIT_SSH_COMMAND").is_none() {
            let ssh_executable = self
                .ssh_executable
                .as_ref()
                .map(|path| shell_quote(&path.to_string_lossy()))
                .unwrap_or_else(|| "ssh".to_owned());
            command.env("GIT_SSH_COMMAND", format!("{ssh_executable} {SSH_OPTIONS}"));
        }
        configure_process_group(&mut command);
        let output_limit = if optional_locks {
            RECOVERY_DIAGNOSTIC_LIMIT
        } else {
            self.recovery_output_limit
        };
        run_bounded_command(command, args, deadline, output_limit, policy, before_spawn)
    }

    fn run_args_with_editor(
        &self,
        args: Vec<String>,
        disable_editor: bool,
    ) -> Result<GitOutput, GitError> {
        let mut command = Command::new("git");
        command
            .current_dir(&self.cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("SSH_ASKPASS", "")
            .env("SSH_ASKPASS_REQUIRE", "never");
        if disable_editor {
            command
                .env("GIT_EDITOR", "true")
                .env("GIT_SEQUENCE_EDITOR", "true");
        }
        if self.ssh_executable.is_some() || env::var_os("GIT_SSH_COMMAND").is_none() {
            let ssh_executable = self
                .ssh_executable
                .as_ref()
                .map(|path| shell_quote(&path.to_string_lossy()))
                .unwrap_or_else(|| "ssh".to_owned());
            command.env("GIT_SSH_COMMAND", format!("{ssh_executable} {SSH_OPTIONS}"));
        }
        let output = command
            .args(&args)
            .output()
            .map_err(|source| GitError::Io {
                args: args.clone(),
                source,
            })?;

        let stdout = if disable_editor {
            String::from_utf8_lossy(&output.stdout).into_owned()
        } else {
            String::from_utf8(output.stdout).map_err(|source| GitError::Utf8 {
                args: args.clone(),
                stream: OutputStream::Stdout,
                source,
            })?
        };
        let stderr = if disable_editor {
            String::from_utf8_lossy(&output.stderr).into_owned()
        } else {
            String::from_utf8(output.stderr).map_err(|source| GitError::Utf8 {
                args: args.clone(),
                stream: OutputStream::Stderr,
                source,
            })?
        };

        if !output.status.success() {
            return Err(GitError::GitFailed {
                args,
                status: output.status,
                stdout,
                stderr,
            });
        }

        Ok(GitOutput {
            status: output.status,
            stdout,
            stderr,
        })
    }

    fn run_path_args<const N: usize>(
        &self,
        args: [&str; N],
        path: Option<&Path>,
        literal_pathspecs: bool,
    ) -> Result<GitOutput, GitError> {
        let mut os_args = args.iter().map(OsString::from).collect::<Vec<_>>();
        if let Some(path) = path {
            os_args.push(OsString::from("--"));
            os_args.push(path.as_os_str().to_owned());
        }
        let output = self.run_os_args(os_args, None, literal_pathspecs)?;
        let stdout = String::from_utf8(output.stdout).map_err(|source| GitError::Utf8 {
            args: output.args.clone(),
            stream: OutputStream::Stdout,
            source,
        })?;
        let stderr = String::from_utf8(output.stderr).map_err(|source| GitError::Utf8 {
            args: output.args.clone(),
            stream: OutputStream::Stderr,
            source,
        })?;
        Ok(GitOutput {
            status: output.status,
            stdout,
            stderr,
        })
    }

    fn run_paths_args<const N: usize>(
        &self,
        args: [&str; N],
        paths: &[PathBuf],
        literal_pathspecs: bool,
    ) -> Result<GitOutput, GitError> {
        let mut os_args = args.iter().map(OsString::from).collect::<Vec<_>>();
        os_args.push(OsString::from("--"));
        os_args.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
        let output = self.run_os_args(os_args, None, literal_pathspecs)?;
        let stdout = String::from_utf8(output.stdout).map_err(|source| GitError::Utf8 {
            args: output.args.clone(),
            stream: OutputStream::Stdout,
            source,
        })?;
        let stderr = String::from_utf8(output.stderr).map_err(|source| GitError::Utf8 {
            args: output.args.clone(),
            stream: OutputStream::Stderr,
            source,
        })?;
        Ok(GitOutput {
            status: output.status,
            stdout,
            stderr,
        })
    }

    fn run_os_paths(
        &self,
        mut args: Vec<OsString>,
        paths: &[PathBuf],
        literal_pathspecs: bool,
    ) -> Result<RawProcessOutput, GitError> {
        args.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
        self.run_os_args(args, None, literal_pathspecs)
    }

    fn run_os_args(
        &self,
        mut args: Vec<OsString>,
        path: Option<&Path>,
        literal_pathspecs: bool,
    ) -> Result<RawProcessOutput, GitError> {
        if let Some(path) = path {
            args.push(path.as_os_str().to_owned());
        }
        let display_args = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let mut command = Command::new("git");
        command.current_dir(&self.cwd).args(&args);
        command.env("GIT_TERMINAL_PROMPT", "0");
        command.env("GIT_ASKPASS", "");
        command.env("SSH_ASKPASS", "");
        command.env("SSH_ASKPASS_REQUIRE", "never");
        if literal_pathspecs {
            command.env("GIT_LITERAL_PATHSPECS", "1");
        }
        let output = command.output().map_err(|source| GitError::Io {
            args: display_args.clone(),
            source,
        })?;

        if !output.status.success() {
            return Err(GitError::GitFailed {
                args: display_args,
                status: output.status,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        Ok(RawProcessOutput {
            args: display_args,
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn run_raw<const N: usize>(&self, args: [&str; N]) -> Result<RawGitOutput, GitError> {
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let output = Command::new("git")
            .current_dir(&self.cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("SSH_ASKPASS", "")
            .env("SSH_ASKPASS_REQUIRE", "never")
            .args(&args)
            .output()
            .map_err(|source| GitError::Io {
                args: args.clone(),
                source,
            })?;

        if !output.status.success() {
            return Err(GitError::GitFailed {
                args,
                status: output.status,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        Ok(RawGitOutput {
            stdout: output.stdout,
        })
    }
}

fn is_recovery_external_driver_config(name: &[u8]) -> bool {
    let name = String::from_utf8_lossy(name).to_ascii_lowercase();
    name == "diff.external"
        || (name.starts_with("filter.")
            && [".clean", ".smudge", ".process"]
                .iter()
                .any(|suffix| name.ends_with(suffix)))
        || (name.starts_with("diff.")
            && [".command", ".textconv"]
                .iter()
                .any(|suffix| name.ends_with(suffix)))
        || (name.starts_with("merge.") && name.ends_with(".driver"))
}

fn recovery_filter_driver(name: &[u8]) -> Option<String> {
    let name = String::from_utf8_lossy(name).to_ascii_lowercase();
    let name = name.strip_prefix("filter.")?;
    let driver = [".clean", ".smudge", ".process"]
        .into_iter()
        .find_map(|suffix| name.strip_suffix(suffix))?;
    (!driver.is_empty()).then(|| driver.to_owned())
}

fn hash_recovery_probe(hasher: &mut Sha256, output: &BoundedCommandOutput) {
    hash_field(
        hasher,
        &output.status.code().unwrap_or(i32::MIN).to_le_bytes(),
    );
    hash_field(hasher, &output.stdout.digest);
}

fn ensure_recovery_probe_observation_safe(
    safety: RecoveryProbeSafety,
    args: &[String],
    output: &BoundedCommandOutput,
) -> Result<(), GitError> {
    match safety {
        RecoveryProbeSafety::None => {
            if !output.status.success() && output.status.code() != Some(1) {
                return Err(output.clone().git_error(args.to_vec()));
            }
        }
        RecoveryProbeSafety::DisabledConfig(key) => {
            if output.stdout.truncated {
                return Err(GitError::Blocked {
                    message: format!("recovery is blocked because {key} is too long"),
                });
            }
            if output.status.code() == Some(1) {
                return Ok(());
            }
            if !output.status.success() {
                return Err(output.clone().git_error(args.to_vec()));
            }
            let value = String::from_utf8_lossy(strip_byte_line_ending(&output.stdout.bytes));
            if !matches!(value.trim(), "" | "0" | "false" | "no" | "off") {
                let risk = if key == "core.fsmonitor" {
                    "an uncontained process"
                } else {
                    "an uncontained signing process"
                };
                return Err(GitError::Blocked {
                    message: format!(
                        "recovery is blocked because {key} may start {risk}; disable it and preview recovery again, or run Git manually"
                    ),
                });
            }
        }
        RecoveryProbeSafety::ExternalDrivers => {
            if output.stdout.truncated {
                return Err(GitError::Blocked {
                    message: "recovery is blocked because Git configuration exceeds the bounded diagnostic limit, so external drivers cannot be ruled out"
                        .to_owned(),
                });
            }
            if !output.status.success() {
                return Err(output.clone().git_error(args.to_vec()));
            }
            let names = output
                .stdout
                .bytes
                .split(|byte| *byte == 0)
                .filter(|name| {
                    is_recovery_external_driver_config(name)
                        && recovery_filter_driver(name).is_none()
                })
                .map(|name| String::from_utf8_lossy(name).into_owned())
                .collect::<Vec<_>>();
            if !names.is_empty() {
                return Err(GitError::Blocked {
                    message: format!(
                        "recovery is blocked because configured external Git drivers may be activated by a recovery target tree and start uncontained processes: {}; remove them from every Git config scope, preview recovery again, or run Git manually",
                        names.join(", ")
                    ),
                });
            }
        }
    }
    Ok(())
}

fn checked_recovery_target_path(path: &[u8]) -> Result<PathBuf, GitError> {
    let path = path_from_bytes(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(GitError::Blocked {
            message: "recovery planning is blocked by an invalid recovery target path".to_owned(),
        });
    }
    Ok(path)
}

fn parse_remaining_rebase_commit_oids(
    relative: &str,
    contents: &str,
) -> Result<BTreeSet<String>, GitError> {
    let mut oids = BTreeSet::new();
    for line in contents.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(command) = fields.next() else {
            continue;
        };
        let oid = match command {
            "pick" | "p" | "reword" | "r" | "edit" | "e" | "squash" | "s" => fields.next(),
            "fixup" | "f" => fields.find(|field| !matches!(*field, "-C" | "-c")),
            "merge" | "m" => {
                let fields = fields.collect::<Vec<_>>();
                fields
                    .windows(2)
                    .find_map(|pair| matches!(pair[0], "-C" | "-c").then_some(pair[1]))
            }
            _ => None,
        };
        if let Some(oid) = oid {
            if oid.len() < 4 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(GitError::Blocked {
                    message: format!(
                        "recovery planning is blocked because {relative} contains an invalid commit object ID"
                    ),
                });
            }
            oids.insert(oid.to_owned());
        }
    }
    Ok(oids)
}

fn run_bounded_command<F>(
    mut command: Command,
    args: Vec<String>,
    deadline: Instant,
    output_limit: usize,
    policy: BoundedCommandPolicy,
    before_spawn: F,
) -> Result<BoundedCommandOutput, GitError>
where
    F: FnOnce() -> Result<(), GitError>,
{
    if Instant::now() >= deadline {
        return Err(GitError::TimedOut { args });
    }
    let capture_limit = if policy == BoundedCommandPolicy::Diagnostic {
        RECOVERY_DIAGNOSTIC_CAPTURE_LIMIT
    } else {
        usize::MAX
    };
    let digest_all_output = policy != BoundedCommandPolicy::RecoveryExecution;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if Instant::now() >= deadline {
        return Err(GitError::TimedOut { args });
    }
    before_spawn()?;
    if Instant::now() >= deadline {
        return Err(GitError::TimedOut { args });
    }
    let mut child = spawn_contained(command).map_err(|source| GitError::Io {
        args: args.clone(),
        source,
    })?;
    let (stdout, stderr) = take_contained_output(&mut child).map_err(|source| GitError::Io {
        args: args.clone(),
        source,
    })?;
    let capture_exceeded = Arc::new(AtomicBool::new(false));
    let stdout_exceeded = Arc::clone(&capture_exceeded);
    let stderr_exceeded = Arc::clone(&capture_exceeded);
    let stdout_reader = thread::spawn(move || {
        drain_bounded_output(
            stdout,
            output_limit,
            capture_limit,
            digest_all_output,
            stdout_exceeded,
        )
    });
    let stderr_reader = thread::spawn(move || {
        drain_bounded_output(
            stderr,
            output_limit,
            capture_limit,
            digest_all_output,
            stderr_exceeded,
        )
    });

    let mut output_limit_hit = false;
    let status = if policy == BoundedCommandPolicy::RecoveryExecution {
        // The deadline protects the checks before spawn. Killing Git after a
        // mutating recovery starts can leave a partial worktree or stale lock.
        match child.wait() {
            Ok(status) => status,
            Err(source) => {
                kill_process_tree(&mut child);
                let _result = child.wait();
                kill_process_tree(&mut child);
                let _result = join_capture_reader(stdout_reader, &args);
                let _result = join_capture_reader(stderr_reader, &args);
                return Err(GitError::Io { args, source });
            }
        }
    } else {
        loop {
            if policy == BoundedCommandPolicy::Diagnostic
                && capture_exceeded.load(Ordering::Acquire)
            {
                output_limit_hit = true;
                kill_process_tree(&mut child);
                break child.wait().map_err(|source| GitError::Io {
                    args: args.clone(),
                    source,
                })?;
            }
            if let Some(status) = child.try_wait().map_err(|source| GitError::Io {
                args: args.clone(),
                source,
            })? {
                break status;
            }
            let now = Instant::now();
            if now >= deadline {
                kill_process_tree(&mut child);
                let _result = child.wait();
                kill_process_tree(&mut child);
                join_capture_reader(stdout_reader, &args)?;
                join_capture_reader(stderr_reader, &args)?;
                return Err(GitError::TimedOut { args });
            }
            let delay = Duration::from_millis(5).min(deadline.saturating_duration_since(now));
            thread::sleep(delay);
        }
    };
    // End the contained process tree before joining the drains so descendants
    // cannot keep inherited pipe writers open.
    kill_process_tree(&mut child);
    let stdout = join_capture_reader(stdout_reader, &args)?;
    let stderr = join_capture_reader(stderr_reader, &args)?;
    if policy == BoundedCommandPolicy::Diagnostic
        && (output_limit_hit || capture_exceeded.load(Ordering::Acquire))
    {
        return Err(GitError::Blocked {
            message: format!(
                "git {} output exceeded the bounded capture limit",
                args.join(" ")
            ),
        });
    }

    Ok(BoundedCommandOutput {
        status,
        stdout,
        stderr,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BoundedCommandPolicy {
    Diagnostic,
    Digest,
    RecoveryExecution,
}

fn drain_bounded_output(
    mut reader: impl Read,
    output_limit: usize,
    capture_limit: usize,
    digest_all_output: bool,
    exceeded: Arc<AtomicBool>,
) -> io::Result<CapturedStream> {
    let mut hasher = Sha256::new();
    let mut bytes = Vec::with_capacity(output_limit.min(capture_limit).min(8192));
    let mut total = 0usize;
    let mut buffer = [0; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let within_limit = capture_limit.saturating_sub(total).min(count);
        if digest_all_output {
            hasher.update(&buffer[..within_limit]);
        }
        let retained = output_limit.saturating_sub(bytes.len()).min(within_limit);
        bytes.extend_from_slice(&buffer[..retained]);
        total = total.saturating_add(count);
        if total > capture_limit {
            exceeded.store(true, Ordering::Release);
        }
    }
    if !digest_all_output {
        hasher.update(&bytes);
    }
    Ok(CapturedStream {
        bytes,
        digest: hasher.finalize().into(),
        truncated: total > output_limit,
    })
}

fn join_capture_reader(
    reader: thread::JoinHandle<io::Result<CapturedStream>>,
    args: &[String],
) -> Result<CapturedStream, GitError> {
    reader
        .join()
        .map_err(|_| recovery_output_io(args, io::Error::other("output drainer panicked")))?
        .map_err(|source| recovery_output_io(args, source))
}

fn recovery_output_io(args: &[String], source: io::Error) -> GitError {
    GitError::Io {
        args: args.to_vec(),
        source,
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn spawn_contained(mut command: Command) -> io::Result<Child> {
    command.spawn()
}

#[cfg(unix)]
fn take_contained_output(child: &mut Child) -> io::Result<(ChildStdout, ChildStderr)> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("contained command has no stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("contained command has no stderr pipe"))?;
    Ok((stdout, stderr))
}

#[cfg(windows)]
fn spawn_contained(mut command: Command) -> io::Result<GroupChild> {
    command.group().kill_on_drop(true).spawn()
}

#[cfg(windows)]
fn take_contained_output(child: &mut GroupChild) -> io::Result<(ChildStdout, ChildStderr)> {
    let child = child.inner();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("contained command has no stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("contained command has no stderr pipe"))?;
    Ok((stdout, stderr))
}

#[cfg(not(any(unix, windows)))]
fn spawn_contained(mut command: Command) -> io::Result<std::process::Child> {
    command.spawn()
}

#[cfg(not(any(unix, windows)))]
fn take_contained_output(
    child: &mut std::process::Child,
) -> io::Result<(ChildStdout, ChildStderr)> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("contained command has no stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("contained command has no stderr pipe"))?;
    Ok((stdout, stderr))
}

#[cfg(unix)]
fn kill_process_tree(child: &mut Child) {
    if let Some(pid) = Pid::from_raw(child.id() as i32) {
        let _result = kill_process_group(pid, Signal::Kill);
    }
    let _result = child.kill();
}

#[cfg(windows)]
fn kill_process_tree(child: &mut GroupChild) {
    let _result = child.kill();
}

#[cfg(not(any(unix, windows)))]
fn kill_process_tree(child: &mut std::process::Child) {
    let _result = child.kill();
}

#[cfg(any(unix, windows))]
fn ensure_recovery_execution_supported(
    _operation: RepositoryOperation,
    _action: RecoveryAction,
) -> Result<(), GitError> {
    Ok(())
}

#[cfg(any(unix, windows))]
fn ensure_recovery_planning_supported() -> Result<(), GitError> {
    Ok(())
}

fn ensure_recovery_git_version(version: &[u8]) -> Result<(), GitError> {
    let version = String::from_utf8_lossy(version);
    let parsed = version.strip_prefix("git version ").and_then(|version| {
        let mut components = version.split('.');
        Some((
            components.next()?.parse().ok()?,
            components.next()?.parse().ok()?,
        ))
    });
    if parsed.is_some_and(|version| version >= RECOVERY_MINIMUM_GIT_VERSION) {
        return Ok(());
    }
    Err(GitError::Blocked {
        message: format!(
            "recovery requires Git {}.{} or newer because exact attribute-state fingerprinting is unavailable in older Git versions; upgrade Git or run recovery manually",
            RECOVERY_MINIMUM_GIT_VERSION.0, RECOVERY_MINIMUM_GIT_VERSION.1
        ),
    })
}

#[cfg(not(any(unix, windows)))]
fn ensure_recovery_planning_supported() -> Result<(), GitError> {
    Err(GitError::Blocked {
        message: format!(
            "recovery planning and execution are blocked on {} because BitByGit cannot provide hard output and descendant-process bounds; inspect the repository and run the corresponding Git recovery command manually, or use BitByGit on Linux",
            env::consts::OS
        ),
    })
}

#[cfg(not(any(unix, windows)))]
fn ensure_recovery_execution_supported(
    operation: RepositoryOperation,
    action: RecoveryAction,
) -> Result<(), GitError> {
    Err(GitError::Blocked {
        message: format!(
            "{} {} is blocked on {} because BitByGit cannot provide hard output and descendant-process bounds on this platform; inspect the repository state and run `git {} --{}` manually, or use BitByGit on Linux",
            operation.label(),
            action.label(),
            env::consts::OS,
            operation.label(),
            action.label()
        ),
    })
}

struct RecoveryLock {
    path: PathBuf,
    file: fs::File,
    guard_path: PathBuf,
    guard: fs::File,
    directory_path: PathBuf,
    directory: RecoveryFile,
    repository_root: PathBuf,
}

impl RecoveryLock {
    fn ensure_identity(
        &self,
        current_repository_root: &Path,
        operation: RepositoryOperation,
        action: RecoveryAction,
    ) -> Result<(), GitError> {
        let opened = self
            .file
            .metadata()
            .map_err(|source| recovery_lock_io(operation, action, source))?;
        let current = fs::symlink_metadata(&self.path)
            .map_err(|source| recovery_lock_io(operation, action, source))?;
        let opened_guard = self
            .guard
            .metadata()
            .map_err(|source| recovery_lock_io(operation, action, source))?;
        let current_guard = fs::symlink_metadata(&self.guard_path)
            .map_err(|source| recovery_lock_io(operation, action, source))?;
        let current_directory =
            open_recovery_lock_directory(&self.directory_path, operation, action)?;
        let same_lock = same_recovery_lock_identity(&self.file, &self.path, &opened, &current)
            .map_err(|source| recovery_lock_io(operation, action, source))?;
        let same_guard = same_recovery_lock_identity(
            &self.guard,
            &self.guard_path,
            &opened_guard,
            &current_guard,
        )
        .map_err(|source| recovery_lock_io(operation, action, source))?;
        let same_directory = self
            .directory
            .same_path_identity(&current_directory)
            .map_err(|source| recovery_lock_io(operation, action, source))?;
        if current_repository_root != self.repository_root
            || !self.directory.metadata.is_dir()
            || !current_directory.metadata.is_dir()
            || !same_directory
            || !opened.is_file()
            || !current.is_file()
            || current.file_type().is_symlink()
            || !same_lock
            || !opened_guard.is_file()
            || !current_guard.is_file()
            || current_guard.file_type().is_symlink()
            || !same_guard
            || recovery_repository_key(current_repository_root)
                != recovery_lock_key_from_path(&self.path).unwrap_or_default()
        {
            return Err(GitError::Blocked {
                message: format!(
                    "{} {} is blocked because the BitByGit recovery lock identity changed",
                    operation.label(),
                    action.label()
                ),
            });
        }
        #[cfg(unix)]
        if opened.nlink() != 1 || opened_guard.nlink() != 1 {
            return Err(GitError::Blocked {
                message: format!(
                    "{} {} is blocked because the BitByGit recovery lock identity changed",
                    operation.label(),
                    action.label()
                ),
            });
        }
        Ok(())
    }
}

fn open_recovery_lock_directory(
    path: &Path,
    operation: RepositoryOperation,
    action: RecoveryAction,
) -> Result<RecoveryFile, GitError> {
    match open_recovery_file(path) {
        Ok(opened) if opened.metadata.is_dir() => Ok(opened),
        Ok(_) | Err(RecoveryOpenError::Missing | RecoveryOpenError::Symlink) => {
            Err(GitError::Blocked {
                message: format!(
                    "{} {} is blocked because the BitByGit recovery lock identity changed",
                    operation.label(),
                    action.label()
                ),
            })
        }
        Err(RecoveryOpenError::Io(source)) => Err(recovery_lock_io(operation, action, source)),
    }
}

#[cfg(windows)]
fn same_recovery_lock_identity(
    opened: &fs::File,
    path: &Path,
    _opened_metadata: &fs::Metadata,
    _path_metadata: &fs::Metadata,
) -> io::Result<bool> {
    let opened = same_file::Handle::from_file(opened.try_clone()?)?;
    let current = same_file::Handle::from_path(path)?;
    Ok(opened == current)
}

#[cfg(not(windows))]
fn same_recovery_lock_identity(
    _opened: &fs::File,
    _path: &Path,
    opened_metadata: &fs::Metadata,
    path_metadata: &fs::Metadata,
) -> io::Result<bool> {
    Ok(same_recovery_file(opened_metadata, path_metadata))
}

#[cfg(unix)]
fn open_recovery_lock_file(
    path: &Path,
    operation: RepositoryOperation,
    action: RecoveryAction,
) -> Result<fs::File, GitError> {
    open(
        path,
        OFlags::CREATE | OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_bits_truncate(0o600),
    )
    .map(fs::File::from)
    .map_err(|source| recovery_lock_io(operation, action, source.into()))
}

#[cfg(not(unix))]
fn open_recovery_lock_file(
    path: &Path,
    operation: RepositoryOperation,
    action: RecoveryAction,
) -> Result<fs::File, GitError> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(GitError::Blocked {
            message: format!(
                "{} {} is blocked because a BitByGit recovery lock path is a symlink",
                operation.label(),
                action.label()
            ),
        });
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(windows)]
    options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    options
        .open(path)
        .map_err(|source| recovery_lock_io(operation, action, source))
}

fn recovery_lock_error(
    operation: RepositoryOperation,
    action: RecoveryAction,
    source: io::Error,
) -> GitError {
    if source.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
        GitError::Blocked {
            message: format!(
                "{} {} is blocked because another BitByGit recovery is active for this repository",
                operation.label(),
                action.label()
            ),
        }
    } else {
        recovery_lock_io(operation, action, source)
    }
}

fn recovery_lock_io(
    operation: RepositoryOperation,
    action: RecoveryAction,
    source: io::Error,
) -> GitError {
    GitError::Io {
        args: vec![format!(
            "{} {} BitByGit recovery lock",
            operation.label(),
            action.label()
        )],
        source,
    }
}

fn resolve_recovery_data_dir(mut env: impl FnMut(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    env("BITBYGIT_DATA_DIR")
        .or_else(|| env("XDG_DATA_HOME").map(|path| path.join("bitbygit")))
        .or_else(|| env("APPDATA").map(|path| path.join("bitbygit").join("data")))
        .or_else(|| env("HOME").map(|path| path.join(".local").join("share").join("bitbygit")))
}

fn recovery_repository_key(repository_root: &Path) -> String {
    let digest = Sha256::digest(repository_root.as_os_str().as_encoded_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn recovery_lock_key_from_path(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let key = name.strip_suffix(".lock")?;
    (key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(key)
}

fn read_bounded_optional_file(
    path: &Path,
    limit: u64,
    deadline: Instant,
) -> Result<Option<String>, GitError> {
    let opened = match open_recovery_file(path) {
        Ok(opened) => opened,
        Err(RecoveryOpenError::Missing) => return Ok(None),
        Err(RecoveryOpenError::Symlink) => {
            return Err(GitError::Blocked {
                message: format!(
                    "recovery is blocked because {} is a symlink instead of a regular file; replace it with a regular file and preview recovery again, or run Git manually",
                    path.display()
                ),
            });
        }
        Err(RecoveryOpenError::Io(source)) => return Err(recovery_state_io(path, source)),
    };
    if !opened.metadata.is_file() {
        return Err(GitError::Blocked {
            message: format!(
                "recovery is blocked because {} is not a regular file; replace it with a regular file and preview recovery again, or run Git manually",
                path.display()
            ),
        });
    }
    let mut opened = opened;
    let Some(file) = opened.file.as_mut() else {
        return Err(recovery_state_io(
            path,
            io::Error::other("regular recovery input has no open descriptor"),
        ));
    };
    let mut contents = Vec::new();
    let mut buffer = [0; 8192];
    while contents.len() as u64 <= limit {
        if Instant::now() >= deadline {
            return Err(GitError::TimedOut {
                args: vec!["recovery-process-configuration".to_owned()],
            });
        }
        let requested = buffer.len().min(
            limit
                .saturating_add(1)
                .saturating_sub(contents.len() as u64) as usize,
        );
        if requested == 0 {
            break;
        }
        let count = file
            .read(&mut buffer[..requested])
            .map_err(|source| recovery_state_io(path, source))?;
        if count == 0 {
            break;
        }
        contents.extend_from_slice(&buffer[..count]);
    }
    if contents.len() as u64 > limit {
        return Err(GitError::Blocked {
            message: format!(
                "recovery is blocked because {} exceeds the bounded inspection limit",
                path.display()
            ),
        });
    }
    let final_metadata = file
        .metadata()
        .map_err(|source| recovery_state_io(path, source))?;
    let reopened = match open_recovery_file(path) {
        Ok(reopened) => reopened,
        Err(RecoveryOpenError::Io(source)) => return Err(recovery_state_io(path, source)),
        Err(RecoveryOpenError::Missing | RecoveryOpenError::Symlink) => {
            return Err(recovery_path_changed(path));
        }
    };
    if !same_recovery_file(&opened.metadata, &final_metadata)
        || !opened
            .same_path_identity(&reopened)
            .map_err(|source| recovery_state_io(path, source))?
    {
        return Err(recovery_path_changed(path));
    }
    Ok(Some(String::from_utf8_lossy(&contents).into_owned()))
}

#[cfg(all(test, unix))]
fn hash_recovery_path(
    hasher: &mut Sha256,
    label: &[u8],
    path: &Path,
    deadline: Instant,
    entries: &mut usize,
) -> Result<(), GitError> {
    hash_recovery_path_fenced(hasher, label, path, deadline, entries, None)
}

fn hash_recovery_path_fenced(
    hasher: &mut Sha256,
    label: &[u8],
    path: &Path,
    deadline: Instant,
    entries: &mut usize,
    fence: Option<&mut RecoveryInputFence>,
) -> Result<(), GitError> {
    hash_recovery_path_with_fence(
        hasher,
        label,
        path,
        deadline,
        entries,
        fence,
        &mut |_, _| Ok(()),
    )
}

#[cfg(test)]
fn hash_recovery_path_with<F>(
    hasher: &mut Sha256,
    label: &[u8],
    path: &Path,
    deadline: Instant,
    entries: &mut usize,
    after_contents: &mut F,
) -> Result<(), GitError>
where
    F: FnMut(&Path, RecoveryPathKind) -> Result<(), GitError>,
{
    hash_recovery_path_with_fence(hasher, label, path, deadline, entries, None, after_contents)
}

fn hash_recovery_path_with_fence<F>(
    hasher: &mut Sha256,
    label: &[u8],
    path: &Path,
    deadline: Instant,
    entries: &mut usize,
    fence: Option<&mut RecoveryInputFence>,
    after_contents: &mut F,
) -> Result<(), GitError>
where
    F: FnMut(&Path, RecoveryPathKind) -> Result<(), GitError>,
{
    let mut byte_budget = None;
    let mut context = RecoveryHashContext {
        deadline,
        entries,
        follow_symlinks: true,
        byte_budget: &mut byte_budget,
        byte_budget_error: "recovery planning is blocked because recovery input data exceeds its fingerprint limit",
        fence,
    };
    hash_recovery_path_inner(hasher, label, path, 0, &mut context, after_contents)
}

#[derive(Default)]
struct RecoveryInputFence {
    inputs: Vec<RecoveryInput>,
    probes: Vec<RecoveryProbe>,
    remaining_rebase_commit_oids: BTreeSet<String>,
    spawn_boundary_input: Option<usize>,
}

struct RecoveryProbe {
    args: Vec<String>,
    digest: [u8; 32],
    status: Option<i32>,
    safety: RecoveryProbeSafety,
}

#[derive(Clone, Copy)]
enum RecoveryProbeSafety {
    None,
    DisabledConfig(&'static str),
    ExternalDrivers,
}

enum RecoveryInput {
    Missing(PathBuf),
    Symlink {
        path: PathBuf,
        target: PathBuf,
        metadata: fs::Metadata,
    },
    Opened {
        path: PathBuf,
        opened: RecoveryFile,
    },
}

impl RecoveryInputFence {
    fn retain_input(&mut self, label: &[u8], input: RecoveryInput) {
        if label == b"index" {
            self.spawn_boundary_input = Some(self.inputs.len());
        }
        self.inputs.push(input);
    }

    fn validate(&self, git: &Git, deadline: Instant) -> Result<(), GitError> {
        for probe in &self.probes {
            let output = match probe.safety {
                RecoveryProbeSafety::None => {
                    git.run_bounded_git_digest(probe.args.clone(), deadline)?
                }
                _ => git.run_bounded_git(probe.args.clone(), deadline, true)?,
            };
            match probe.safety {
                RecoveryProbeSafety::ExternalDrivers => {
                    git.ensure_recovery_external_drivers_safe_until(&probe.args, &output, deadline)?
                }
                _ => ensure_recovery_probe_observation_safe(probe.safety, &probe.args, &output)?,
            }
            if output.status.code() != probe.status {
                return Err(GitError::Blocked {
                    message: format!(
                        "recovery planning is blocked because Git {} status changed while it was fingerprinted",
                        probe.args.join(" ")
                    ),
                });
            }
            if !output.status.success() && output.status.code() != Some(1) {
                return Err(output.git_error(probe.args.clone()));
            }
            if output.stdout.digest != probe.digest {
                return Err(GitError::Blocked {
                    message: format!(
                        "recovery planning is blocked because Git {} output changed while it was fingerprinted",
                        probe.args.join(" ")
                    ),
                });
            }
        }
        for (index, input) in self.inputs.iter().enumerate() {
            if Some(index) != self.spawn_boundary_input {
                Self::validate_input(input, deadline)?;
            }
        }
        // Git consumes the index immediately on startup, so recheck it after
        // every other retained input at the spawn boundary.
        if let Some(index) = self.spawn_boundary_input {
            Self::validate_input(&self.inputs[index], deadline)?;
        }
        Ok(())
    }

    fn validate_input(input: &RecoveryInput, deadline: Instant) -> Result<(), GitError> {
        if Instant::now() >= deadline {
            return Err(GitError::TimedOut {
                args: vec!["recovery-state".to_owned()],
            });
        }
        match input {
            RecoveryInput::Missing(path) => match fs::symlink_metadata(path) {
                Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
                Ok(_) => Err(recovery_path_changed(path)),
                Err(source) => Err(recovery_state_io(path, source)),
            },
            RecoveryInput::Symlink {
                path,
                target,
                metadata,
            } => {
                let current_target =
                    fs::read_link(path).map_err(|source| recovery_state_io(path, source))?;
                let current_metadata = recovery_symlink_metadata(path)?;
                if current_target != *target || !same_recovery_file(metadata, &current_metadata) {
                    return Err(recovery_path_changed(path));
                }
                Ok(())
            }
            RecoveryInput::Opened { path, opened } => {
                let final_metadata = opened
                    .file
                    .as_ref()
                    .map_or_else(|| fs::symlink_metadata(path), fs::File::metadata)
                    .map_err(|source| recovery_state_io(path, source))?;
                let current = match open_recovery_file(path) {
                    Ok(current) => current,
                    Err(RecoveryOpenError::Io(source)) => {
                        return Err(recovery_state_io(path, source));
                    }
                    Err(RecoveryOpenError::Missing | RecoveryOpenError::Symlink) => {
                        return Err(recovery_path_changed(path));
                    }
                };
                if !same_recovery_file(&opened.metadata, &final_metadata)
                    || !opened
                        .same_path_identity(&current)
                        .map_err(|source| recovery_state_io(path, source))?
                {
                    return Err(recovery_path_changed(path));
                }
                Ok(())
            }
        }
    }
}

struct RecoveryHashContext<'a> {
    deadline: Instant,
    entries: &'a mut usize,
    follow_symlinks: bool,
    byte_budget: &'a mut Option<u64>,
    byte_budget_error: &'static str,
    fence: Option<&'a mut RecoveryInputFence>,
}

fn hash_recovery_path_inner<F>(
    hasher: &mut Sha256,
    label: &[u8],
    path: &Path,
    symlink_depth: usize,
    context: &mut RecoveryHashContext<'_>,
    after_contents: &mut F,
) -> Result<(), GitError>
where
    F: FnMut(&Path, RecoveryPathKind) -> Result<(), GitError>,
{
    ensure_recovery_fingerprint_capacity(context.deadline, context.entries)?;
    hash_field(hasher, label);
    let opened = match open_recovery_file(path) {
        Ok(opened) => opened,
        Err(RecoveryOpenError::Missing) => {
            hash_field(hasher, b"missing");
            if let Some(fence) = context.fence.as_deref_mut() {
                fence.retain_input(label, RecoveryInput::Missing(path.to_owned()));
            }
            return Ok(());
        }
        Err(RecoveryOpenError::Symlink) => {
            if symlink_depth >= 16 {
                return Err(GitError::Blocked {
                    message: format!(
                        "recovery planning is blocked because {} has too many symlink levels",
                        path.display()
                    ),
                });
            }
            *context.entries += 1;
            hash_field(hasher, b"symlink");
            let metadata = recovery_symlink_metadata(path)?;
            let target = fs::read_link(path).map_err(|source| recovery_state_io(path, source))?;
            let read_metadata = recovery_symlink_metadata(path)?;
            if !same_recovery_file(&metadata, &read_metadata) {
                return Err(recovery_path_changed(path));
            }
            hash_field(hasher, target.as_os_str().as_encoded_bytes());
            ensure_recovery_fingerprint_capacity(context.deadline, context.entries)?;
            if context.follow_symlinks {
                let resolved_target = if target.is_absolute() {
                    target.clone()
                } else {
                    path.parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(&target)
                };
                hash_recovery_path_inner(
                    hasher,
                    label,
                    &resolved_target,
                    symlink_depth + 1,
                    context,
                    after_contents,
                )?;
            }
            after_contents(path, RecoveryPathKind::Symlink)?;
            let final_target =
                fs::read_link(path).map_err(|source| recovery_state_io(path, source))?;
            let final_metadata = recovery_symlink_metadata(path)?;
            if final_target != target || !same_recovery_file(&metadata, &final_metadata) {
                return Err(recovery_path_changed(path));
            }
            if let Some(fence) = context.fence.as_deref_mut() {
                fence.retain_input(
                    label,
                    RecoveryInput::Symlink {
                        path: path.to_owned(),
                        target,
                        metadata,
                    },
                );
            }
            return Ok(());
        }
        Err(RecoveryOpenError::Io(source)) => return Err(recovery_state_io(path, source)),
    };
    let mut opened = opened;
    let metadata = opened.metadata.clone();
    *context.entries += 1;
    ensure_recovery_hook_observation_safe(label, &metadata)?;
    hash_field(hasher, &metadata.len().to_le_bytes());
    #[cfg(unix)]
    hash_field(hasher, &metadata.mode().to_le_bytes());
    #[cfg(not(unix))]
    hash_field(hasher, &[u8::from(metadata.permissions().readonly())]);

    if metadata.is_file() {
        hash_field(hasher, b"file");
        let Some(file) = opened.file.as_mut() else {
            return Err(recovery_state_io(
                path,
                io::Error::other("regular recovery input has no open descriptor"),
            ));
        };
        let mut buffer = [0; 64 * 1024];
        let inspect_process_safety = recovery_control_input_requires_safety_inspection(label);
        let mut process_safety_contents = Vec::new();
        loop {
            if Instant::now() >= context.deadline {
                return Err(GitError::TimedOut {
                    args: vec!["recovery-state".to_owned()],
                });
            }
            let count = file
                .read(&mut buffer)
                .map_err(|source| recovery_state_io(path, source))?;
            if count == 0 {
                break;
            }
            if let Some(remaining) = context.byte_budget.as_mut() {
                let Some(updated) = remaining.checked_sub(count as u64) else {
                    return Err(GitError::Blocked {
                        message: context.byte_budget_error.to_owned(),
                    });
                };
                *remaining = updated;
            }
            hasher.update(&buffer[..count]);
            if inspect_process_safety {
                if process_safety_contents.len().saturating_add(count)
                    > RECOVERY_REBASE_TODO_LIMIT as usize
                {
                    return Err(GitError::Blocked {
                        message: "recovery is blocked because a process-controlling rebase input exceeds the bounded inspection limit"
                            .to_owned(),
                    });
                }
                process_safety_contents.extend_from_slice(&buffer[..count]);
            }
        }
        ensure_recovery_control_observation_safe(label, &process_safety_contents)?;
        if matches!(
            label,
            b"rebase-merge/git-rebase-todo" | b"rebase-apply/git-rebase-todo"
        ) {
            let relative = String::from_utf8_lossy(label);
            let contents = String::from_utf8_lossy(&process_safety_contents);
            let oids = parse_remaining_rebase_commit_oids(&relative, &contents)?;
            if let Some(fence) = context.fence.as_deref_mut() {
                fence.remaining_rebase_commit_oids.extend(oids);
            }
        }
        let final_metadata = file
            .metadata()
            .map_err(|source| recovery_state_io(path, source))?;
        after_contents(path, RecoveryPathKind::File)?;
        let reopened = match open_recovery_file(path) {
            Ok(reopened) => reopened,
            Err(RecoveryOpenError::Io(source)) => return Err(recovery_state_io(path, source)),
            Err(RecoveryOpenError::Missing | RecoveryOpenError::Symlink) => {
                return Err(GitError::Blocked {
                    message: format!(
                        "recovery planning is blocked because {} changed while it was fingerprinted",
                        path.display()
                    ),
                });
            }
        };
        if !same_recovery_file(&metadata, &final_metadata)
            || !opened
                .same_path_identity(&reopened)
                .map_err(|source| recovery_state_io(path, source))?
        {
            return Err(GitError::Blocked {
                message: format!(
                    "recovery planning is blocked because {} changed while it was fingerprinted",
                    path.display()
                ),
            });
        }
    } else if metadata.is_dir() {
        hash_field(hasher, b"directory");
        let mut children = recovery_directory_children(
            opened.file.as_ref(),
            path,
            context.deadline,
            context.entries,
        )?;
        children.sort();
        for child in children {
            ensure_recovery_fingerprint_capacity(context.deadline, context.entries)?;
            let mut child_label = label.to_vec();
            child_label.push(b'/');
            child_label.extend_from_slice(child.as_encoded_bytes());
            hash_recovery_path_inner(
                hasher,
                &child_label,
                &path.join(child),
                symlink_depth,
                context,
                after_contents,
            )?;
        }
        after_contents(path, RecoveryPathKind::Directory)?;
        let final_metadata = match opened.file.as_ref() {
            Some(file) => file
                .metadata()
                .map_err(|source| recovery_state_io(path, source))?,
            None => fs::symlink_metadata(path).map_err(|source| recovery_state_io(path, source))?,
        };
        let reopened = match open_recovery_file(path) {
            Ok(reopened) => reopened,
            Err(RecoveryOpenError::Io(source)) => return Err(recovery_state_io(path, source)),
            Err(RecoveryOpenError::Missing | RecoveryOpenError::Symlink) => {
                return Err(recovery_path_changed(path));
            }
        };
        if !final_metadata.is_dir()
            || !same_recovery_file(&metadata, &final_metadata)
            || !opened
                .same_path_identity(&reopened)
                .map_err(|source| recovery_state_io(path, source))?
        {
            return Err(recovery_path_changed(path));
        }
    } else {
        return Err(GitError::Blocked {
            message: format!(
                "recovery planning is blocked because {} is not a regular file or directory",
                path.display()
            ),
        });
    }
    if let Some(fence) = context.fence.as_deref_mut() {
        fence.retain_input(
            label,
            RecoveryInput::Opened {
                path: path.to_owned(),
                opened,
            },
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryPathKind {
    File,
    Symlink,
    Directory,
}

fn recovery_symlink_metadata(path: &Path) -> Result<fs::Metadata, GitError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| recovery_state_io(path, source))?;
    if !metadata.file_type().is_symlink() {
        return Err(recovery_path_changed(path));
    }
    Ok(metadata)
}

fn recovery_path_changed(path: &Path) -> GitError {
    GitError::Blocked {
        message: format!(
            "recovery planning is blocked because {} changed while it was fingerprinted",
            path.display()
        ),
    }
}

fn recovery_state_changed(operation: RepositoryOperation, action: RecoveryAction) -> GitError {
    GitError::Blocked {
        message: format!(
            "{} {} is blocked because repository state changed after preview",
            operation.label(),
            action.label()
        ),
    }
}

struct RecoveryFile {
    file: Option<fs::File>,
    metadata: fs::Metadata,
    #[cfg(windows)]
    identity: same_file::Handle,
}

impl RecoveryFile {
    #[cfg(windows)]
    fn same_path_identity(&self, current: &Self) -> io::Result<bool> {
        Ok(self.identity == current.identity)
    }

    #[cfg(unix)]
    fn same_path_identity(&self, current: &Self) -> io::Result<bool> {
        Ok(self.metadata.dev() == current.metadata.dev()
            && self.metadata.ino() == current.metadata.ino())
    }

    #[cfg(not(any(unix, windows)))]
    fn same_path_identity(&self, current: &Self) -> io::Result<bool> {
        Ok(same_recovery_file(&self.metadata, &current.metadata))
    }
}

enum RecoveryOpenError {
    Missing,
    Symlink,
    Io(io::Error),
}

#[cfg(unix)]
fn recovery_directory_children(
    file: Option<&fs::File>,
    path: &Path,
    deadline: Instant,
    entries: &usize,
) -> Result<Vec<OsString>, GitError> {
    use rustix::fs::Dir;

    let Some(file) = file else {
        return Err(recovery_state_io(
            path,
            io::Error::other("recovery directory has no open descriptor"),
        ));
    };
    let mut children = Vec::new();
    let directory =
        Dir::read_from(file).map_err(|source| recovery_state_io(path, io::Error::from(source)))?;
    for child in directory {
        ensure_recovery_fingerprint_capacity(deadline, entries)?;
        if children.len() + *entries >= RECOVERY_STATE_ENTRY_LIMIT {
            return Err(GitError::Blocked {
                message: "recovery planning is blocked because recovery state has too many entries"
                    .to_owned(),
            });
        }
        let child = child.map_err(|source| recovery_state_io(path, io::Error::from(source)))?;
        let name = child.file_name().to_bytes();
        if name != b"." && name != b".." {
            children.push(OsString::from_vec(name.to_vec()));
        }
    }
    Ok(children)
}

#[cfg(not(unix))]
fn recovery_directory_children(
    _file: Option<&fs::File>,
    path: &Path,
    deadline: Instant,
    entries: &usize,
) -> Result<Vec<OsString>, GitError> {
    let mut children = Vec::new();
    for child in fs::read_dir(path).map_err(|source| recovery_state_io(path, source))? {
        ensure_recovery_fingerprint_capacity(deadline, entries)?;
        if children.len() + *entries >= RECOVERY_STATE_ENTRY_LIMIT {
            return Err(GitError::Blocked {
                message: "recovery planning is blocked because recovery state has too many entries"
                    .to_owned(),
            });
        }
        children.push(
            child
                .map_err(|source| recovery_state_io(path, source))?
                .file_name(),
        );
    }
    Ok(children)
}

#[cfg(unix)]
fn open_recovery_file(path: &Path) -> Result<RecoveryFile, RecoveryOpenError> {
    let descriptor = match open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(Errno::NOENT) => return Err(RecoveryOpenError::Missing),
        Err(Errno::LOOP) => return Err(RecoveryOpenError::Symlink),
        Err(source) => return Err(RecoveryOpenError::Io(source.into())),
    };
    let file = fs::File::from(descriptor);
    let metadata = file.metadata().map_err(RecoveryOpenError::Io)?;
    Ok(RecoveryFile {
        file: Some(file),
        metadata,
    })
}

#[cfg(windows)]
fn open_recovery_file(path: &Path) -> Result<RecoveryFile, RecoveryOpenError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(RecoveryOpenError::Missing);
        }
        Err(source) => return Err(RecoveryOpenError::Io(source)),
    };
    if metadata.file_type().is_symlink() {
        return Err(RecoveryOpenError::Symlink);
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .map_err(RecoveryOpenError::Io)?;
    let metadata = file.metadata().map_err(RecoveryOpenError::Io)?;
    let identity = same_file::Handle::from_file(file.try_clone().map_err(RecoveryOpenError::Io)?)
        .map_err(RecoveryOpenError::Io)?;
    Ok(RecoveryFile {
        file: Some(file),
        metadata,
        identity,
    })
}

#[cfg(not(any(unix, windows)))]
fn open_recovery_file(path: &Path) -> Result<RecoveryFile, RecoveryOpenError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(RecoveryOpenError::Missing);
        }
        Err(source) => return Err(RecoveryOpenError::Io(source)),
    };
    if metadata.file_type().is_symlink() {
        return Err(RecoveryOpenError::Symlink);
    }
    let file = if metadata.is_file() {
        Some(fs::File::open(path).map_err(RecoveryOpenError::Io)?)
    } else {
        None
    };
    Ok(RecoveryFile { file, metadata })
}

#[cfg(unix)]
fn same_recovery_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.mode() == right.mode()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(windows)]
fn same_recovery_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.is_file() == right.is_file()
        && left.len() == right.len()
        && left.permissions().readonly() == right.permissions().readonly()
        && left.modified().ok() == right.modified().ok()
}

#[cfg(not(any(unix, windows)))]
fn same_recovery_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.is_file() == right.is_file()
        && left.len() == right.len()
        && left.permissions().readonly() == right.permissions().readonly()
        && left.modified().ok() == right.modified().ok()
}

fn ensure_recovery_fingerprint_capacity(
    deadline: Instant,
    entries: &usize,
) -> Result<(), GitError> {
    if Instant::now() >= deadline {
        return Err(GitError::TimedOut {
            args: vec!["recovery-state".to_owned()],
        });
    }
    if *entries >= RECOVERY_STATE_ENTRY_LIMIT {
        return Err(GitError::Blocked {
            message: "recovery planning is blocked because recovery state has too many entries"
                .to_owned(),
        });
    }
    Ok(())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn recovery_state_io(path: &Path, source: io::Error) -> GitError {
    GitError::Io {
        args: vec![format!("recovery-state {}", path.display())],
        source,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub root: PathBuf,
    pub branch: BranchState,
    pub remotes: Vec<Remote>,
    pub status: WorktreeStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadTarget {
    pub oid: Option<String>,
    pub reference: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    pub reference: String,
    pub oid: String,
    pub upstream: Option<String>,
    pub current: bool,
    pub kind: BranchKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchTarget {
    pub name: String,
    pub reference: String,
    pub oid: String,
    pub kind: BranchKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchKind {
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawGitOutput {
    stdout: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawProcessOutput {
    args: Vec<String>,
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug, Clone)]
struct BoundedCommandOutput {
    status: ExitStatus,
    stdout: CapturedStream,
    stderr: CapturedStream,
}

impl BoundedCommandOutput {
    fn git_error(self, args: Vec<String>) -> GitError {
        GitError::GitFailed {
            args,
            status: self.status,
            stdout: self.stdout.lossy_text(),
            stderr: self.stderr.lossy_text(),
        }
    }
}

#[derive(Debug, Clone)]
struct CapturedStream {
    bytes: Vec<u8>,
    digest: [u8; 32],
    truncated: bool,
}

impl CapturedStream {
    fn lossy_text(self) -> String {
        let mut text = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.truncated {
            if !text.ends_with('\n') && !text.is_empty() {
                text.push('\n');
            }
            text.push_str("[output truncated by bitbygit]\n");
        }
        text
    }
}

#[derive(Debug)]
pub enum GitError {
    Io {
        args: Vec<String>,
        source: std::io::Error,
    },
    Utf8 {
        args: Vec<String>,
        stream: OutputStream,
        source: FromUtf8Error,
    },
    GitFailed {
        args: Vec<String>,
        status: ExitStatus,
        stdout: String,
        stderr: String,
    },
    TimedOut {
        args: Vec<String>,
    },
    Blocked {
        message: String,
    },
    Parse {
        message: String,
    },
}

impl Display for GitError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { args, source } => {
                write!(formatter, "failed to run git {}: {source}", args.join(" "))
            }
            Self::Utf8 {
                args,
                stream,
                source,
            } => write!(
                formatter,
                "git {} returned non-UTF-8 {stream}: {source}",
                args.join(" ")
            ),
            Self::GitFailed {
                args,
                status,
                stdout,
                stderr,
                ..
            } => {
                let detail = if !stderr.trim().is_empty() {
                    stderr.trim()
                } else if !stdout.trim().is_empty() {
                    stdout.trim()
                } else {
                    "no output"
                };
                write!(
                    formatter,
                    "git {} failed with status {status}: {detail}",
                    args.join(" ")
                )
            }
            Self::TimedOut { args } => {
                write!(formatter, "git {} timed out", args.join(" "))
            }
            Self::Blocked { message } => formatter.write_str(message),
            Self::Parse { message } => write!(formatter, "failed to parse git output: {message}"),
        }
    }
}

impl Error for GitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Utf8 { source, .. } => Some(source),
            Self::GitFailed { .. }
            | Self::TimedOut { .. }
            | Self::Blocked { .. }
            | Self::Parse { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

impl Display for OutputStream {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdout => formatter.write_str("stdout"),
            Self::Stderr => formatter.write_str("stderr"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchState {
    pub head: Head,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub unborn: bool,
}

impl Default for BranchState {
    fn default() -> Self {
        Self {
            head: Head::Unborn,
            upstream: None,
            ahead: 0,
            behind: 0,
            unborn: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    Branch(String),
    Detached(String),
    Unborn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryOperation {
    Merge,
    Rebase,
}

impl RepositoryOperation {
    fn label(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    Continue,
    Abort,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryState {
    fingerprint: [u8; 32],
}

impl RecoveryAction {
    fn label(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Abort => "abort",
            Self::Skip => "skip",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub fetch_url: Option<String>,
    pub push_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeStatus {
    pub branch: BranchState,
    pub entries: Vec<StatusEntry>,
    pub operation: Option<RepositoryOperation>,
}

impl WorktreeStatus {
    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn staged_files(&self) -> Vec<&StatusEntry> {
        self.entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.entry_type,
                    StatusEntryType::Ordinary | StatusEntryType::Renamed | StatusEntryType::Copied
                ) && entry.index != ChangeKind::Unmodified
            })
            .collect()
    }

    pub fn unstaged_files(&self) -> Vec<&StatusEntry> {
        self.entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.entry_type,
                    StatusEntryType::Ordinary | StatusEntryType::Renamed | StatusEntryType::Copied
                ) && entry.worktree != ChangeKind::Unmodified
            })
            .collect()
    }

    pub fn untracked_files(&self) -> Vec<&StatusEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.entry_type == StatusEntryType::Untracked)
            .collect()
    }

    pub fn conflicted_files(&self) -> Vec<&StatusEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.entry_type == StatusEntryType::Conflict)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: PathBuf,
    pub original_path: Option<PathBuf>,
    pub index: ChangeKind,
    pub worktree: ChangeKind,
    pub entry_type: StatusEntryType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusEntryType {
    Ordinary,
    Renamed,
    /// Porcelain v2 reserves the `2` record family for renames and copies. Git
    /// status does not normally emit copies, but the parser keeps the model
    /// explicit for forward-compatible callers.
    Copied,
    Untracked,
    Ignored,
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Unmodified,
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Unmerged,
    Untracked,
    Ignored,
    Unknown(char),
}

#[cfg(test)]
fn parse_status(input: &str) -> Result<WorktreeStatus, GitError> {
    parse_status_bytes(input.as_bytes())
}

fn parse_status_bytes(input: &[u8]) -> Result<WorktreeStatus, GitError> {
    let mut branch = BranchState::default();
    let mut oid = None;
    let mut entries = Vec::new();
    let nul_delimited = input.contains(&0);
    let mut records: Box<dyn Iterator<Item = &[u8]> + '_> = if nul_delimited {
        Box::new(
            input
                .split(|byte| *byte == 0)
                .filter(|record| !record.is_empty()),
        )
    } else {
        Box::new(
            input
                .split(|byte| *byte == b'\n')
                .filter(|record| !record.is_empty()),
        )
    };

    while let Some(line) = records.next() {
        if let Some(value) = strip_bytes_prefix(line, b"# branch.oid ") {
            let value = parse_utf8(value, "branch oid")?;
            oid = Some(value.to_owned());
            if value == "(initial)" {
                branch.unborn = true;
            }
            continue;
        }

        if let Some(value) = strip_bytes_prefix(line, b"# branch.head ") {
            let value = parse_utf8(value, "branch head")?;
            branch.head = if value == "(detached)" {
                Head::Detached(oid.clone().unwrap_or_default())
            } else {
                Head::Branch(value.to_owned())
            };
            continue;
        }

        if let Some(value) = strip_bytes_prefix(line, b"# branch.upstream ") {
            let value = parse_utf8(value, "branch upstream")?;
            branch.upstream = Some(value.to_owned());
            continue;
        }

        if let Some(value) = strip_bytes_prefix(line, b"# branch.ab ") {
            let value = parse_utf8(value, "branch ahead/behind")?;
            let (ahead, behind) = parse_ahead_behind(value)?;
            branch.ahead = ahead;
            branch.behind = behind;
            continue;
        }

        if let Some(path) = strip_bytes_prefix(line, b"? ") {
            entries.push(StatusEntry {
                path: path_from_bytes(path),
                original_path: None,
                index: ChangeKind::Unmodified,
                worktree: ChangeKind::Untracked,
                entry_type: StatusEntryType::Untracked,
            });
            continue;
        }

        if let Some(path) = strip_bytes_prefix(line, b"! ") {
            entries.push(StatusEntry {
                path: path_from_bytes(path),
                original_path: None,
                index: ChangeKind::Unmodified,
                worktree: ChangeKind::Ignored,
                entry_type: StatusEntryType::Ignored,
            });
            continue;
        }

        if line.starts_with(b"1 ") {
            entries.push(parse_ordinary_entry(line)?);
            continue;
        }

        if line.starts_with(b"2 ") {
            let original_path = if nul_delimited {
                Some(
                    records
                        .next()
                        .ok_or_else(|| parse_error("renamed entry missing original path"))?,
                )
            } else {
                None
            };
            entries.push(parse_renamed_entry(line, original_path)?);
            continue;
        }

        if line.starts_with(b"u ") {
            entries.push(parse_conflict_entry(line)?);
            continue;
        }

        if !line.starts_with(b"# ") {
            return Err(parse_error(&format!(
                "unknown porcelain v2 record: {}",
                String::from_utf8_lossy(line)
            )));
        }
    }

    Ok(WorktreeStatus {
        branch,
        entries,
        operation: None,
    })
}

fn parse_ahead_behind(value: &str) -> Result<(u32, u32), GitError> {
    let mut parts = value.split_whitespace();
    let ahead = parts
        .next()
        .ok_or_else(|| parse_error("missing ahead count"))?
        .strip_prefix('+')
        .ok_or_else(|| parse_error("ahead count must start with +"))?
        .parse::<u32>()
        .map_err(|_| parse_error("ahead count is not a number"))?;
    let behind = parts
        .next()
        .ok_or_else(|| parse_error("missing behind count"))?
        .strip_prefix('-')
        .ok_or_else(|| parse_error("behind count must start with -"))?
        .parse::<u32>()
        .map_err(|_| parse_error("behind count is not a number"))?;
    Ok((ahead, behind))
}

fn parse_ordinary_entry(line: &[u8]) -> Result<StatusEntry, GitError> {
    let parts = splitn_bytes(line, b' ', 9);
    let xy = parts
        .get(1)
        .copied()
        .ok_or_else(|| parse_error("ordinary entry missing status"))?;
    let path = parts
        .get(8)
        .copied()
        .ok_or_else(|| parse_error("ordinary entry missing path"))?;
    let (index, worktree) = parse_xy(xy)?;

    Ok(StatusEntry {
        path: path_from_bytes(path),
        original_path: None,
        index,
        worktree,
        entry_type: StatusEntryType::Ordinary,
    })
}

fn parse_renamed_entry(line: &[u8], original_path: Option<&[u8]>) -> Result<StatusEntry, GitError> {
    let parts = splitn_bytes(line, b' ', 10);
    let xy = parts
        .get(1)
        .copied()
        .ok_or_else(|| parse_error("renamed entry missing status"))?;
    let path = parts
        .get(9)
        .copied()
        .ok_or_else(|| parse_error("renamed entry missing path"))?;
    let (path, original_path) = match original_path {
        Some(original_path) => (path, original_path),
        None => split_once_byte(path, b'\t')
            .ok_or_else(|| parse_error("renamed entry missing original path"))?,
    };
    let (index, worktree) = parse_xy(xy)?;

    Ok(StatusEntry {
        path: path_from_bytes(path),
        original_path: Some(path_from_bytes(original_path)),
        index,
        worktree,
        entry_type: if index == ChangeKind::Copied || worktree == ChangeKind::Copied {
            StatusEntryType::Copied
        } else {
            StatusEntryType::Renamed
        },
    })
}

fn parse_conflict_entry(line: &[u8]) -> Result<StatusEntry, GitError> {
    let parts = splitn_bytes(line, b' ', 11);
    let path = parts
        .get(10)
        .copied()
        .ok_or_else(|| parse_error("conflict entry missing path"))?;

    Ok(StatusEntry {
        path: path_from_bytes(path),
        original_path: None,
        index: ChangeKind::Unmerged,
        worktree: ChangeKind::Unmerged,
        entry_type: StatusEntryType::Conflict,
    })
}

fn parse_xy(value: &[u8]) -> Result<(ChangeKind, ChangeKind), GitError> {
    let index = value
        .first()
        .copied()
        .ok_or_else(|| parse_error("missing index status"))?;
    let worktree = value
        .get(1)
        .copied()
        .ok_or_else(|| parse_error("missing worktree status"))?;
    Ok((change_kind(index), change_kind(worktree)))
}

fn change_kind(value: u8) -> ChangeKind {
    match value {
        b'.' => ChangeKind::Unmodified,
        b'M' => ChangeKind::Modified,
        b'A' => ChangeKind::Added,
        b'D' => ChangeKind::Deleted,
        b'R' => ChangeKind::Renamed,
        b'C' => ChangeKind::Copied,
        b'U' => ChangeKind::Unmerged,
        b'?' => ChangeKind::Untracked,
        b'!' => ChangeKind::Ignored,
        other => ChangeKind::Unknown(char::from(other)),
    }
}

fn parse_remotes(input: &str) -> Vec<Remote> {
    let mut remotes = BTreeMap::<String, Remote>::new();

    for line in input.lines() {
        let Some((name, rest)) = line.split_once('\t') else {
            continue;
        };
        let Some((url, kind)) = rest.rsplit_once(' ') else {
            continue;
        };

        let remote = remotes.entry(name.to_owned()).or_insert_with(|| Remote {
            name: name.to_owned(),
            fetch_url: None,
            push_url: None,
        });

        match kind {
            "(fetch)" => remote.fetch_url = Some(url.to_owned()),
            "(push)" => remote.push_url = Some(url.to_owned()),
            _ => {}
        }
    }

    remotes.into_values().collect()
}

fn parse_branches(input: &str) -> Result<Vec<BranchInfo>, GitError> {
    let mut branches = Vec::new();
    for line in input.lines().filter(|line| !line.trim().is_empty()) {
        let branch = parse_branch_line(line)?;
        if branch.kind == BranchKind::Remote && branch.name.ends_with("/HEAD") {
            continue;
        }
        branches.push(branch);
    }
    Ok(branches)
}

fn parse_branch_line(line: &str) -> Result<BranchInfo, GitError> {
    let fields = line.split('\0').collect::<Vec<_>>();
    let [reference, oid, upstream, head] = fields.as_slice() else {
        return Err(GitError::Parse {
            message: "git branch list output has unexpected fields".to_owned(),
        });
    };
    let (kind, name) = if let Some(name) = reference.strip_prefix("refs/heads/") {
        (BranchKind::Local, name)
    } else if let Some(name) = reference.strip_prefix("refs/remotes/") {
        (BranchKind::Remote, name)
    } else {
        return Err(GitError::Parse {
            message: format!("unsupported branch reference: {reference}"),
        });
    };
    Ok(BranchInfo {
        name: name.to_owned(),
        reference: (*reference).to_owned(),
        oid: (*oid).to_owned(),
        upstream: (!upstream.is_empty()).then(|| (*upstream).to_owned()),
        current: *head == "*",
        kind,
    })
}

fn local_name_for_remote_branch(name: &str) -> Option<&str> {
    name.split_once('/')
        .map(|(_remote, branch)| branch)
        .filter(|branch| !branch.is_empty())
}

fn strip_byte_line_ending(value: &[u8]) -> &[u8] {
    value.strip_suffix(b"\n").unwrap_or(value)
}

fn strip_bytes_prefix<'a>(value: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    value.strip_prefix(prefix)
}

fn parse_utf8<'a>(value: &'a [u8], description: &str) -> Result<&'a str, GitError> {
    std::str::from_utf8(value).map_err(|_| parse_error(&format!("{description} is not UTF-8")))
}

fn splitn_bytes(value: &[u8], delimiter: u8, count: usize) -> Vec<&[u8]> {
    value.splitn(count, |byte| *byte == delimiter).collect()
}

fn split_once_byte(value: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let index = value.iter().position(|byte| *byte == delimiter)?;
    Some((&value[..index], &value[index + 1..]))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn path_from_bytes(value: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(value.to_vec()))
}

#[cfg(not(unix))]
fn path_from_bytes(value: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(value).into_owned())
}

fn parse_error(message: &str) -> GitError {
    GitError::Parse {
        message: message.to_owned(),
    }
}

fn remote_tracking_ref(remote: &str, branch: &str) -> String {
    format!("refs/remotes/{remote}/{branch}")
}

fn force_with_lease_arg(branch: &str, expected_remote_oid: Option<&str>) -> String {
    format!(
        "--force-with-lease=refs/heads/{branch}:{}",
        expected_remote_oid.unwrap_or("")
    )
}

fn combine_outputs(first: GitOutput, second: GitOutput) -> GitOutput {
    let stdout = [first.stdout.trim(), second.stdout.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let stderr = [first.stderr.trim(), second.stderr.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    GitOutput {
        status: second.status,
        stdout,
        stderr,
    }
}

#[cfg(unix)]
fn hook_is_enabled(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn hook_is_enabled(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

fn ensure_recovery_hook_observation_safe(
    label: &[u8],
    metadata: &fs::Metadata,
) -> Result<(), GitError> {
    let Some(hook) = RECOVERY_HOOKS
        .iter()
        .find(|hook| label.strip_prefix(b"hooks/") == Some(hook.as_bytes()))
    else {
        return Ok(());
    };
    #[cfg(unix)]
    let enabled = metadata.is_file() && metadata.mode() & 0o111 != 0;
    #[cfg(not(unix))]
    let enabled = metadata.is_file();
    if enabled {
        return Err(GitError::Blocked {
            message: format!(
                "recovery is blocked because executable Git hooks may start uncontained processes: {hook}; disable them and preview recovery again, or run Git manually"
            ),
        });
    }
    Ok(())
}

fn recovery_control_input_requires_safety_inspection(label: &[u8]) -> bool {
    matches!(
        label,
        b"rebase-merge/gpg_sign_opt"
            | b"rebase-apply/gpg_sign_opt"
            | b"rebase-merge/strategy"
            | b"rebase-apply/strategy"
            | b"rebase-merge/strategy_opts"
            | b"rebase-apply/strategy_opts"
            | b"rebase-merge/git-rebase-todo"
            | b"rebase-apply/git-rebase-todo"
    )
}

fn ensure_recovery_control_observation_safe(label: &[u8], contents: &[u8]) -> Result<(), GitError> {
    if !recovery_control_input_requires_safety_inspection(label) {
        return Ok(());
    }
    let nonempty = contents.iter().any(|byte| !byte.is_ascii_whitespace());
    if matches!(
        label,
        b"rebase-merge/gpg_sign_opt" | b"rebase-apply/gpg_sign_opt"
    ) && nonempty
    {
        return Err(GitError::Blocked {
            message: "recovery is blocked because the active rebase requests commit signing with --gpg-sign, which may start an uncontained signer; abort and restart the rebase without signing, or run Git manually"
                .to_owned(),
        });
    }
    if matches!(label, b"rebase-merge/strategy" | b"rebase-apply/strategy") && nonempty {
        return Err(GitError::Blocked {
            message: "recovery is blocked because the active rebase persists an explicit merge strategy, which may start an uncontained executable; abort and restart without custom strategy settings, or run Git manually"
                .to_owned(),
        });
    }
    if matches!(
        label,
        b"rebase-merge/strategy_opts" | b"rebase-apply/strategy_opts"
    ) && nonempty
    {
        return Err(GitError::Blocked {
            message: "recovery is blocked because the active rebase persists an explicit merge strategy option, which may start an uncontained executable; abort and restart without custom strategy settings, or run Git manually"
                .to_owned(),
        });
    }
    if matches!(
        label,
        b"rebase-merge/git-rebase-todo" | b"rebase-apply/git-rebase-todo"
    ) && contents.split(|byte| *byte == b'\n').any(|line| {
        matches!(
            line.split(|byte| byte.is_ascii_whitespace())
                .find(|field| !field.is_empty()),
            Some(b"exec" | b"x")
        )
    }) {
        return Err(GitError::Blocked {
            message: "recovery is blocked because the remaining rebase plan contains an exec command that may start uncontained processes; remove it and preview recovery again, or run Git manually"
                .to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[cfg(unix)]
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    static NEXT_REPO_ID: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn parses_clean_status() -> Result<(), Box<dyn Error>> {
        let status = parse_status(
            "# branch.oid 1234567\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +0 -0\n",
        )?;

        assert!(status.is_clean());
        assert_eq!(status.branch.head, Head::Branch("main".to_owned()));
        assert_eq!(status.branch.upstream, Some("origin/main".to_owned()));
        assert_eq!(status.branch.ahead, 0);
        assert_eq!(status.branch.behind, 0);
        assert_eq!(status.operation, None);
        Ok(())
    }

    #[test]
    fn parses_dirty_unstaged_file() -> Result<(), Box<dyn Error>> {
        let status = parse_status(
            "# branch.oid 1234567\n# branch.head main\n1 .M N... 100644 100644 100644 abc abc README.md\n",
        )?;

        assert_eq!(status.unstaged_files().len(), 1);
        assert_eq!(status.entries[0].path, PathBuf::from("README.md"));
        assert_eq!(status.entries[0].worktree, ChangeKind::Modified);
        Ok(())
    }

    #[test]
    fn parses_staged_file() -> Result<(), Box<dyn Error>> {
        let status = parse_status(
            "# branch.oid 1234567\n# branch.head main\n1 A. N... 000000 100644 100644 zero abc src/main.rs\n",
        )?;

        assert_eq!(status.staged_files().len(), 1);
        assert_eq!(status.entries[0].index, ChangeKind::Added);
        Ok(())
    }

    #[test]
    fn parses_untracked_file() -> Result<(), Box<dyn Error>> {
        let status = parse_status("# branch.head main\n? notes.txt\n")?;

        assert_eq!(status.untracked_files().len(), 1);
        assert_eq!(status.entries[0].entry_type, StatusEntryType::Untracked);
        Ok(())
    }

    #[test]
    fn parses_renamed_file() -> Result<(), Box<dyn Error>> {
        let status = parse_status(
            "# branch.head main\n2 R. N... 100644 100644 100644 abc def R100 new.txt\told.txt\n",
        )?;

        assert_eq!(status.entries[0].entry_type, StatusEntryType::Renamed);
        assert_eq!(status.entries[0].path, PathBuf::from("new.txt"));
        assert_eq!(
            status.entries[0].original_path,
            Some(PathBuf::from("old.txt"))
        );
        Ok(())
    }

    #[test]
    fn parses_conflicted_file() -> Result<(), Box<dyn Error>> {
        let status = parse_status(
            "# branch.head main\nu UU N... 100644 100644 100644 100644 one two three conflict.txt\n",
        )?;

        assert_eq!(status.conflicted_files().len(), 1);
        assert_eq!(status.staged_files().len(), 0);
        assert_eq!(status.entries[0].entry_type, StatusEntryType::Conflict);
        Ok(())
    }

    #[test]
    fn parses_detached_head() -> Result<(), Box<dyn Error>> {
        let status = parse_status("# branch.oid abc123\n# branch.head (detached)\n")?;

        assert_eq!(status.branch.head, Head::Detached("abc123".to_owned()));
        Ok(())
    }

    #[test]
    fn parses_unborn_branch() -> Result<(), Box<dyn Error>> {
        let status = parse_status("# branch.oid (initial)\n# branch.head main\n")?;

        assert_eq!(status.branch.head, Head::Branch("main".to_owned()));
        assert!(status.branch.unborn);
        Ok(())
    }

    #[test]
    fn parses_nul_delimited_raw_paths() -> Result<(), Box<dyn Error>> {
        let status = parse_status("# branch.head main\0? café.txt\0? tab\tname.txt\0")?;

        assert_eq!(status.untracked_files().len(), 2);
        assert_eq!(status.entries[0].path, PathBuf::from("café.txt"));
        assert_eq!(status.entries[1].path, PathBuf::from("tab\tname.txt"));
        Ok(())
    }

    #[test]
    fn parses_nul_delimited_rename() -> Result<(), Box<dyn Error>> {
        let status = parse_status(concat!(
            "# branch.head main\0",
            "2 R. N... 100644 100644 100644 abc def R100 new\tname.txt\0",
            "old name.txt\0"
        ))?;

        assert_eq!(status.entries[0].entry_type, StatusEntryType::Renamed);
        assert_eq!(status.entries[0].path, PathBuf::from("new\tname.txt"));
        assert_eq!(
            status.entries[0].original_path,
            Some(PathBuf::from("old name.txt"))
        );
        Ok(())
    }

    #[test]
    fn parses_copied_file() -> Result<(), Box<dyn Error>> {
        let status = parse_status(concat!(
            "# branch.head main\0",
            "2 C. N... 100644 100644 100644 abc def C100 copy.txt\0",
            "source.txt\0"
        ))?;

        assert_eq!(status.entries[0].entry_type, StatusEntryType::Copied);
        assert_eq!(status.staged_files().len(), 1);
        Ok(())
    }

    #[test]
    fn parses_remotes() {
        let remotes = parse_remotes(
            "origin\thttps://github.com/cosentinode/bitbygit.git (fetch)\norigin\tgit@github.com:cosentinode/bitbygit.git (push)\n",
        );

        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].name, "origin");
        assert_eq!(
            remotes[0].fetch_url,
            Some("https://github.com/cosentinode/bitbygit.git".to_owned())
        );
        assert_eq!(
            remotes[0].push_url,
            Some("git@github.com:cosentinode/bitbygit.git".to_owned())
        );
    }

    #[test]
    fn parses_branches_and_skips_remote_head() -> Result<(), Box<dyn Error>> {
        let branches = parse_branches(
            "refs/heads/main\x001111111111111111111111111111111111111111\x00origin/main\x00*\nrefs/remotes/origin/main\x002222222222222222222222222222222222222222\x00\x00\nrefs/remotes/origin/HEAD\x002222222222222222222222222222222222222222\x00\x00\n",
        )?;

        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0].name, "main");
        assert_eq!(branches[0].kind, BranchKind::Local);
        assert!(branches[0].current);
        assert_eq!(branches[0].upstream.as_deref(), Some("origin/main"));
        assert_eq!(branches[1].name, "origin/main");
        assert_eq!(branches[1].kind, BranchKind::Remote);
        Ok(())
    }

    #[test]
    fn branch_target_blocks_ambiguous_local_and_remote_names() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        repo.run(["branch", "origin/main"])?;
        repo.run(["update-ref", "refs/remotes/origin/main", "HEAD"])?;
        let git = Git::new(repo.path());

        let result = git.branch_target("origin/main");

        let Err(error) = result else {
            return Err("expected ambiguous branch guardrail".into());
        };
        assert!(error.to_string().contains("ambiguous"));
        Ok(())
    }

    #[test]
    fn reads_repository_state_from_temp_repo() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        repo.write("README.md", "changed\n")?;
        repo.write("staged.txt", "staged\n")?;
        repo.run(["add", "staged.txt"])?;
        repo.write("untracked.txt", "untracked\n")?;

        let git = Git::new(repo.path());
        let state = git.repository()?;

        assert_eq!(state.root, repo.path());
        assert_eq!(state.branch.head, Head::Branch("main".to_owned()));
        assert_eq!(state.status.staged_files().len(), 1);
        assert_eq!(state.status.unstaged_files().len(), 1);
        assert_eq!(state.status.untracked_files().len(), 1);
        Ok(())
    }

    #[test]
    fn branch_workflows_create_checkout_merge_and_rebase() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        let git = Git::new(repo.path());

        git.create_branch("feature", None, &git.head_target()?)?;
        repo.write("feature.txt", "feature\n")?;
        repo.run(["add", "feature.txt"])?;
        repo.run(["commit", "-m", "feature"])?;
        let feature = git.branch_target("feature")?.ok_or("missing feature")?;
        let main = git.branch_target("main")?.ok_or("missing main")?;
        git.checkout_branch(&main, &git.head_target()?)?;
        git.merge_ff_only(&feature, &git.head_target()?)?;
        assert!(repo.path().join("feature.txt").exists());

        git.create_branch("topic", None, &git.head_target()?)?;
        repo.write("topic.txt", "topic\n")?;
        repo.run(["add", "topic.txt"])?;
        repo.run(["commit", "-m", "topic"])?;
        let main = git.branch_target("main")?.ok_or("missing main")?;
        git.checkout_branch(&main, &git.head_target()?)?;
        repo.write("base.txt", "base\n")?;
        repo.run(["add", "base.txt"])?;
        repo.run(["commit", "-m", "base"])?;
        let topic = git.branch_target("topic")?.ok_or("missing topic")?;
        git.checkout_branch(&topic, &git.head_target()?)?;
        let main = git.branch_target("main")?.ok_or("missing main")?;
        git.rebase_onto(&main, &git.head_target()?)?;

        repo.run(["merge-base", "--is-ancestor", "main", "HEAD"])?;
        Ok(())
    }

    #[test]
    fn branch_workflows_block_dirty_tree() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        repo.write("README.md", "dirty\n")?;
        let git = Git::new(repo.path());

        let result = git.create_branch("feature", None, &git.head_target()?);

        let Err(error) = result else {
            return Err("expected dirty tree guardrail".into());
        };
        assert!(error.to_string().contains("working tree is not clean"));
        Ok(())
    }

    #[test]
    fn remote_checkout_blocks_when_local_branch_exists() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        repo.run(["update-ref", "refs/remotes/origin/main", "HEAD"])?;
        let git = Git::new(repo.path());
        let remote = git.branch_target("origin/main")?.ok_or("missing remote")?;

        let result = git.checkout_branch(&remote, &git.head_target()?);

        let Err(error) = result else {
            return Err("expected remote checkout guardrail".into());
        };
        assert!(
            error
                .to_string()
                .contains("local branch main already exists")
        );
        Ok(())
    }

    #[test]
    fn branch_workflows_block_in_progress_rebase() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        repo.run(["switch", "-c", "topic"])?;
        repo.write("topic.txt", "topic\n")?;
        repo.run(["add", "topic.txt"])?;
        repo.run(["commit", "-m", "topic"])?;
        repo.run(["switch", "main"])?;
        repo.write("base.txt", "base\n")?;
        repo.run(["add", "base.txt"])?;
        repo.run(["commit", "-m", "base"])?;
        repo.run(["switch", "topic"])?;
        repo.run_allow_failure(["rebase", "--exec", "false", "main"])?;
        let git = Git::new(repo.path());

        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));

        let result = git.create_branch("new-topic", None, &git.head_target()?);

        let Err(error) = result else {
            return Err("expected in-progress rebase guardrail".into());
        };
        assert!(
            error
                .to_string()
                .contains("rebase operation is in progress")
        );
        Ok(())
    }

    #[test]
    fn detects_conflicts_in_temp_repo() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("conflict.txt", "base\n")?;
        repo.run(["add", "conflict.txt"])?;
        repo.run(["commit", "-m", "base"])?;
        repo.run(["checkout", "-b", "other"])?;
        repo.write("conflict.txt", "other\n")?;
        repo.run(["commit", "-am", "other"])?;
        repo.run(["checkout", "main"])?;
        repo.write("conflict.txt", "main\n")?;
        repo.run(["commit", "-am", "main"])?;
        repo.run_allow_failure(["merge", "other"])?;

        let status = Git::new(repo.path()).status()?;

        assert_eq!(status.operation, Some(RepositoryOperation::Merge));
        assert_eq!(status.conflicted_files().len(), 1);
        assert_eq!(
            status.conflicted_files()[0].path,
            PathBuf::from("conflict.txt")
        );

        repo.run(["add", "conflict.txt"])?;
        let status = Git::new(repo.path()).status()?;

        assert_eq!(status.operation, Some(RepositoryOperation::Merge));
        assert!(status.conflicted_files().is_empty());
        Ok(())
    }

    #[test]
    fn merge_continue_requires_resolution_and_finishes_without_an_editor()
    -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        repo.run(["config", "core.editor", "false"])?;
        let git = Git::new(repo.path());

        let Err(error) = git.recover(RepositoryOperation::Merge, RecoveryAction::Continue) else {
            return Err("expected unresolved merge to block continue".into());
        };
        assert!(error.to_string().contains("unresolved conflicts"));

        repo.write("conflict.txt", "resolved\n")?;
        repo.run(["add", "conflict.txt"])?;
        git.recover(RepositoryOperation::Merge, RecoveryAction::Continue)?;

        assert_eq!(git.status()?.operation, None);
        repo.run(["rev-parse", "--verify", "HEAD^2"])?;
        Ok(())
    }

    #[test]
    fn merge_abort_clears_operation_and_restores_head() -> Result<(), Box<dyn Error>> {
        let (repo, original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());

        git.recover(RepositoryOperation::Merge, RecoveryAction::Abort)?;

        assert_eq!(git.status()?.operation, None);
        assert_eq!(
            repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("conflict.txt"))?,
            "main\n"
        );
        Ok(())
    }

    #[test]
    fn rebase_continue_reports_the_next_conflict_then_finishes() -> Result<(), Box<dyn Error>> {
        let repo = prepare_two_conflict_rebase()?;
        repo.run(["config", "core.editor", "false"])?;
        let git = Git::new(repo.path());

        repo.write("first.txt", "topic first\n")?;
        repo.run(["add", "first.txt"])?;
        let Err(error) = git.recover(RepositoryOperation::Rebase, RecoveryAction::Continue) else {
            return Err("expected rebase to stop at the next conflict".into());
        };
        let GitError::GitFailed {
            args,
            stdout,
            stderr,
            ..
        } = &error
        else {
            return Err(format!("expected git failure for next conflict, got {error}").into());
        };
        assert_eq!(args, &["rebase".to_owned(), "--continue".to_owned()]);
        assert!(format!("{stdout}\n{stderr}").contains("second.txt"));
        let status = git.status()?;
        assert_eq!(status.operation, Some(RepositoryOperation::Rebase));
        assert_eq!(status.conflicted_files().len(), 1);
        assert_eq!(
            status.conflicted_files()[0].path,
            PathBuf::from("second.txt")
        );

        repo.write("second.txt", "topic second\n")?;
        repo.run(["add", "second.txt"])?;
        git.recover(RepositoryOperation::Rebase, RecoveryAction::Continue)?;

        assert_eq!(git.status()?.operation, None);
        repo.run(["merge-base", "--is-ancestor", "main", "HEAD"])?;
        Ok(())
    }

    #[test]
    fn rebase_abort_clears_operation_and_restores_head() -> Result<(), Box<dyn Error>> {
        let (repo, original_head) = prepare_rebase_conflict()?;
        let git = Git::new(repo.path());

        git.recover(RepositoryOperation::Rebase, RecoveryAction::Abort)?;

        assert_eq!(git.status()?.operation, None);
        assert_eq!(
            repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("conflict.txt"))?,
            "topic\n"
        );
        Ok(())
    }

    #[test]
    fn rebase_skip_drops_the_conflicting_commit() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_rebase_conflict()?;
        let git = Git::new(repo.path());

        git.recover(RepositoryOperation::Rebase, RecoveryAction::Skip)?;

        assert_eq!(git.status()?.operation, None);
        assert_eq!(
            fs::read_to_string(repo.path().join("conflict.txt"))?,
            "main\n"
        );
        assert_eq!(
            repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            repo.git_stdout(["rev-parse", "main"])?.trim()
        );
        Ok(())
    }

    #[test]
    fn rebase_merge_step_recovery_preserves_rebase_identity() -> Result<(), Box<dyn Error>> {
        let (abort_repo, original_head) = prepare_rebase_merge_conflict()?;
        let abort_git = Git::new(abort_repo.path());

        let Err(error) = abort_git.recover(RepositoryOperation::Merge, RecoveryAction::Abort)
        else {
            return Err("expected nested merge recovery to be rejected".into());
        };
        assert!(error.to_string().contains("rebase operation is active"));
        assert!(abort_git.git_path("rebase-merge")?.exists());
        assert!(abort_git.git_path("MERGE_HEAD")?.exists());

        abort_git.recover(RepositoryOperation::Rebase, RecoveryAction::Abort)?;
        assert_eq!(abort_git.status()?.operation, None);
        assert_eq!(
            abort_repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );

        let (continue_repo, _original_head) = prepare_rebase_merge_conflict()?;
        let continue_git = Git::new(continue_repo.path());
        let Err(error) =
            continue_git.recover(RepositoryOperation::Rebase, RecoveryAction::Continue)
        else {
            return Err("expected unresolved nested merge to block continue".into());
        };
        assert!(error.to_string().contains("unresolved conflicts"));
        continue_repo.write("conflict.txt", "resolved again\n")?;
        continue_repo.run(["add", "conflict.txt"])?;
        continue_git.recover(RepositoryOperation::Rebase, RecoveryAction::Continue)?;
        assert_eq!(continue_git.status()?.operation, None);

        let (skip_repo, _original_head) = prepare_rebase_merge_conflict()?;
        let skip_git = Git::new(skip_repo.path());
        skip_git.recover(RepositoryOperation::Rebase, RecoveryAction::Skip)?;
        assert_eq!(skip_git.status()?.operation, None);
        Ok(())
    }

    #[test]
    fn recovery_rejects_merge_skip_absent_state_and_mismatched_state() -> Result<(), Box<dyn Error>>
    {
        let clean = initialized_repo()?;
        let clean_git = Git::new(clean.path());
        for (operation, action) in [
            (RepositoryOperation::Merge, RecoveryAction::Continue),
            (RepositoryOperation::Merge, RecoveryAction::Abort),
            (RepositoryOperation::Rebase, RecoveryAction::Continue),
            (RepositoryOperation::Rebase, RecoveryAction::Abort),
            (RepositoryOperation::Rebase, RecoveryAction::Skip),
        ] {
            let Err(error) = clean_git.recover(operation, action) else {
                return Err(
                    format!("expected {operation:?} {action:?} to require active state").into(),
                );
            };
            assert!(error.to_string().contains("no"));
            assert!(error.to_string().contains("operation is active"));
        }

        let (merge_repo, _original_head) = prepare_merge_conflict()?;
        let merge_git = Git::new(merge_repo.path());
        let Err(error) = merge_git.recover(RepositoryOperation::Merge, RecoveryAction::Skip) else {
            return Err("expected merge skip to be rejected".into());
        };
        assert!(error.to_string().contains("does not support"));
        assert_eq!(
            merge_git.status()?.operation,
            Some(RepositoryOperation::Merge)
        );

        let Err(error) = merge_git.recover(RepositoryOperation::Rebase, RecoveryAction::Abort)
        else {
            return Err("expected mismatched rebase recovery to be rejected".into());
        };
        assert!(error.to_string().contains("merge operation is active"));
        assert_eq!(
            merge_git.status()?.operation,
            Some(RepositoryOperation::Merge)
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_executable_hooks_without_starting_them() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let (repo, _original_head) = prepare_merge_conflict()?;
        let hooks = repo.path().join("hooks");
        fs::create_dir(&hooks)?;
        let commit_msg_hook = hooks.join("commit-msg");
        fs::write(
            &commit_msg_hook,
            "#!/bin/sh\ntouch hook-ran\nprintf '\\377'\n",
        )?;
        let mut permissions = fs::metadata(&commit_msg_hook)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&commit_msg_hook, permissions)?;
        repo.run(["config", "core.hooksPath", "hooks"])?;

        repo.write("conflict.txt", "resolved\n")?;
        repo.run(["add", "conflict.txt"])?;
        let git = Git::new(repo.path());

        let Err(error) = git.recover(RepositoryOperation::Merge, RecoveryAction::Continue) else {
            return Err("expected executable hook configuration to fail closed".into());
        };
        assert!(error.to_string().contains("executable Git hooks"));
        assert!(!repo.path().join("hook-ran").exists());
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        Ok(())
    }

    #[test]
    fn recovery_fingerprint_tracks_refs_index_config_hooks_and_progress()
    -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let baseline = git.recovery_state()?;

        repo.run(["update-ref", "refs/heads/preview-race", "HEAD"])?;
        assert_ne!(git.recovery_state()?, baseline);
        repo.run(["update-ref", "-d", "refs/heads/preview-race"])?;
        assert_eq!(git.recovery_state()?, baseline);

        repo.run(["config", "user.name", "changed after preview"])?;
        assert_ne!(git.recovery_state()?, baseline);
        repo.run(["config", "user.name", "bitbygit test"])?;
        assert_eq!(git.recovery_state()?, baseline);

        let hook = repo.path().join(".git/hooks/commit-msg");
        fs::write(&hook, "#!/bin/sh\nexit 0\n")?;
        assert_ne!(git.recovery_state()?, baseline);
        fs::remove_file(hook)?;
        assert_eq!(git.recovery_state()?, baseline);

        repo.write("staged-after-preview.txt", "unexpected\n")?;
        repo.run(["add", "staged-after-preview.txt"])?;
        assert_ne!(git.recovery_state()?, baseline);

        let (rebase_repo, _original_head) = prepare_rebase_conflict()?;
        let rebase_git = Git::new(rebase_repo.path());
        let rebase_baseline = rebase_git.recovery_state()?;
        let orig_head = rebase_git.git_path("rebase-merge/orig-head")?;
        fs::write(orig_head, format!("{ZERO_OID}\n"))?;
        assert_ne!(rebase_git.recovery_state()?, rebase_baseline);
        Ok(())
    }

    #[test]
    fn recovery_fingerprint_tracks_autostash_and_rerere_state() -> Result<(), Box<dyn Error>> {
        let (repo, original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let baseline = git.recovery_state()?;

        for relative in ["MERGE_AUTOSTASH", "MERGE_RR"] {
            let path = git.git_path(relative)?;
            fs::write(&path, format!("{original_head}\n"))?;
            assert_ne!(git.recovery_state()?, baseline, "{relative}");
            fs::remove_file(path)?;
            assert_eq!(git.recovery_state()?, baseline, "{relative}");
        }

        let rr_cache = git.git_path("rr-cache")?;
        let rr_cache_existed = rr_cache.exists();
        fs::create_dir_all(&rr_cache)?;
        fs::write(rr_cache.join("review-regression"), "changed rerere state\n")?;
        assert_ne!(git.recovery_state()?, baseline, "rr-cache");
        fs::remove_file(rr_cache.join("review-regression"))?;
        if !rr_cache_existed {
            fs::remove_dir(rr_cache)?;
        }
        assert_eq!(git.recovery_state()?, baseline, "rr-cache");
        Ok(())
    }

    #[test]
    fn recovery_fingerprint_tracks_attributes_and_sparse_checkout_inputs()
    -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let external = TempRepo::new()?;
        let external_attributes = external.path().join("attributes");
        fs::write(&external_attributes, "*.txt text\n")?;
        repo.run_args(&[
            "config",
            "core.attributesFile",
            &external_attributes.to_string_lossy(),
        ])?;
        repo.write(".gitattributes", "*.txt text\n")?;
        let git = Git::new(repo.path());
        let info_attributes = git.git_path("info/attributes")?;
        let sparse_checkout = git.git_path("info/sparse-checkout")?;
        fs::write(&info_attributes, "*.md text\n")?;
        fs::write(&sparse_checkout, "/*\n")?;
        let baseline = git.recovery_state()?;

        for (path, changed, original) in [
            (
                repo.path().join(".gitattributes"),
                "*.txt binary\n",
                "*.txt text\n",
            ),
            (info_attributes.clone(), "*.md binary\n", "*.md text\n"),
            (sparse_checkout.clone(), "/src/\n", "/*\n"),
            (
                external_attributes.clone(),
                "*.txt binary\n",
                "*.txt text\n",
            ),
        ] {
            fs::write(&path, changed)?;
            assert_ne!(git.recovery_state()?, baseline, "{}", path.display());
            fs::write(path, original)?;
            assert_eq!(git.recovery_state()?, baseline);
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_fingerprint_tracks_symlinked_hook_target_contents() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let (repo, _original_head) = prepare_merge_conflict()?;
        let external = TempRepo::new()?;
        let target = external.path().join("commit-msg-target");
        fs::write(&target, "#!/bin/sh\nexit 0\n")?;
        symlink(&target, repo.path().join(".git/hooks/commit-msg"))?;
        let git = Git::new(repo.path());
        let baseline = git.recovery_state()?;

        fs::write(&target, "#!/bin/sh\nexit 1\n")?;

        assert_ne!(git.recovery_state()?, baseline);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_fingerprint_rejects_synchronized_symlink_retarget() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let repo = TempRepo::new()?;
        let first = repo.path().join("first-hook");
        let second = repo.path().join("second-hook");
        let hook = repo.path().join("commit-msg");
        let replacement = repo.path().join("replacement-link");
        fs::write(&first, "same hook contents\n")?;
        fs::write(&second, "same hook contents\n")?;
        symlink(&first, &hook)?;
        symlink(&second, &replacement)?;
        let mut hasher = Sha256::new();
        let mut entries = 0;
        let mut replaced = false;

        let Err(error) = hash_recovery_path_with(
            &mut hasher,
            b"hook",
            &hook,
            Instant::now() + Duration::from_secs(2),
            &mut entries,
            &mut |path, kind| {
                if path == hook && kind == RecoveryPathKind::Symlink {
                    fs::rename(&replacement, &hook)
                        .map_err(|source| recovery_state_io(path, source))?;
                    replaced = true;
                }
                Ok(())
            },
        ) else {
            return Err("expected a retargeted symlink to be rejected".into());
        };

        assert!(replaced);
        assert!(
            error
                .to_string()
                .contains("changed while it was fingerprinted")
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_fingerprint_rejects_synchronized_directory_replacement()
    -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        let hooks = repo.path().join("hooks");
        let original = repo.path().join("original-hooks");
        let replacement = repo.path().join("replacement-hooks");
        fs::create_dir(&hooks)?;
        fs::write(hooks.join("commit-msg"), "same hook contents\n")?;
        fs::create_dir(&replacement)?;
        fs::write(replacement.join("commit-msg"), "same hook contents\n")?;
        let mut hasher = Sha256::new();
        let mut entries = 0;
        let mut replaced = false;

        let Err(error) = hash_recovery_path_with(
            &mut hasher,
            b"hooks",
            &hooks,
            Instant::now() + Duration::from_secs(2),
            &mut entries,
            &mut |path, kind| {
                if path == hooks && kind == RecoveryPathKind::Directory {
                    fs::rename(&hooks, &original)
                        .map_err(|source| recovery_state_io(path, source))?;
                    fs::rename(&replacement, &hooks)
                        .map_err(|source| recovery_state_io(path, source))?;
                    replaced = true;
                }
                Ok(())
            },
        ) else {
            return Err("expected a replaced directory to be rejected".into());
        };

        assert!(replaced);
        assert!(
            error
                .to_string()
                .contains("changed while it was fingerprinted")
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_fingerprint_rejects_fifo_without_blocking() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        let fifo = repo.path().join("attributes.fifo");
        let status = Command::new("mkfifo").arg(&fifo).status()?;
        assert!(status.success());
        let mut hasher = Sha256::new();
        let mut entries = 0;
        let started = Instant::now();

        let Err(error) = hash_recovery_path(
            &mut hasher,
            b"fifo",
            &fifo,
            Instant::now() + Duration::from_millis(200),
            &mut entries,
        ) else {
            return Err("expected FIFO recovery input to be rejected".into());
        };

        assert!(
            error
                .to_string()
                .contains("not a regular file or directory")
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        Ok(())
    }

    #[test]
    fn recovery_output_drainer_retains_only_hard_bounded_memory() -> Result<(), Box<dyn Error>> {
        let exceeded = Arc::new(AtomicBool::new(false));
        let capture = drain_bounded_output(
            io::Cursor::new(vec![b'x'; 8192]),
            1024,
            4096,
            true,
            Arc::clone(&exceeded),
        )?;

        assert_eq!(capture.bytes.len(), 1024);
        assert!(capture.truncated);
        assert!(exceeded.load(Ordering::Acquire));
        Ok(())
    }

    #[test]
    fn recovery_reports_clear_minimum_git_version() -> Result<(), Box<dyn Error>> {
        let Err(error) = ensure_recovery_git_version(b"git version 2.41.3") else {
            return Err("expected old Git to be rejected".into());
        };

        assert!(error.to_string().contains("requires Git 2.42 or newer"));
        ensure_recovery_git_version(b"git version 2.42.0")?;
        ensure_recovery_git_version(b"git version 2.55.0.windows.1")?;
        Ok(())
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn recovery_platform_end_to_end_deadline_does_not_interrupt_merge_abort()
    -> Result<(), Box<dyn Error>> {
        let repo = initialized_repo()?;
        repo.run(["config", "core.autocrlf", "false"])?;
        repo.write("conflict.txt", "base\n")?;
        repo.run(["add", "conflict.txt"])?;
        repo.run(["commit", "-m", "base"])?;
        repo.run(["switch", "-c", "other"])?;
        repo.write("conflict.txt", "other\n")?;
        for index in 0..2_000 {
            repo.write(&format!("generated-{index:04}.txt"), "other\n")?;
        }
        repo.run(["add", "."])?;
        repo.run(["commit", "-m", "other"])?;
        repo.run(["switch", "main"])?;
        repo.write("conflict.txt", "main\n")?;
        repo.run(["commit", "-am", "main"])?;
        let original_head = repo.git_stdout(["rev-parse", "HEAD"])?.trim().to_owned();
        repo.run_allow_failure(["merge", "other"])?;
        let git = Git::new(repo.path());
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));

        let mut command = Command::new("git");
        command
            .current_dir(repo.path())
            .args([
                "-c",
                "maintenance.auto=false",
                "-c",
                "gc.auto=0",
                "merge",
                "--abort",
            ])
            .env("GIT_TERMINAL_PROMPT", "0");
        configure_process_group(&mut command);
        let start_timeout = Duration::from_millis(10);
        let started = Instant::now();

        let output = run_bounded_command(
            command,
            vec!["merge".to_owned(), "--abort".to_owned()],
            started + start_timeout,
            4096,
            BoundedCommandPolicy::RecoveryExecution,
            || Ok(()),
        )?;

        assert!(output.status.success());
        assert!(started.elapsed() >= start_timeout);
        assert_eq!(git.status()?.operation, None);
        assert_eq!(
            repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("conflict.txt"))?,
            "main\n"
        );
        for index in 0..2_000 {
            assert!(
                !repo
                    .path()
                    .join(format!("generated-{index:04}.txt"))
                    .exists()
            );
        }
        assert!(!git.git_path("index.lock")?.exists());
        assert!(repo.git_stdout(["status", "--porcelain"])?.is_empty());
        Ok(())
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn recovery_platform_command_output_is_bounded() -> Result<(), Box<dyn Error>> {
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "while :; do printf '0123456789'; done"]);
            command
        };
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("powershell");
            command.args([
                "-NoProfile",
                "-Command",
                "[Console]::Out.Write('x' * 70000000)",
            ]);
            command
        };
        configure_process_group(&mut command);

        let result = run_bounded_command(
            command,
            vec!["recovery-output-bound-test".to_owned()],
            Instant::now() + Duration::from_secs(5),
            1024,
            BoundedCommandPolicy::Diagnostic,
            || Ok(()),
        );

        assert!(
            matches!(result, Err(GitError::TimedOut { .. }))
                || result
                    .as_ref()
                    .is_err_and(|error| error.to_string().contains("bounded capture limit")),
            "unexpected result: {result:?}"
        );
        Ok(())
    }

    #[test]
    fn exact_recovery_rejects_changed_merge_control_input() -> Result<(), Box<dyn Error>> {
        let (repo, original_head) = prepare_merge_conflict()?;
        repo.write("conflict.txt", "resolved\n")?;
        repo.run(["add", "conflict.txt"])?;
        let git = Git::new(repo.path());
        let expected =
            git.prepare_recovery(RepositoryOperation::Merge, RecoveryAction::Continue)?;
        fs::write(git.git_path("MERGE_MSG")?, "substituted message\n")?;

        let Err(error) = git.recover_exact(
            RepositoryOperation::Merge,
            RecoveryAction::Continue,
            &expected,
        ) else {
            return Err("expected changed merge metadata to block recovery".into());
        };

        assert!(error.to_string().contains("state changed after preview"));
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        assert_eq!(
            repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );
        Ok(())
    }

    #[test]
    fn exact_recovery_rejects_changed_merge_autostash() -> Result<(), Box<dyn Error>> {
        let (repo, original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let autostash = git.git_path("MERGE_AUTOSTASH")?;
        fs::write(&autostash, format!("{original_head}\n"))?;
        let expected = git.prepare_recovery(RepositoryOperation::Merge, RecoveryAction::Abort)?;
        let merge_head = fs::read_to_string(git.git_path("MERGE_HEAD")?)?;
        fs::write(&autostash, merge_head)?;

        let Err(error) =
            git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)
        else {
            return Err("expected changed merge autostash to block recovery".into());
        };

        assert!(error.to_string().contains("state changed after preview"));
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        assert_eq!(
            repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );
        Ok(())
    }

    #[test]
    fn rebase_abort_blocks_ignored_collision_path() -> Result<(), Box<dyn Error>> {
        let repo = initialized_repo()?;
        repo.write(".gitignore", "target/\n")?;
        repo.run(["add", ".gitignore"])?;
        repo.run(["commit", "-m", "ignore target"])?;
        repo.run(["switch", "-c", "topic"])?;
        repo.write("first.txt", "first\n")?;
        repo.run(["add", "first.txt"])?;
        repo.run(["commit", "-m", "first"])?;
        repo.write(".gitignore", "")?;
        fs::create_dir(repo.path().join("target"))?;
        repo.write("target/victim.bin", "committed\n")?;
        repo.run(["add", ".gitignore", "target/victim.bin"])?;
        repo.run(["commit", "-m", "track victim"])?;
        repo.run(["switch", "main"])?;
        repo.write("upstream.txt", "upstream\n")?;
        repo.run(["add", "upstream.txt"])?;
        repo.run(["commit", "-m", "upstream"])?;
        repo.run(["switch", "topic"])?;
        repo.run_allow_failure(["rebase", "--exec", "false", "main"])?;
        let todo = Git::new(repo.path()).git_path("rebase-merge/git-rebase-todo")?;
        let remaining = fs::read_to_string(&todo)?
            .lines()
            .filter(|line| {
                let line = line.trim_start();
                !line.starts_with("exec ") && !line.starts_with("x ")
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(todo, remaining)?;
        fs::create_dir(repo.path().join("target"))?;
        repo.write("target/victim.bin", "at preview\n")?;
        let git = Git::new(repo.path());
        let Err(error) = git.prepare_recovery(RepositoryOperation::Rebase, RecoveryAction::Abort)
        else {
            return Err("expected ignored collision to block abort planning".into());
        };

        assert!(error.to_string().contains("would be overwritten"));
        assert_eq!(
            fs::read_to_string(repo.path().join("target/victim.bin"))?,
            "at preview\n"
        );
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        Ok(())
    }

    #[test]
    fn merge_abort_blocks_ignored_collision_omitted_from_endpoint_diff()
    -> Result<(), Box<dyn Error>> {
        let repo = initialized_repo()?;
        repo.write("conflict.txt", "base\n")?;
        repo.write("stable.txt", "tracked stable contents\n")?;
        repo.run(["add", "conflict.txt", "stable.txt"])?;
        repo.run(["commit", "-m", "base"])?;
        repo.run(["switch", "-c", "other"])?;
        repo.write("conflict.txt", "other\n")?;
        repo.run(["commit", "-am", "other"])?;
        repo.run(["switch", "main"])?;
        repo.write("conflict.txt", "main\n")?;
        repo.run(["commit", "-am", "main"])?;
        repo.run_allow_failure(["merge", "other"])?;
        repo.run(["rm", "--cached", "stable.txt"])?;
        repo.write(".gitignore", "stable.txt\n")?;
        repo.write("stable.txt", "local ignored data\n")?;
        let git = Git::new(repo.path());

        let Err(error) = git.prepare_recovery(RepositoryOperation::Merge, RecoveryAction::Abort)
        else {
            return Err("expected ignored stable path to block merge abort".into());
        };

        assert!(error.to_string().contains("would be overwritten"));
        assert_eq!(
            fs::read_to_string(repo.path().join("stable.txt"))?,
            "local ignored data\n"
        );
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        Ok(())
    }

    #[test]
    fn rebase_continue_blocks_ignored_path_touched_only_by_intermediate_commits()
    -> Result<(), Box<dyn Error>> {
        let repo = initialized_repo()?;
        repo.write(".gitignore", "victim.txt\n")?;
        repo.write("conflict.txt", "base\n")?;
        repo.run(["add", ".gitignore", "conflict.txt"])?;
        repo.run(["commit", "-m", "rebase base"])?;
        repo.run(["switch", "-c", "topic"])?;
        repo.write("conflict.txt", "topic\n")?;
        repo.run(["commit", "-am", "conflicting change"])?;
        repo.write("victim.txt", "committed intermediate contents\n")?;
        repo.run(["add", "-f", "victim.txt"])?;
        repo.run(["commit", "-m", "add ignored victim"])?;
        fs::remove_file(repo.path().join("victim.txt"))?;
        repo.run(["add", "-u"])?;
        repo.run(["commit", "-m", "remove ignored victim"])?;
        repo.run(["switch", "main"])?;
        repo.write("conflict.txt", "main\n")?;
        repo.write("upstream.txt", "upstream\n")?;
        repo.run(["add", "conflict.txt", "upstream.txt"])?;
        repo.run(["commit", "-m", "upstream change"])?;
        repo.run(["switch", "topic"])?;
        repo.run_allow_failure(["rebase", "main"])?;
        repo.write("conflict.txt", "resolved\n")?;
        repo.run(["add", "conflict.txt"])?;
        repo.write("victim.txt", "local ignored data\n")?;
        let git = Git::new(repo.path());

        let Err(error) =
            git.prepare_recovery(RepositoryOperation::Rebase, RecoveryAction::Continue)
        else {
            return Err("expected an intermediate rebase collision to block continue".into());
        };

        assert!(error.to_string().contains("would be overwritten"));
        assert_eq!(
            fs::read_to_string(repo.path().join("victim.txt"))?,
            "local ignored data\n"
        );
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        Ok(())
    }

    #[test]
    fn exact_recovery_rejects_changed_untracked_contents() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        repo.write("untracked.txt", "at preview\n")?;
        let git = Git::new(repo.path());
        let expected = git.prepare_recovery(RepositoryOperation::Merge, RecoveryAction::Abort)?;
        repo.write("untracked.txt", "changed after preview\n")?;

        let Err(error) =
            git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)
        else {
            return Err("expected changed untracked content to block abort".into());
        };

        assert!(error.to_string().contains("state changed after preview"));
        assert_eq!(
            fs::read_to_string(repo.path().join("untracked.txt"))?,
            "changed after preview\n"
        );
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        Ok(())
    }

    #[test]
    fn recovery_fingerprint_bounds_untracked_data() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let untracked = fs::File::create(repo.path().join("large.untracked"))?;
        untracked.set_len(RECOVERY_UNTRACKED_DATA_LIMIT + 1)?;

        let Err(error) = Git::new(repo.path()).recovery_state() else {
            return Err("expected oversized untracked data to block planning".into());
        };

        assert!(
            error
                .to_string()
                .contains("untracked worktree data exceeds")
        );
        Ok(())
    }

    #[test]
    fn exact_merge_abort_allows_large_unrelated_ignored_build_tree() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        repo.write(".gitignore", "target/\n")?;
        fs::create_dir(repo.path().join("target"))?;
        let artifact = fs::File::create(repo.path().join("target/large-artifact.bin"))?;
        artifact.set_len(RECOVERY_UNTRACKED_DATA_LIMIT * 2)?;
        for index in 0..4_000 {
            repo.write(&format!("target/generated-artifact-{index:04}.bin"), "")?;
        }
        let git = Git::new(repo.path());
        let expected = git.prepare_recovery(RepositoryOperation::Merge, RecoveryAction::Abort)?;

        git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)?;

        assert_eq!(git.status()?.operation, None);
        assert_eq!(
            artifact.metadata()?.len(),
            RECOVERY_UNTRACKED_DATA_LIMIT * 2
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_preview_rechecks_process_safety_inside_state_fence() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let (repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let hook = repo.path().join(".git/hooks/commit-msg");

        let Err(error) =
            git.recovery_state_fenced_until(Instant::now() + Duration::from_secs(10), || {
                fs::write(&hook, "#!/bin/sh\nexit 0\n").map_err(|source| GitError::Io {
                    args: vec!["install synchronized hook".to_owned()],
                    source,
                })?;
                let mut permissions = fs::metadata(&hook)
                    .map_err(|source| GitError::Io {
                        args: vec!["inspect synchronized hook".to_owned()],
                        source,
                    })?
                    .permissions();
                permissions.set_mode(0o755);
                fs::set_permissions(&hook, permissions).map_err(|source| GitError::Io {
                    args: vec!["enable synchronized hook".to_owned()],
                    source,
                })
            })
        else {
            return Err("expected a hook enabled during fingerprinting to be rejected".into());
        };

        assert!(
            error
                .to_string()
                .contains("changed while it was fingerprinted")
        );
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_fingerprint_rejects_swapped_executable_hook_observation()
    -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let (repo, _original_head) = prepare_merge_conflict()?;
        let active = repo.path().join("active-hooks");
        let safe = repo.path().join("safe-hooks");
        let unsafe_hooks = repo.path().join("unsafe-hooks");
        fs::create_dir(&active)?;
        fs::create_dir(&unsafe_hooks)?;
        let marker = repo.path().join("swapped-hook-ran");
        let hook = unsafe_hooks.join("commit-msg");
        fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display()))?;
        let mut permissions = fs::metadata(&hook)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions)?;
        repo.run(["config", "core.hooksPath", "active-hooks"])?;
        let git = Git::new(repo.path());
        let deadline = Instant::now() + Duration::from_secs(10);
        git.ensure_recovery_process_configuration_safe_until(deadline)?;

        let Err(error) = git.recovery_state_fenced_until(deadline, || {
            fs::rename(&active, &safe).map_err(|source| recovery_state_io(&active, source))?;
            fs::rename(&unsafe_hooks, &active)
                .map_err(|source| recovery_state_io(&unsafe_hooks, source))
        }) else {
            return Err("expected swapped executable hook directory to fail closed".into());
        };

        assert!(
            error
                .to_string()
                .contains("changed while it was fingerprinted")
        );
        assert!(!marker.exists(), "swapped hook process started");
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        Ok(())
    }

    #[test]
    fn exact_recovery_rechecks_index_after_first_final_fence_pass() -> Result<(), Box<dyn Error>> {
        let (repo, original_head) = prepare_merge_conflict()?;
        repo.write("conflict.txt", "resolved\n")?;
        repo.run(["add", "conflict.txt"])?;
        let git = Git::new(repo.path());
        let expected =
            git.prepare_recovery(RepositoryOperation::Merge, RecoveryAction::Continue)?;

        let Err(error) = git.recover_exact_with(
            RepositoryOperation::Merge,
            RecoveryAction::Continue,
            &expected,
            || {
                fs::write(repo.path().join("late.txt"), "late staged content\n").map_err(
                    |source| GitError::Io {
                        args: vec!["write synchronized mutation".to_owned()],
                        source,
                    },
                )?;
                let output = Command::new("git")
                    .current_dir(repo.path())
                    .args(["add", "late.txt"])
                    .output()
                    .map_err(|source| GitError::Io {
                        args: vec!["add synchronized mutation".to_owned()],
                        source,
                    })?;
                if !output.status.success() {
                    return Err(GitError::Blocked {
                        message: "synchronized mutation failed".to_owned(),
                    });
                }
                Ok(())
            },
        ) else {
            return Err("expected final fingerprint fence to block recovery".into());
        };

        assert!(error.to_string().contains("state changed after preview"));
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        assert_eq!(
            repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );
        Ok(())
    }

    #[test]
    fn exact_recovery_rechecks_process_config_after_first_final_fence_pass()
    -> Result<(), Box<dyn Error>> {
        let (repo, original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.prepare_recovery(RepositoryOperation::Merge, RecoveryAction::Abort)?;

        let Err(error) = git.recover_exact_with(
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            &expected,
            || {
                let output = Command::new("git")
                    .current_dir(repo.path())
                    .args(["config", "core.fsmonitor", "true"])
                    .output()
                    .map_err(|source| GitError::Io {
                        args: vec!["install synchronized fsmonitor config".to_owned()],
                        source,
                    })?;
                if !output.status.success() {
                    return Err(GitError::Blocked {
                        message: "synchronized config mutation failed".to_owned(),
                    });
                }
                Ok(())
            },
        ) else {
            return Err("expected final process-config fence to block recovery".into());
        };

        assert!(error.to_string().contains("core.fsmonitor"));
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        assert_eq!(
            repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );
        Ok(())
    }

    #[test]
    fn exact_recovery_rejects_filter_activated_at_spawn_boundary() -> Result<(), Box<dyn Error>> {
        let (repo, original_head) = prepare_merge_conflict()?;
        repo.write(".gitattributes", "*.txt filter=unsafe\n")?;
        let git = Git::new(repo.path());
        let expected = git.prepare_recovery(RepositoryOperation::Merge, RecoveryAction::Abort)?;

        let Err(error) = git.recover_exact_with(
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            &expected,
            || {
                let output = Command::new("git")
                    .current_dir(repo.path())
                    .args(["config", "filter.unsafe.smudge", "cat"])
                    .output()
                    .map_err(|source| GitError::Io {
                        args: vec!["install synchronized filter config".to_owned()],
                        source,
                    })?;
                if !output.status.success() {
                    return Err(GitError::Blocked {
                        message: "synchronized filter mutation failed".to_owned(),
                    });
                }
                Ok(())
            },
        ) else {
            return Err("expected activated filter at spawn boundary to block recovery".into());
        };

        assert!(error.to_string().contains("filter.unsafe.smudge"));
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        assert_eq!(
            repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );
        Ok(())
    }

    #[test]
    fn exact_recovery_rechecks_process_control_after_first_final_fence_pass()
    -> Result<(), Box<dyn Error>> {
        for (relative, contents) in [
            ("rebase-merge/git-rebase-todo", "exec false\n"),
            ("rebase-merge/strategy", "resolve\n"),
        ] {
            let (repo, original_head) = prepare_rebase_conflict()?;
            repo.write("conflict.txt", "resolved\n")?;
            repo.run(["add", "conflict.txt"])?;
            let git = Git::new(repo.path());
            let expected =
                git.prepare_recovery(RepositoryOperation::Rebase, RecoveryAction::Continue)?;
            let control = git.git_path(relative)?;

            let Err(error) = git.recover_exact_with(
                RepositoryOperation::Rebase,
                RecoveryAction::Continue,
                &expected,
                || {
                    fs::write(&control, contents)
                        .map_err(|source| recovery_state_io(&control, source))
                },
            ) else {
                return Err(format!("expected synchronized {relative} mutation to block").into());
            };

            assert!(error.to_string().contains("state changed after preview"));
            assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
            assert_eq!(
                repo.git_stdout(["rev-parse", "rebase-merge/orig-head"])?
                    .trim(),
                original_head
            );
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn recovery_platform_rejects_same_metadata_file_replacement() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        let input = repo.path().join("index");
        let retained = repo.path().join("index-retained");
        fs::write(&input, b"original")?;
        let original_metadata = fs::metadata(&input)?;
        let modified = original_metadata.modified()?;
        let mut hasher = Sha256::new();
        let mut entries = 0;
        let mut replaced = false;

        let Err(error) = hash_recovery_path_with(
            &mut hasher,
            b"index",
            &input,
            Instant::now() + Duration::from_secs(2),
            &mut entries,
            &mut |path, kind| {
                if path == input && kind == RecoveryPathKind::File {
                    fs::rename(&input, &retained)
                        .map_err(|source| recovery_state_io(path, source))?;
                    fs::write(&input, b"replaced")
                        .map_err(|source| recovery_state_io(path, source))?;
                    fs::OpenOptions::new()
                        .write(true)
                        .open(&input)
                        .and_then(|file| {
                            file.set_times(fs::FileTimes::new().set_modified(modified))
                        })
                        .map_err(|source| recovery_state_io(path, source))?;
                    replaced = true;
                }
                Ok(())
            },
        ) else {
            return Err("expected same-metadata replacement to be rejected".into());
        };

        assert!(replaced, "unexpected error: {error}");
        assert!(same_recovery_file(
            &original_metadata,
            &fs::metadata(&input)?
        ));
        assert!(
            error
                .to_string()
                .contains("changed while it was fingerprinted")
        );
        Ok(())
    }

    #[test]
    fn recovery_fingerprint_streams_large_binary_diffs_into_fixed_state()
    -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        fs::write(
            repo.path().join("conflict.txt"),
            vec![0xa5; 8 * 1024 * 1024],
        )?;
        let git = Git::with_recovery_limits(
            repo.path(),
            Duration::from_secs(10),
            Duration::from_secs(5),
            4096,
        );

        let state = git.recovery_state()?;

        assert_eq!(std::mem::size_of_val(&state), 32);
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_planning_rejects_fsmonitor_before_it_runs() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let (repo, _original_head) = prepare_merge_conflict()?;
        let hook = repo.path().join("fsmonitor");
        fs::write(&hook, "#!/bin/sh\nsleep 30\n")?;
        let mut permissions = fs::metadata(&hook)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions)?;
        repo.run_args(&["config", "core.fsmonitor", &hook.to_string_lossy()])?;
        let git = Git::with_recovery_limits(
            repo.path(),
            Duration::from_millis(200),
            Duration::from_secs(5),
            4096,
        );
        let started = Instant::now();

        let marker = repo.path().join("fsmonitor-ran");
        fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display()))?;
        let Err(error) = git.recovery_state() else {
            return Err("expected fsmonitor recovery planning to fail closed".into());
        };

        assert!(error.to_string().contains("core.fsmonitor"));
        assert!(!marker.exists());
        assert!(started.elapsed() < Duration::from_secs(2));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_rejects_fifo_rebase_todo_without_blocking() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_rebase_conflict()?;
        let todo = Git::new(repo.path()).git_path("rebase-merge/git-rebase-todo")?;
        fs::remove_file(&todo)?;
        assert!(Command::new("mkfifo").arg(&todo).status()?.success());
        let git = Git::with_recovery_limits(
            repo.path(),
            Duration::from_millis(200),
            Duration::from_secs(5),
            4096,
        );
        let started = Instant::now();

        let Err(error) = git.recovery_state() else {
            return Err("expected FIFO rebase todo to fail closed".into());
        };

        assert!(
            error.to_string().contains("not a regular file"),
            "unexpected error: {error}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_rejects_fifo_attributes_without_blocking() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let attributes = Git::new(repo.path()).git_path("info/attributes")?;
        assert!(Command::new("mkfifo").arg(&attributes).status()?.success());
        let git = Git::with_recovery_limits(
            repo.path(),
            Duration::from_millis(200),
            Duration::from_secs(5),
            4096,
        );
        let started = Instant::now();

        let Err(error) = git.recovery_state() else {
            return Err("expected FIFO attributes to fail closed".into());
        };

        let error = error.to_string();
        assert!(
            error.contains("not a regular file") || error.contains("timed out"),
            "unexpected error: {error}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        Ok(())
    }

    #[test]
    fn recovery_rejects_all_configured_external_driver_kinds() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        repo.write(".gitattributes", "*.txt filter=unsafe\n")?;
        let git = Git::new(repo.path());
        for key in [
            "filter.unsafe.clean",
            "filter.unsafe.smudge",
            "filter.unsafe.process",
            "diff.external",
            "diff.unsafe.command",
            "diff.unsafe.textconv",
            "merge.unsafe.driver",
        ] {
            repo.run(["config", key, "cat"])?;
            let Err(error) = git.recovery_state() else {
                return Err(format!("expected {key} to fail closed").into());
            };
            assert!(error.to_string().contains("external Git drivers"));
            assert!(error.to_string().contains(key));
            repo.run(["config", "--unset", key])?;
        }
        Ok(())
    }

    #[test]
    fn recovery_rejects_external_driver_from_global_scope() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let config_dir = TempRepo::new()?;
        let global_config = config_dir.path().join("global-config");
        fs::write(&global_config, "[diff]\n\texternal = global-diff-command\n")?;
        let git = Git::new(repo.path()).with_test_global_config(global_config);

        let Err(error) = git.recovery_state() else {
            return Err("expected a global external diff driver to fail closed".into());
        };

        assert!(error.to_string().contains("diff.external"));
        assert!(error.to_string().contains("every Git config scope"));
        Ok(())
    }

    #[test]
    fn recovery_allows_inactive_global_lfs_configuration() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let config_dir = TempRepo::new()?;
        let global_config = config_dir.path().join("global-config");
        fs::write(
            &global_config,
            "[filter \"lfs\"]\n\tclean = git-lfs clean -- %f\n\tsmudge = git-lfs smudge -- %f\n\tprocess = git-lfs filter-process\n\trequired = true\n",
        )?;
        let git = Git::new(repo.path()).with_test_global_config(global_config);

        let expected = git.prepare_recovery(RepositoryOperation::Merge, RecoveryAction::Abort)?;
        git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)?;

        assert_eq!(git.status()?.operation, None);
        Ok(())
    }

    #[test]
    fn rebase_abort_rejects_filter_selected_only_by_target_tree() -> Result<(), Box<dyn Error>> {
        let repo = initialized_repo()?;
        repo.write(".gitattributes", "*.txt text\n")?;
        repo.write("conflict.txt", "base\n")?;
        repo.run(["add", ".gitattributes", "conflict.txt"])?;
        repo.run(["commit", "-m", "base"])?;
        repo.run(["switch", "-c", "topic"])?;
        repo.write(".gitattributes", "*.txt filter=unsafe\n")?;
        repo.write("conflict.txt", "topic\n")?;
        repo.run(["commit", "-am", "topic"])?;
        let target = repo.git_stdout(["rev-parse", "HEAD"])?.trim().to_owned();
        repo.run(["switch", "main"])?;
        repo.write("conflict.txt", "main\n")?;
        repo.run(["commit", "-am", "main"])?;
        repo.run(["switch", "topic"])?;
        repo.run_allow_failure(["rebase", "main"])?;
        repo.write(".gitattributes", "*.txt text\n")?;
        assert_eq!(
            repo.git_stdout_args(&["show", &format!("{target}:.gitattributes")])?,
            "*.txt filter=unsafe\n"
        );

        let marker = repo.path().join("filter-ran");
        repo.run_args(&[
            "config",
            "filter.unsafe.smudge",
            &format!("touch '{}'", marker.display()),
        ])?;
        let git = Git::new(repo.path());
        let Err(error) = git.prepare_recovery(RepositoryOperation::Rebase, RecoveryAction::Abort)
        else {
            return Err("expected target-tree filter configuration to fail closed".into());
        };

        assert!(error.to_string().contains("recovery target tree"));
        assert!(!marker.exists());
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_rejects_signing_configuration() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());

        for key in ["commit.gpgsign", "tag.gpgsign"] {
            repo.run_args(&["config", key, "true"])?;
            let Err(error) = git.recovery_state() else {
                return Err(format!("expected {key} to fail closed").into());
            };
            assert!(error.to_string().contains(key));
            repo.run_args(&["config", "--unset", key])?;
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_rejects_active_rebase_gpg_sign_before_starting_signer() -> Result<(), Box<dyn Error>>
    {
        use std::os::unix::fs::PermissionsExt;

        let repo = initialized_repo()?;
        repo.write("conflict.txt", "base\n")?;
        repo.run(["add", "conflict.txt"])?;
        repo.run(["commit", "-m", "base"])?;
        repo.run(["switch", "-c", "topic"])?;
        repo.write("conflict.txt", "topic\n")?;
        repo.run(["commit", "-am", "topic"])?;
        repo.run(["switch", "main"])?;
        repo.write("conflict.txt", "main\n")?;
        repo.run(["commit", "-am", "main"])?;
        repo.run(["switch", "topic"])?;
        let marker = repo.path().join("signer-ran");
        let signer = repo.path().join("fake-signer");
        fs::write(
            &signer,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )?;
        let mut permissions = fs::metadata(&signer)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&signer, permissions)?;
        repo.run_args(&["config", "gpg.program", &signer.to_string_lossy()])?;
        repo.run_allow_failure(["rebase", "--gpg-sign", "main"])?;
        let git = Git::new(repo.path());
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        repo.write("conflict.txt", "resolved\n")?;
        repo.run(["add", "conflict.txt"])?;

        let Err(error) = git.recover(RepositoryOperation::Rebase, RecoveryAction::Continue) else {
            return Err("expected active signed rebase to fail closed".into());
        };

        assert!(
            error
                .to_string()
                .contains("active rebase requests commit signing")
        );
        assert!(!marker.exists());
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_git_invocation_disables_automatic_maintenance() -> Result<(), Box<dyn Error>> {
        let repo = initialized_repo()?;
        let git = Git::new(repo.path());

        for (key, expected) in [
            ("maintenance.auto", &b"false\n"[..]),
            ("gc.auto", &b"0\n"[..]),
        ] {
            let args = vec!["config".to_owned(), "--get".to_owned(), key.to_owned()];
            let output =
                git.run_bounded_git(args.clone(), Instant::now() + Duration::from_secs(2), false)?;
            assert!(output.status.success(), "{key}");
            assert_eq!(output.stdout.bytes, expected, "{key}");
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_platform_execution_cleans_descendants_after_parent_exit()
    -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        let marker = repo.path().join("descendant-ran");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("(sleep 1; touch \"$1\") &")
            .arg("sh")
            .arg(&marker);
        configure_process_group(&mut command);

        let output = run_bounded_command(
            command,
            vec!["descendant-cleanup-test".to_owned()],
            Instant::now() + Duration::from_secs(5),
            4096,
            BoundedCommandPolicy::RecoveryExecution,
            || Ok(()),
        )?;
        assert!(output.status.success());
        thread::sleep(Duration::from_millis(1100));
        assert!(!marker.exists());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn recovery_platform_execution_cleans_windows_job_descendants_after_parent_exit()
    -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        let marker = repo.path().join("windows-descendant-ran");
        let script = format!(
            "Start-Process powershell -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 1; Set-Content -Path ''{}'' -Value ran' | Out-Null",
            marker.display()
        );
        let mut command = Command::new("powershell");
        command.args(["-NoProfile", "-Command", &script]);

        let output = run_bounded_command(
            command,
            vec!["windows-job-cleanup-test".to_owned()],
            Instant::now() + Duration::from_secs(5),
            4096,
            BoundedCommandPolicy::RecoveryExecution,
            || Ok(()),
        )?;
        assert!(output.status.success());
        thread::sleep(Duration::from_millis(1200));
        assert!(!marker.exists());
        Ok(())
    }

    #[test]
    fn recovery_rejects_remaining_rebase_exec_command() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_rebase_conflict()?;
        let todo = Git::new(repo.path()).git_path("rebase-merge/git-rebase-todo")?;
        fs::write(&todo, "exec touch must-not-run\n")?;

        let Err(error) = Git::new(repo.path()).recovery_state() else {
            return Err("expected rebase exec command to fail closed".into());
        };

        assert!(error.to_string().contains("rebase plan contains an exec"));
        assert!(!repo.path().join("must-not-run").exists());
        Ok(())
    }

    #[test]
    fn recovery_fingerprint_applies_process_policy_to_exact_control_observation()
    -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        for (label, contents, expected) in [
            (
                b"rebase-merge/git-rebase-todo".as_slice(),
                "exec touch must-not-run\n",
                "rebase plan contains an exec",
            ),
            (
                b"rebase-merge/strategy".as_slice(),
                "resolve\n",
                "explicit merge strategy",
            ),
        ] {
            let input = repo.path().join("control-input");
            fs::write(&input, contents)?;
            let mut hasher = Sha256::new();
            let mut entries = 0;
            let mut fence = RecoveryInputFence::default();

            let Err(error) = hash_recovery_path_fenced(
                &mut hasher,
                label,
                &input,
                Instant::now() + Duration::from_secs(2),
                &mut entries,
                Some(&mut fence),
            ) else {
                return Err(format!("expected exact {expected} observation to be rejected").into());
            };

            assert!(error.to_string().contains(expected), "{error}");
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_persisted_explicit_strategy_before_spawn() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let (repo, _original_head) = prepare_rebase_conflict()?;
        repo.write("conflict.txt", "resolved\n")?;
        repo.run(["add", "conflict.txt"])?;
        let exec_dir = TempRepo::new()?;
        let marker = repo.path().join("explicit-strategy-ran");
        let strategy = exec_dir.path().join("git-merge-resolve");
        fs::write(
            &strategy,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )?;
        let mut permissions = fs::metadata(&strategy)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&strategy, permissions)?;
        let git = Git::new(repo.path()).with_test_git_exec_path(exec_dir.path());
        fs::write(git.git_path("rebase-merge/strategy")?, "resolve\n")?;

        let Err(error) = git.recover(RepositoryOperation::Rebase, RecoveryAction::Continue) else {
            return Err("expected a persisted explicit strategy to be rejected".into());
        };

        assert!(error.to_string().contains("explicit merge strategy"));
        assert!(!marker.exists(), "explicit merge strategy was started");
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        Ok(())
    }

    #[test]
    fn recovery_rejects_persisted_strategy_options_before_spawn() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_rebase_conflict()?;
        repo.write("conflict.txt", "resolved\n")?;
        repo.run(["add", "conflict.txt"])?;
        let git = Git::new(repo.path());
        fs::write(
            git.git_path("rebase-merge/strategy_opts")?,
            "--evil-option\n",
        )?;

        let Err(error) = git.recover(RepositoryOperation::Rebase, RecoveryAction::Continue) else {
            return Err("expected persisted strategy options to be rejected".into());
        };

        assert!(error.to_string().contains("strategy option"));
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_rejects_tab_delimited_rebase_exec_commands_before_spawn()
    -> Result<(), Box<dyn Error>> {
        for (index, command) in ["exec", "x"].into_iter().enumerate() {
            let (repo, _original_head) = prepare_rebase_conflict()?;
            repo.write("conflict.txt", "resolved\n")?;
            repo.run(["add", "conflict.txt"])?;
            let git = Git::new(repo.path());
            let todo = git.git_path("rebase-merge/git-rebase-todo")?;
            let marker = repo.path().join(format!("tab-exec-{index}-ran"));
            fs::write(&todo, format!("{command}\ttouch '{}'\n", marker.display()))?;

            let Err(error) = git.recover(RepositoryOperation::Rebase, RecoveryAction::Continue)
            else {
                return Err(format!("expected tab-delimited {command} to fail closed").into());
            };

            assert!(error.to_string().contains("rebase plan contains an exec"));
            assert!(!marker.exists(), "{command} process started");
            assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        }
        Ok(())
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn recovery_platform_lock_serializes_bitbygit_execution() -> Result<(), Box<dyn Error>> {
        let (repo, original_head) = prepare_merge_conflict()?;
        let storage = TempRepo::new()?;
        let git = Git::new(repo.path()).with_recovery_data_dir(storage.path());
        let expected = git.prepare_recovery(RepositoryOperation::Merge, RecoveryAction::Abort)?;
        let held = git.acquire_recovery_lock_until(
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            Instant::now() + Duration::from_secs(2),
        )?;

        let Err(error) =
            git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)
        else {
            return Err("expected concurrent BitByGit recovery to be blocked".into());
        };
        assert!(error.to_string().contains("another BitByGit recovery"));
        assert_eq!(
            repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );

        drop(held);
        git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)?;
        assert_eq!(git.status()?.operation, None);
        Ok(())
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn recovery_lock_replacement_cannot_enable_concurrent_recovery() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let storage = TempRepo::new()?;
        let git = Git::new(repo.path()).with_recovery_data_dir(storage.path());
        let expected = git.prepare_recovery(RepositoryOperation::Merge, RecoveryAction::Abort)?;
        let lock = git.acquire_recovery_lock_until(
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            Instant::now() + Duration::from_secs(2),
        )?;
        git.ensure_recovery_state(
            &expected,
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            Instant::now() + Duration::from_secs(2),
        )?;
        let replaced = lock.path.with_extension("replaced");
        fs::rename(&lock.path, &replaced)?;
        fs::File::create(&lock.path)?;

        let Err(concurrent_error) =
            git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)
        else {
            return Err("expected stable repository lock to block concurrent recovery".into());
        };
        assert!(
            concurrent_error
                .to_string()
                .contains("another BitByGit recovery")
        );

        let Err(error) = lock.ensure_identity(
            &lock.repository_root,
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
        ) else {
            return Err("expected replaced recovery lock to fail identity check".into());
        };

        assert!(error.to_string().contains("lock identity changed"));
        drop(lock);
        fs::remove_file(replaced)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_platform_lock_directory_replacement_is_rejected_after_final_validation()
    -> Result<(), Box<dyn Error>> {
        let (repo, original_head) = prepare_merge_conflict()?;
        let storage = TempRepo::new()?;
        let git = Git::new(repo.path()).with_recovery_data_dir(storage.path());
        let expected = git.prepare_recovery(RepositoryOperation::Merge, RecoveryAction::Abort)?;
        let lock_directory = storage.path().join(RECOVERY_LOCK_DIRECTORY);
        let replaced_directory = storage.path().join("recovery-locks-replaced");
        let concurrent = std::cell::RefCell::new(None);

        let Err(error) = git.recover_exact_with(
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            &expected,
            || {
                fs::rename(&lock_directory, &replaced_directory)
                    .map_err(|source| recovery_state_io(&lock_directory, source))?;
                fs::create_dir(&lock_directory)
                    .map_err(|source| recovery_state_io(&lock_directory, source))?;
                let lock = git.acquire_recovery_lock_until(
                    RepositoryOperation::Merge,
                    RecoveryAction::Abort,
                    Instant::now() + Duration::from_secs(2),
                )?;
                *concurrent.borrow_mut() = Some(lock);
                Ok(())
            },
        ) else {
            return Err("expected replaced lock directory to block recovery".into());
        };

        assert!(concurrent.borrow().is_some());
        assert!(error.to_string().contains("lock identity changed"));
        assert_eq!(
            repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        drop(concurrent.into_inner());
        fs::remove_dir_all(replaced_directory)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_lock_survives_git_directory_replacement() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let storage = TempRepo::new()?;
        let git = Git::new(repo.path()).with_recovery_data_dir(storage.path());
        let lock = git.acquire_recovery_lock_until(
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            Instant::now() + Duration::from_secs(2),
        )?;
        let original_lock_path = lock.path.clone();
        fs::rename(repo.path().join(".git"), repo.path().join(".git-original"))?;
        repo.run(["init", "-b", "main"])?;
        let replacement = Git::new(repo.path()).with_recovery_data_dir(storage.path());

        assert_eq!(
            replacement.recovery_lock_path(
                RepositoryOperation::Merge,
                RecoveryAction::Abort,
                &replacement
                    .canonical_repository_root_until(Instant::now() + Duration::from_secs(2))?,
            )?,
            original_lock_path
        );
        let Err(error) = replacement.acquire_recovery_lock_until(
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            Instant::now() + Duration::from_secs(2),
        ) else {
            return Err("expected replacement Git directory to use the held app lock".into());
        };
        assert!(error.to_string().contains("another BitByGit recovery"));
        Ok(())
    }

    #[test]
    fn recovery_platform_lock_key_and_path_are_safe() -> Result<(), Box<dyn Error>> {
        let repo = initialized_repo()?;
        let storage = TempRepo::new()?;
        let git = Git::new(repo.path()).with_recovery_data_dir(storage.path());
        let root = git.canonical_repository_root_until(Instant::now() + Duration::from_secs(2))?;
        let path =
            git.recovery_lock_path(RepositoryOperation::Merge, RecoveryAction::Abort, &root)?;
        let key = recovery_lock_key_from_path(&path).ok_or("unsafe recovery lock name")?;
        let expected_parent = fs::canonicalize(storage.path().join(RECOVERY_LOCK_DIRECTORY))?;

        assert_eq!(key.len(), 64);
        assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(path.parent(), Some(expected_parent.as_path()));
        assert!(!repo.path().join(".git/bitbygit-recovery.lock").exists());
        Ok(())
    }

    #[test]
    fn recovery_storage_uses_bitbygit_data_root_precedence() {
        let resolved = resolve_recovery_data_dir(|name| match name {
            "BITBYGIT_DATA_DIR" => Some(PathBuf::from("/custom/data")),
            "XDG_DATA_HOME" => Some(PathBuf::from("/xdg/data")),
            "APPDATA" => Some(PathBuf::from("/appdata")),
            "HOME" => Some(PathBuf::from("/home/test")),
            _ => None,
        });
        assert_eq!(resolved, Some(PathBuf::from("/custom/data")));

        let resolved = resolve_recovery_data_dir(|name| match name {
            "XDG_DATA_HOME" => Some(PathBuf::from("/xdg/data")),
            "HOME" => Some(PathBuf::from("/home/test")),
            _ => None,
        });
        assert_eq!(resolved, Some(PathBuf::from("/xdg/data/bitbygit")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn exact_recovery_surfaces_standard_git_lock_failure() -> Result<(), Box<dyn Error>> {
        let (repo, original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.prepare_recovery(RepositoryOperation::Merge, RecoveryAction::Abort)?;
        let index_lock = git.git_path("index.lock")?;
        fs::write(&index_lock, "external Git writer\n")?;

        let Err(error) =
            git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)
        else {
            return Err("expected Git index lock failure".into());
        };

        let GitError::GitFailed { args, stderr, .. } = error else {
            return Err("expected standard Git failure to be surfaced".into());
        };
        assert_eq!(args, ["merge".to_owned(), "--abort".to_owned()]);
        assert!(stderr.contains("index.lock"));
        assert_eq!(
            repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        fs::remove_file(index_lock)?;
        Ok(())
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn recovery_platform_end_to_end_exact_merge_and_rebase() -> Result<(), Box<dyn Error>> {
        ensure_recovery_execution_supported(RepositoryOperation::Merge, RecoveryAction::Abort)?;
        let (repo, original_head) = prepare_merge_conflict()?;
        let storage = TempRepo::new()?;
        let git = Git::new(repo.path()).with_recovery_data_dir(storage.path());
        let expected = git.prepare_recovery(RepositoryOperation::Merge, RecoveryAction::Abort)?;
        git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)?;
        assert_eq!(git.status()?.operation, None);
        assert_eq!(
            repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );
        assert!(!git.git_path("index.lock")?.exists());
        drop(git.acquire_recovery_lock_until(
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            Instant::now() + Duration::from_secs(2),
        )?);

        let (repo, original_head) = prepare_rebase_conflict()?;
        let git = Git::new(repo.path()).with_recovery_data_dir(storage.path());
        let expected = git.prepare_recovery(RepositoryOperation::Rebase, RecoveryAction::Abort)?;
        git.recover_exact(
            RepositoryOperation::Rebase,
            RecoveryAction::Abort,
            &expected,
        )?;
        assert_eq!(git.status()?.operation, None);
        assert_eq!(
            repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("conflict.txt"))?,
            "topic\n"
        );
        assert!(!git.git_path("index.lock")?.exists());
        drop(git.acquire_recovery_lock_until(
            RepositoryOperation::Rebase,
            RecoveryAction::Abort,
            Instant::now() + Duration::from_secs(2),
        )?);
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    #[test]
    fn recovery_platform_capability_fails_closed_before_planning() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());

        let Err(error) = git.prepare_recovery(RepositoryOperation::Merge, RecoveryAction::Abort)
        else {
            return Err("expected recovery planning to fail closed".into());
        };

        let message = error.to_string();
        assert!(message.contains("recovery planning and execution are blocked"));
        assert!(message.contains("hard output and descendant-process bounds"));
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_hooks_that_would_detach_descendants() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let (repo, _original_head) = prepare_merge_conflict()?;
        repo.write("conflict.txt", "resolved\n")?;
        repo.run(["add", "conflict.txt"])?;
        let background_pid = repo.path().join("background.pid");
        let detached_pid = repo.path().join("detached.pid");
        let hook = repo.path().join(".git/hooks/commit-msg");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nsleep 30 &\nprintf '%s' $! > '{}'\nsetsid sleep 30 &\nprintf '%s' $! > '{}'\n",
                background_pid.display(),
                detached_pid.display()
            ),
        )?;
        let mut permissions = fs::metadata(&hook)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions)?;
        let git = Git::with_recovery_limits(
            repo.path(),
            Duration::from_secs(10),
            Duration::from_secs(5),
            4096,
        );
        let Err(error) = git.recover(RepositoryOperation::Merge, RecoveryAction::Continue) else {
            return Err("expected detached hook to fail closed".into());
        };

        assert!(error.to_string().contains("executable Git hooks"));
        assert!(!background_pid.exists());
        assert!(!detached_pid.exists());
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        Ok(())
    }

    #[test]
    fn detects_both_rebase_backends_and_no_operation_in_clean_repo() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        let git = Git::new(repo.path());

        assert_eq!(git.status()?.operation, None);

        for marker in ["rebase-merge", "rebase-apply"] {
            let path = git.git_path(marker)?;
            fs::create_dir(&path)?;
            assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
            fs::remove_dir(path)?;
        }
        Ok(())
    }

    #[test]
    fn paused_am_is_not_reported_as_rebase_and_remains_blocking() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        repo.run(["commit", "--allow-empty", "-m", "empty"])?;
        let patch = repo.git_stdout(["format-patch", "-1", "--stdout"])?;
        repo.run(["reset", "--hard", "HEAD~"])?;
        repo.write("empty.patch", &patch)?;
        repo.run_allow_failure(["am", "empty.patch"])?;
        let git = Git::new(repo.path());

        assert!(git.git_path("rebase-apply/applying")?.exists());
        assert_eq!(git.status()?.operation, None);
        let Err(error) = git.ensure_clean_worktree("checkout") else {
            return Err("expected paused am guardrail".into());
        };
        assert!(error.to_string().contains("am operation is in progress"));
        Ok(())
    }

    #[test]
    fn cherry_pick_and_revert_markers_remain_blocking() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        let git = Git::new(repo.path());

        for (marker, operation) in [
            ("CHERRY_PICK_HEAD", "cherry-pick"),
            ("REVERT_HEAD", "revert"),
        ] {
            let path = git.git_path(marker)?;
            fs::write(&path, "marker\n")?;

            assert_eq!(git.status()?.operation, None);
            let Err(error) = git.ensure_clean_worktree("checkout") else {
                return Err(format!("expected {operation} marker to block checkout").into());
            };
            assert!(error.to_string().contains(operation));

            fs::remove_file(path)?;
        }
        Ok(())
    }

    #[test]
    fn reads_renames_from_temp_repo() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("old name.txt", "old\n")?;
        repo.run(["add", "old name.txt"])?;
        repo.run(["commit", "-m", "initial"])?;
        repo.run(["mv", "old name.txt", "new\tname.txt"])?;

        let status = Git::new(repo.path()).status()?;

        assert_eq!(status.entries[0].entry_type, StatusEntryType::Renamed);
        assert_eq!(status.entries[0].path, PathBuf::from("new\tname.txt"));
        assert_eq!(
            status.entries[0].original_path,
            Some(PathBuf::from("old name.txt"))
        );
        Ok(())
    }

    #[test]
    fn reads_raw_paths_from_temp_repo() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.write("café.txt", "unicode\n")?;
        repo.write("tab\tname.txt", "tab\n")?;

        let status = Git::new(repo.path()).status()?;

        assert_eq!(status.untracked_files().len(), 2);
        assert!(
            status
                .entries
                .iter()
                .any(|entry| entry.path.as_path() == Path::new("café.txt"))
        );
        assert!(
            status
                .entries
                .iter()
                .any(|entry| entry.path.as_path() == Path::new("tab\tname.txt"))
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn reads_non_utf8_paths_from_temp_repo() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        let path = PathBuf::from(OsString::from_vec(b"bad-\xff.txt".to_vec()));
        repo.write_path(&path, "non-utf8\n")?;

        let status = Git::new(repo.path()).status()?;

        assert!(
            status
                .entries
                .iter()
                .any(|entry| entry.path.as_os_str().as_bytes() == b"bad-\xff.txt")
        );
        Ok(())
    }

    #[test]
    fn upstream_missing_is_none() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;

        assert_eq!(Git::new(repo.path()).upstream()?, None);
        Ok(())
    }

    #[test]
    fn upstream_present_is_returned() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        repo.run([
            "remote",
            "add",
            "origin",
            "https://example.invalid/repo.git",
        ])?;
        repo.run(["update-ref", "refs/remotes/origin/main", "HEAD"])?;
        repo.run(["branch", "--set-upstream-to", "origin/main"])?;

        assert_eq!(
            Git::new(repo.path()).upstream()?,
            Some("origin/main".to_owned())
        );
        Ok(())
    }

    #[test]
    fn upstream_invalid_repo_is_error() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;

        let result = Git::new(repo.path()).upstream();

        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn stages_and_unstages_selected_path() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        repo.write("README.md", "changed\n")?;

        let git = Git::new(repo.path());
        git.stage_path(Path::new("README.md"))?;
        assert_eq!(git.status()?.staged_files().len(), 1);

        git.unstage_path(Path::new("README.md"))?;
        assert_eq!(git.status()?.staged_files().len(), 0);
        assert_eq!(git.status()?.unstaged_files().len(), 1);
        Ok(())
    }

    #[test]
    fn stages_and_unstages_all_paths() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        repo.write("one.txt", "one\n")?;
        repo.write("two.txt", "two\n")?;

        let git = Git::new(repo.path());
        git.stage_all()?;
        assert_eq!(git.status()?.staged_files().len(), 2);

        git.unstage_all()?;
        assert_eq!(git.status()?.staged_files().len(), 0);
        assert_eq!(git.status()?.untracked_files().len(), 2);
        Ok(())
    }

    #[test]
    fn unstages_selected_path_in_unborn_repository() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;

        let git = Git::new(repo.path());
        git.unstage_path(Path::new("README.md"))?;

        let status = git.status()?;
        assert_eq!(status.staged_files().len(), 0);
        assert_eq!(status.untracked_files().len(), 1);
        Ok(())
    }

    #[test]
    fn unstages_all_paths_in_unborn_repository() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.write("one.txt", "one\n")?;
        repo.write("two.txt", "two\n")?;
        repo.run(["add", "one.txt", "two.txt"])?;

        let git = Git::new(repo.path());
        git.unstage_all()?;

        let status = git.status()?;
        assert_eq!(status.staged_files().len(), 0);
        assert_eq!(status.untracked_files().len(), 2);
        Ok(())
    }

    #[test]
    fn unstages_and_diffs_staged_rename_with_source_and_destination() -> Result<(), Box<dyn Error>>
    {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("old.txt", "content\n")?;
        repo.run(["add", "old.txt"])?;
        repo.run(["commit", "-m", "initial"])?;
        repo.run(["mv", "old.txt", "new.txt"])?;

        let git = Git::new(repo.path());
        let paths = vec![PathBuf::from("old.txt"), PathBuf::from("new.txt")];
        let diff = git.diff_paths(&paths, true)?;
        assert!(diff.contains("rename from old.txt"));
        assert!(diff.contains("rename to new.txt"));

        git.unstage_paths(&paths)?;

        assert_eq!(git.status()?.staged_files().len(), 0);
        Ok(())
    }

    #[test]
    fn commits_staged_changes_with_message() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;

        let output = Git::new(repo.path()).commit("initial commit")?;

        assert!(output.stdout.contains("initial commit"));
        assert!(Git::new(repo.path()).status()?.is_clean());
        Ok(())
    }

    #[test]
    fn commit_message_is_passed_without_shell_execution() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;

        Git::new(repo.path()).commit("literal $(touch owned)")?;

        assert!(!repo.path().join("owned").exists());
        Ok(())
    }

    #[test]
    fn push_current_branch_set_upstream_sets_tracking_branch() -> Result<(), Box<dyn Error>> {
        let remote = TempRepo::new()?;
        remote.run(["init", "--bare"])?;
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        let remote_path = remote.path().to_string_lossy().into_owned();
        repo.run_args(&["remote", "add", "origin", &remote_path])?;

        let head = repo.git_stdout(["rev-parse", "HEAD"])?;
        Git::new(repo.path()).push_current_branch_set_upstream(
            "origin",
            "main",
            head.trim(),
            None,
        )?;

        assert_eq!(
            Git::new(repo.path()).upstream()?,
            Some("origin/main".to_owned())
        );
        Ok(())
    }

    #[test]
    fn commit_staged_tree_uses_confirmed_tree() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        let git = Git::new(repo.path());
        let confirmed_tree = git.staged_tree()?;
        let target = git.head_target()?;

        repo.write("injected.txt", "not confirmed\n")?;
        repo.run(["add", "injected.txt"])?;
        git.commit_staged_tree("initial commit", &confirmed_tree, &target)?;

        let files = repo.git_stdout(["ls-tree", "--name-only", "HEAD"])?;
        assert!(files.contains("README.md"));
        assert!(!files.contains("injected.txt"));
        assert_eq!(git.status()?.staged_files().len(), 1);
        Ok(())
    }

    #[test]
    fn commit_staged_tree_rejects_changed_head() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        repo.write("README.md", "changed\n")?;
        repo.run(["add", "README.md"])?;
        let git = Git::new(repo.path());
        let confirmed_tree = git.staged_tree()?;
        let target = git.head_target()?;
        let Some(confirmed_head) = target.oid.clone() else {
            return Err("expected repository to have HEAD".into());
        };
        let base_tree = repo.git_stdout(["rev-parse", "HEAD^{tree}"])?;
        let external_commit = repo.git_stdout_args(&[
            "commit-tree",
            base_tree.trim(),
            "-p",
            &confirmed_head,
            "-m",
            "external",
        ])?;
        repo.run_args(&["update-ref", "HEAD", external_commit.trim()])?;

        let result = git.commit_staged_tree("confirmed", &confirmed_tree, &target);

        let Err(error) = result else {
            return Err("expected stale HEAD rejection".into());
        };
        assert!(error.to_string().contains("target ref changed"));
        let head = repo.git_stdout(["rev-parse", "HEAD"])?;
        assert_eq!(head.trim(), external_commit.trim());
        Ok(())
    }

    #[test]
    fn commit_staged_tree_rejects_changed_head_ref() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        repo.write("README.md", "changed\n")?;
        repo.run(["add", "README.md"])?;
        let git = Git::new(repo.path());
        let confirmed_tree = git.staged_tree()?;
        let target = git.head_target()?;
        repo.run(["branch", "other"])?;
        repo.run(["checkout", "other"])?;

        let result = git.commit_staged_tree("confirmed", &confirmed_tree, &target);

        let Err(error) = result else {
            return Err("expected stale ref rejection".into());
        };
        assert!(error.to_string().contains("target ref changed"));
        assert_eq!(
            repo.git_stdout(["rev-parse", "other"])?.trim(),
            target.oid.as_deref().unwrap_or_default()
        );
        Ok(())
    }

    #[test]
    fn commit_staged_tree_blocks_empty_commit() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        let git = Git::new(repo.path());
        let tree = git.staged_tree()?;
        let target = git.head_target()?;

        let result = git.commit_staged_tree("empty", &tree, &target);

        let Err(error) = result else {
            return Err("expected empty commit guardrail".into());
        };
        assert!(error.to_string().contains("no staged changes"));
        Ok(())
    }

    #[test]
    fn commit_staged_tree_blocks_gpgsign_policy() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.run(["config", "commit.gpgsign", "true"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        let git = Git::new(repo.path());
        let tree = git.staged_tree()?;
        let target = git.head_target()?;

        let result = git.commit_staged_tree("initial", &tree, &target);

        let Err(error) = result else {
            return Err("expected gpgsign guardrail".into());
        };
        assert!(error.to_string().contains("commit.gpgsign"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn commit_staged_tree_blocks_configured_commit_hooks() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        let hook = repo.path().join(".git").join("hooks").join("commit-msg");
        fs::write(&hook, "#!/bin/sh\nexit 1\n")?;
        let mut permissions = fs::metadata(&hook)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions)?;
        let git = Git::new(repo.path());
        let tree = git.staged_tree()?;
        let target = git.head_target()?;

        let result = git.commit_staged_tree("initial", &tree, &target);

        let Err(error) = result else {
            return Err("expected hook guardrail".into());
        };
        assert!(error.to_string().contains("commit hook"));
        Ok(())
    }

    #[test]
    fn staged_tree_changes_when_staged_content_changes() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.write("README.md", "one\n")?;
        repo.run(["add", "README.md"])?;
        let git = Git::new(repo.path());
        let first_tree = git.staged_tree()?;

        repo.write("README.md", "two\n")?;
        repo.run(["add", "README.md"])?;
        let second_tree = git.staged_tree()?;

        assert_ne!(first_tree, second_tree);
        Ok(())
    }

    #[test]
    fn returns_diff_for_selected_path() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        repo.write("README.md", "changed\n")?;

        let diff = Git::new(repo.path()).diff_path(Path::new("README.md"), false)?;

        assert!(diff.contains("-initial"));
        assert!(diff.contains("+changed"));
        Ok(())
    }

    #[test]
    fn selected_path_operations_treat_pathspec_magic_as_literal() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.write(":(glob)*", "literal\n")?;
        repo.run(["add", "README.md", ":(glob)*"])?;
        repo.run(["commit", "-m", "initial"])?;
        repo.write("README.md", "changed\n")?;
        repo.write(":(glob)*", "changed\n")?;

        let git = Git::new(repo.path());
        let diff = git.diff_path(Path::new(":(glob)*"), false)?;
        assert!(diff.contains("-literal"));
        assert!(!diff.contains("-initial"));

        git.stage_path(Path::new(":(glob)*"))?;
        let status = git.status()?;

        assert_eq!(status.staged_files().len(), 1);
        assert_eq!(status.unstaged_files().len(), 1);
        assert_eq!(status.staged_files()[0].path, PathBuf::from(":(glob)*"));
        assert_eq!(status.unstaged_files()[0].path, PathBuf::from("README.md"));

        git.unstage_path(Path::new(":(glob)*"))?;
        let status = git.status()?;
        assert_eq!(status.staged_files().len(), 0);
        assert_eq!(status.unstaged_files().len(), 2);
        Ok(())
    }

    #[test]
    fn upstream_unborn_repo_is_none() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;

        assert_eq!(Git::new(repo.path()).upstream()?, None);
        Ok(())
    }

    #[test]
    fn remote_push_urls_applies_push_instead_of_rewrites() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run([
            "remote",
            "add",
            "origin",
            "https://github.com/upstream/repo.git",
        ])?;
        repo.run([
            "config",
            "--add",
            "url.https://github.com/fork/repo.git.pushInsteadOf",
            "https://github.com/upstream/repo.git",
        ])?;
        assert_eq!(
            Git::new(repo.path()).remote_push_urls("origin")?,
            vec!["https://github.com/fork/repo.git"]
        );
        Ok(())
    }

    #[test]
    fn remote_push_urls_accepts_leading_hyphen_remote() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run([
            "remote",
            "add",
            "--",
            "-fork",
            "https://github.com/fork/repo.git",
        ])?;

        assert_eq!(
            Git::new(repo.path()).remote_push_urls("-fork")?,
            vec!["https://github.com/fork/repo.git"]
        );
        Ok(())
    }

    #[test]
    fn push_target_uses_default_simple_in_triangular_workflow() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "feature"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.run(["commit", "--allow-empty", "-m", "initial"])?;
        repo.run([
            "remote",
            "add",
            "upstream",
            "https://github.com/upstream/repo.git",
        ])?;
        repo.run(["remote", "add", "fork", "https://github.com/fork/repo.git"])?;
        repo.run(["update-ref", "refs/remotes/upstream/main", "HEAD"])?;
        repo.run(["update-ref", "refs/remotes/fork/feature", "HEAD"])?;
        repo.run(["config", "branch.feature.remote", "upstream"])?;
        repo.run(["config", "branch.feature.merge", "refs/heads/main"])?;
        repo.run(["config", "branch.feature.pushRemote", "fork"])?;

        assert_eq!(
            Git::new(repo.path()).push_target("feature")?,
            Some(("fork".to_owned(), "feature".to_owned()))
        );
        Ok(())
    }

    #[test]
    fn typed_push_target_resolves_tracked_and_new_branches() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "topic"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.run(["commit", "--allow-empty", "-m", "initial"])?;
        repo.run(["remote", "add", "fork", "https://github.com/octo/repo.git"])?;
        repo.run([
            "remote",
            "add",
            "origin",
            "https://github.com/upstream/repo.git",
        ])?;
        repo.run(["config", "branch.topic.remote", "fork"])?;
        repo.run(["config", "branch.topic.merge", "refs/heads/topic"])?;
        let git = Git::new(repo.path());

        assert_eq!(
            git.typed_push_target("topic", Some(("fork", "topic")), Some("origin"))?,
            git.push_target("topic")?
        );

        repo.run(["config", "remote.pushDefault", "origin"])?;
        for push_default in [
            "simple", "current", "upstream", "tracking", "matching", "nothing",
        ] {
            repo.run(["config", "push.default", push_default])?;
            assert_eq!(
                git.typed_push_target("topic", Some(("fork", "topic")), Some("fork"))?,
                git.push_target("topic")?,
                "push.default={push_default}"
            );
        }

        repo.run(["config", "branch.topic.pushRemote", "fork"])?;
        repo.run(["config", "push.default", "current"])?;
        assert_eq!(
            git.typed_push_target("topic", Some(("fork", "topic")), Some("origin"))?,
            git.push_target("topic")?
        );
        repo.run(["config", "branch.new.pushRemote", "fork"])?;
        assert_eq!(
            git.typed_push_target("new", None, Some("origin"))?,
            Some(("fork".to_owned(), "new".to_owned()))
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn remote_url_head_oid_ignores_configured_ssh_command() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        let marker = repo.path().join("configured-ssh-ran");
        let configured_ssh = repo.path().join("configured-ssh");
        fs::write(
            &configured_ssh,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )?;
        let mut permissions = fs::metadata(&configured_ssh)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&configured_ssh, permissions)?;
        repo.run_args(&[
            "config",
            "core.sshCommand",
            &configured_ssh.display().to_string(),
        ])?;

        let result =
            Git::new(repo.path()).remote_url_head_oid("ssh://git@127.0.0.1:1/repo.git", "main");

        assert!(result.is_err());
        assert!(!marker.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn fetch_preserves_inherited_ssh_command() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        if let Some(repo) = env::var_os("BITBYGIT_TEST_INHERITED_SSH_REPO") {
            Git::new(repo).fetch_remote_branch("origin", "main")?;
            return Ok(());
        }

        let remote = TempRepo::new()?;
        remote.run(["init", "--bare"])?;
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.run(["commit", "--allow-empty", "-m", "initial"])?;
        let remote_path = remote.path().to_string_lossy().into_owned();
        repo.run_args(&["remote", "add", "origin", &remote_path])?;
        repo.run(["push", "origin", "main"])?;
        repo.run([
            "remote",
            "set-url",
            "origin",
            "ssh://git@127.0.0.1/repo.git",
        ])?;

        let inherited_marker = repo.path().join("inherited-ssh-ran");
        let inherited_ssh = repo.path().join("inherited-ssh");
        fs::write(
            &inherited_ssh,
            "#!/bin/sh\ntouch \"$BITBYGIT_TEST_INHERITED_SSH_MARKER\"\nexec git-upload-pack \"$BITBYGIT_TEST_INHERITED_SSH_REMOTE\"\n",
        )?;
        let mut permissions = fs::metadata(&inherited_ssh)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&inherited_ssh, permissions)?;

        let configured_marker = repo.path().join("configured-ssh-ran");
        let configured_ssh = repo.path().join("configured-ssh");
        fs::write(
            &configured_ssh,
            format!(
                "#!/bin/sh\ntouch '{}'\nexit 1\n",
                configured_marker.display()
            ),
        )?;
        let mut permissions = fs::metadata(&configured_ssh)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&configured_ssh, permissions)?;
        repo.run_args(&[
            "config",
            "core.sshCommand",
            &configured_ssh.display().to_string(),
        ])?;

        let output = Command::new(env::current_exe()?)
            .args([
                "--exact",
                "tests::fetch_preserves_inherited_ssh_command",
                "--nocapture",
            ])
            .env("BITBYGIT_TEST_INHERITED_SSH_REPO", repo.path())
            .env("BITBYGIT_TEST_INHERITED_SSH_REMOTE", remote.path())
            .env("BITBYGIT_TEST_INHERITED_SSH_MARKER", &inherited_marker)
            .env("GIT_SSH_COMMAND", &inherited_ssh)
            .output()?;

        assert!(
            output.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(inherited_marker.exists());
        assert!(!configured_marker.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn reads_non_utf8_repository_root() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new_non_utf8()?;
        repo.run(["init", "-b", "main"])?;

        let root = Git::new(repo.path()).repo_root()?;

        assert_eq!(
            root.as_os_str().as_bytes(),
            repo.path().as_os_str().as_bytes()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn reads_repository_root_ending_in_carriage_return() -> Result<(), Box<dyn Error>> {
        let repo = TempRepo::new_with_raw_suffix(b"-cr\r")?;
        repo.run(["init", "-b", "main"])?;

        let root = Git::new(repo.path()).repo_root()?;

        assert_eq!(
            root.as_os_str().as_bytes(),
            repo.path().as_os_str().as_bytes()
        );
        Ok(())
    }

    fn initialized_repo() -> Result<TempRepo, Box<dyn Error>> {
        let repo = TempRepo::new()?;
        repo.run(["init", "-b", "main"])?;
        repo.run(["config", "user.email", "bitbygit@example.invalid"])?;
        repo.run(["config", "user.name", "bitbygit test"])?;
        repo.write("README.md", "initial\n")?;
        repo.run(["add", "README.md"])?;
        repo.run(["commit", "-m", "initial"])?;
        Ok(repo)
    }

    fn prepare_merge_conflict() -> Result<(TempRepo, String), Box<dyn Error>> {
        let repo = initialized_repo()?;
        repo.write("conflict.txt", "base\n")?;
        repo.run(["add", "conflict.txt"])?;
        repo.run(["commit", "-m", "base"])?;
        repo.run(["switch", "-c", "other"])?;
        repo.write("conflict.txt", "other\n")?;
        repo.run(["commit", "-am", "other"])?;
        repo.run(["switch", "main"])?;
        repo.write("conflict.txt", "main\n")?;
        repo.run(["commit", "-am", "main"])?;
        let original_head = repo.git_stdout(["rev-parse", "HEAD"])?.trim().to_owned();
        repo.run_allow_failure(["merge", "other"])?;
        assert_eq!(
            Git::new(repo.path()).status()?.operation,
            Some(RepositoryOperation::Merge)
        );
        Ok((repo, original_head))
    }

    fn prepare_rebase_conflict() -> Result<(TempRepo, String), Box<dyn Error>> {
        let repo = initialized_repo()?;
        repo.write("conflict.txt", "base\n")?;
        repo.run(["add", "conflict.txt"])?;
        repo.run(["commit", "-m", "base"])?;
        repo.run(["switch", "-c", "topic"])?;
        repo.write("conflict.txt", "topic\n")?;
        repo.run(["commit", "-am", "topic"])?;
        let original_head = repo.git_stdout(["rev-parse", "HEAD"])?.trim().to_owned();
        repo.run(["switch", "main"])?;
        repo.write("conflict.txt", "main\n")?;
        repo.run(["commit", "-am", "main"])?;
        repo.run(["switch", "topic"])?;
        repo.run_allow_failure(["rebase", "main"])?;
        assert_eq!(
            Git::new(repo.path()).status()?.operation,
            Some(RepositoryOperation::Rebase)
        );
        Ok((repo, original_head))
    }

    fn prepare_two_conflict_rebase() -> Result<TempRepo, Box<dyn Error>> {
        let repo = initialized_repo()?;
        repo.write("first.txt", "base first\n")?;
        repo.write("second.txt", "base second\n")?;
        repo.run(["add", "first.txt", "second.txt"])?;
        repo.run(["commit", "-m", "base"])?;
        repo.run(["switch", "-c", "topic"])?;
        repo.write("first.txt", "topic first\n")?;
        repo.run(["commit", "-am", "topic first"])?;
        repo.write("second.txt", "topic second\n")?;
        repo.run(["commit", "-am", "topic second"])?;
        repo.run(["switch", "main"])?;
        repo.write("first.txt", "main first\n")?;
        repo.write("second.txt", "main second\n")?;
        repo.run(["commit", "-am", "main changes"])?;
        repo.run(["switch", "topic"])?;
        repo.run_allow_failure(["rebase", "main"])?;
        assert_eq!(
            Git::new(repo.path()).status()?.operation,
            Some(RepositoryOperation::Rebase)
        );
        Ok(repo)
    }

    fn prepare_rebase_merge_conflict() -> Result<(TempRepo, String), Box<dyn Error>> {
        let repo = initialized_repo()?;
        repo.write("conflict.txt", "base\n")?;
        repo.run(["add", "conflict.txt"])?;
        repo.run(["commit", "-m", "base"])?;
        repo.run(["switch", "-c", "topic"])?;
        repo.write("conflict.txt", "topic\n")?;
        repo.run(["commit", "-am", "topic"])?;
        repo.run(["switch", "-c", "side", "main"])?;
        repo.write("conflict.txt", "side\n")?;
        repo.run(["commit", "-am", "side"])?;
        repo.run(["switch", "topic"])?;
        repo.run_allow_failure(["merge", "--no-ff", "side"])?;
        repo.write("conflict.txt", "resolved\n")?;
        repo.run(["add", "conflict.txt"])?;
        repo.run(["commit", "-m", "merge side"])?;
        let original_head = repo.git_stdout(["rev-parse", "HEAD"])?.trim().to_owned();
        repo.run(["switch", "main"])?;
        repo.write("upstream.txt", "upstream\n")?;
        repo.run(["add", "upstream.txt"])?;
        repo.run(["commit", "-m", "upstream"])?;
        repo.run(["switch", "topic"])?;
        repo.run_allow_failure(["rebase", "--rebase-merges", "main"])?;
        let git = Git::new(repo.path());
        assert!(git.git_path("rebase-merge")?.exists());
        assert!(git.git_path("MERGE_HEAD")?.exists());
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        Ok((repo, original_head))
    }

    struct TempRepo {
        path: PathBuf,
    }

    impl TempRepo {
        fn new() -> Result<Self, Box<dyn Error>> {
            let id = NEXT_REPO_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("bitbygit-test-{}-{id}", std::process::id()));
            if path.exists() {
                fs::remove_dir_all(&path)?;
            }
            fs::create_dir_all(&path)?;
            Ok(Self { path })
        }

        #[cfg(unix)]
        fn new_non_utf8() -> Result<Self, Box<dyn Error>> {
            Self::new_with_raw_suffix(b"-\xff")
        }

        #[cfg(unix)]
        fn new_with_raw_suffix(suffix: &[u8]) -> Result<Self, Box<dyn Error>> {
            let id = NEXT_REPO_ID.fetch_add(1, Ordering::Relaxed);
            let mut name = format!("bitbygit-test-{}-{id}", std::process::id()).into_bytes();
            name.extend_from_slice(suffix);
            let path = std::env::temp_dir().join(OsString::from_vec(name));
            if path.exists() {
                fs::remove_dir_all(&path)?;
            }
            fs::create_dir_all(&path)?;
            Ok(Self { path })
        }

        fn path(&self) -> PathBuf {
            self.path.clone()
        }

        fn write(&self, relative_path: &str, contents: &str) -> Result<(), Box<dyn Error>> {
            fs::write(self.path.join(relative_path), contents)?;
            Ok(())
        }

        #[cfg(unix)]
        fn write_path(
            &self,
            relative_path: &PathBuf,
            contents: &str,
        ) -> Result<(), Box<dyn Error>> {
            fs::write(self.path.join(relative_path), contents)?;
            Ok(())
        }

        fn run<const N: usize>(&self, args: [&str; N]) -> Result<(), Box<dyn Error>> {
            let output = Command::new("git")
                .current_dir(&self.path)
                .args(args)
                .output()?;
            if !output.status.success() {
                return Err(format!(
                    "git command failed with status {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                )
                .into());
            }
            Ok(())
        }

        fn run_allow_failure<const N: usize>(&self, args: [&str; N]) -> Result<(), Box<dyn Error>> {
            let _output = Command::new("git")
                .current_dir(&self.path)
                .args(args)
                .output()?;
            Ok(())
        }

        fn run_args(&self, args: &[&str]) -> Result<(), Box<dyn Error>> {
            let output = Command::new("git")
                .current_dir(&self.path)
                .args(args)
                .output()?;
            if !output.status.success() {
                return Err(format!(
                    "git command failed with status {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                )
                .into());
            }
            Ok(())
        }

        fn git_stdout<const N: usize>(&self, args: [&str; N]) -> Result<String, Box<dyn Error>> {
            self.git_stdout_args(&args)
        }

        fn git_stdout_args(&self, args: &[&str]) -> Result<String, Box<dyn Error>> {
            let output = Command::new("git")
                .current_dir(&self.path)
                .args(args)
                .output()?;
            if !output.status.success() {
                return Err(format!(
                    "git command failed with status {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                )
                .into());
            }
            Ok(String::from_utf8(output.stdout)?)
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _result = fs::remove_dir_all(&self.path);
        }
    }
}
