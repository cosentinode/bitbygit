use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::string::FromUtf8Error;

use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const EMPTY_TREE_OID: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const SSH_OPTIONS: &str = "-oBatchMode=yes -oNumberOfPasswordPrompts=0 -oKbdInteractiveAuthentication=no -oStrictHostKeyChecking=yes";
const COMMIT_HOOKS: &[&str] = &[
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
];
const RECOVERY_METADATA_PATHS: &[&str] = &[
    "MERGE_HEAD",
    "MERGE_MSG",
    "MERGE_MODE",
    "MERGE_AUTOSTASH",
    "AUTO_MERGE",
    "ORIG_HEAD",
    "REBASE_HEAD",
    "rebase-merge",
    "rebase-apply",
];
const REBASE_PROGRESS_PATHS: &[&str] = &["rebase-merge/msgnum", "rebase-apply/next"];
const MAX_RECOVERY_RETAINED_BYTES: usize = 8 * 1024 * 1024;
const MAX_RECOVERY_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_RECOVERY_FILE_BYTES_READ: u64 = 64 * 1024 * 1024;
const MAX_RECOVERY_RELEVANT_FILES: usize = 10_000;
const MAX_RECOVERY_PATH_BYTES: usize = 256 * 1024;
const MAX_RECOVERY_METADATA_BYTES: usize = 4 * 1024 * 1024;
const MAX_RECOVERY_METADATA_ENTRIES: usize = 4_096;
const MAX_RECOVERY_SUBPROCESSES: usize = 12;
const MAX_RECOVERY_GENERATION_ENTRIES: usize = 100_000;
const MAX_RECOVERY_GENERATION_PATH_BYTES: usize = 4 * 1024 * 1024;
const MAX_RECOVERY_GENERATION_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const RECOVERY_BACKUP_POINTER: &str = "bitbygit-recovery-backup.pointer";
const RECOVERY_CANDIDATE_PREFIX: &str = ".bitbygit-recovery-candidate-";
const RECOVERY_GIT_ENVIRONMENT: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_DIR",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_QUARANTINE_PATH",
    "GIT_SHALLOW_FILE",
    "GIT_WORK_TREE",
];
const RECOVERY_CAPABILITY_UNAVAILABLE: &str =
    "atomic recovery is unavailable because required platform capabilities are missing";

#[derive(Debug, Clone)]
pub struct Git {
    cwd: PathBuf,
    ssh_executable: Option<PathBuf>,
}

impl Git {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            ssh_executable: None,
        }
    }

    pub fn with_ssh_executable(
        cwd: impl Into<PathBuf>,
        ssh_executable: impl Into<PathBuf>,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            ssh_executable: Some(ssh_executable.into()),
        }
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

    #[cfg(test)]
    fn recover(
        &self,
        operation: RepositoryOperation,
        action: RecoveryAction,
    ) -> Result<GitOutput, GitError> {
        if operation == RepositoryOperation::Merge && action == RecoveryAction::Skip {
            return Err(GitError::Blocked {
                message: "merge skip is blocked because Git does not support it".to_owned(),
            });
        }

        let status = self.status()?;
        match status.operation {
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
        if action == RecoveryAction::Continue && !status.conflicted_files().is_empty() {
            return Err(GitError::Blocked {
                message: format!(
                    "{} continue is blocked while unresolved conflicts are present",
                    operation.label()
                ),
            });
        }

        self.run_recovery_args(vec![
            operation.label().to_owned(),
            format!("--{}", action.label()),
        ])
    }

    pub fn recover_exact(
        &self,
        operation: RepositoryOperation,
        action: RecoveryAction,
        expected_state: &RecoveryState,
    ) -> Result<GitOutput, GitError> {
        ensure_recovery_environment_isolated()?;
        let current_state = self.recovery_state()?;
        if current_state != *expected_state {
            return Err(GitError::Blocked {
                message: format!(
                    "{} {} is blocked because repository state changed after preview",
                    operation.label(),
                    action.label()
                ),
            });
        }
        let mut transaction = RecoveryTransaction::prepare(self)?;
        let result = transaction.run_recovery(
            self,
            vec![
                operation.label().to_owned(),
                format!("--{}", action.label()),
            ],
        );
        match result {
            Ok(mut output) => {
                let backup = transaction.promote(self, &current_state)?;
                append_recovery_backup_notice(&mut output.stderr, &backup);
                Ok(output)
            }
            Err(mut error)
                if operation == RepositoryOperation::Rebase
                    && matches!(action, RecoveryAction::Continue | RecoveryAction::Skip)
                    && transaction.has_advanced_rebase_conflict(&current_state) =>
            {
                let backup = transaction.promote(self, &current_state)?;
                if let GitError::GitFailed { stderr, .. } = &mut error {
                    append_recovery_backup_notice(stderr, &backup);
                }
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    pub fn ensure_recovery_supported(&self) -> Result<(), GitError> {
        ensure_recovery_environment_isolated()?;
        RecoveryTransaction::supported_root(self).map(|_| ())
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

    pub fn remote_tracking_oid(
        &self,
        remote: &str,
        branch: &str,
    ) -> Result<Option<String>, GitError> {
        self.ref_oid(&remote_tracking_ref(remote, branch))
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

    pub fn recovery_state(&self) -> Result<RecoveryState, GitError> {
        let mut capture = RecoveryCapture::default();
        let git_dir = path_from_bytes(strip_byte_line_ending(
            &capture.required(self, &["rev-parse", "--absolute-git-dir"])?,
        ));
        let root = path_from_bytes(strip_byte_line_ending(
            &capture.required(self, &["rev-parse", "--show-toplevel"])?,
        ));
        let operation = recovery_operation_at(&git_dir);
        let head_oid = capture
            .optional(self, &["rev-parse", "--verify", "--quiet", "HEAD"])?
            .map(|output| String::from_utf8_lossy(strip_byte_line_ending(&output)).into_owned());
        let head_reference = capture
            .optional(self, &["symbolic-ref", "--quiet", "HEAD"])?
            .map(|output| String::from_utf8_lossy(strip_byte_line_ending(&output)).into_owned());
        let metadata = self.recovery_metadata(&git_dir, &mut capture)?;
        let original_head = match operation {
            Some(RepositoryOperation::Rebase) => Some(rebase_original_head(&metadata)?),
            Some(RepositoryOperation::Merge) => capture
                .optional(self, &["rev-parse", "--verify", "--quiet", "ORIG_HEAD"])?
                .map(|output| {
                    String::from_utf8_lossy(strip_byte_line_ending(&output)).into_owned()
                }),
            None => None,
        };
        let mut paths = BTreeSet::new();
        let changed_paths =
            capture.required(self, &["diff", "--raw", "-z", "--no-abbrev", "HEAD", "--"])?;
        parse_recovery_changed_paths(&changed_paths, &mut paths)?;
        if let Some(original_head) = original_head {
            let destination_paths = capture.required(
                self,
                &[
                    "diff",
                    "--raw",
                    "-z",
                    "--no-abbrev",
                    "HEAD",
                    &original_head,
                    "--",
                ],
            )?;
            parse_recovery_changed_paths(&destination_paths, &mut paths)?;
            if operation == Some(RepositoryOperation::Rebase) {
                let range = format!("{}..{original_head}", head_oid.as_deref().unwrap_or("HEAD"));
                let changed_paths = capture.required(
                    self,
                    &[
                        "log",
                        "--format=%x00",
                        "--raw",
                        "-z",
                        "--no-abbrev",
                        "--diff-merges=first-parent",
                        &range,
                        "--",
                    ],
                )?;
                parse_recovery_changed_paths(&changed_paths, &mut paths)?;
            }
        }
        let path_bytes = paths
            .iter()
            .map(|path| path.as_os_str().as_encoded_bytes().len())
            .sum::<usize>();
        if path_bytes > MAX_RECOVERY_PATH_BYTES {
            return Err(recovery_bound_error(format!(
                "relevant path bytes exceed {MAX_RECOVERY_PATH_BYTES}"
            )));
        }
        capture.retain(
            path_bytes.saturating_add(paths.len() * 96),
            "relevant paths",
        )?;

        let mut index_args = vec![
            OsString::from("ls-files"),
            OsString::from("--stage"),
            OsString::from("-z"),
            OsString::from("--"),
        ];
        index_args.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
        let index = capture.required_os(self, index_args)?;
        capture.retain(index.len(), "index output")?;

        let worktree = self.snapshot_recovery_worktree(&root, paths, &mut capture)?;
        run_recovery_capture_hook(&self.cwd);
        let refs = self.recovery_refs(operation, &metadata, &mut capture)?;
        capture.retain(refs.len(), "recovery refs")?;

        Ok(RecoveryState {
            operation,
            head: HeadTarget {
                oid: head_oid,
                reference: head_reference,
            },
            index,
            worktree,
            metadata,
            refs,
        })
    }

    fn snapshot_recovery_worktree(
        &self,
        root: &Path,
        mut paths: BTreeSet<PathBuf>,
        capture: &mut RecoveryCapture,
    ) -> Result<Vec<RecoveryWorktreeEntry>, GitError> {
        let mut entries = Vec::with_capacity(paths.len());
        let mut path_bytes = paths
            .iter()
            .map(|path| path.as_os_str().as_encoded_bytes().len())
            .sum::<usize>();
        let mut path_count = paths.len();
        while let Some(relative) = paths.pop_first() {
            let path = root.join(&relative);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                    entries.push(RecoveryWorktreeEntry {
                        path: relative.to_owned(),
                        value: RecoveryWorktreeValue::Missing,
                    });
                    continue;
                }
                Err(source) => return Err(recovery_metadata_io_error(&relative, source)),
            };
            let file_type = metadata.file_type();
            let value = if file_type.is_file() {
                let (file, opened_metadata) =
                    open_recovery_regular_file(&path, &relative, &metadata)?;
                let size = opened_metadata.len();
                capture.reserve_file_bytes(size)?;
                let file = BufReader::new(file);
                let mut file = file.take(size.saturating_add(1));
                let mut hasher = Sha256::new();
                let mut buffer = [0_u8; 64 * 1024];
                let mut bytes_read = 0_u64;
                loop {
                    let read = file
                        .read(&mut buffer)
                        .map_err(|source| recovery_metadata_io_error(&relative, source))?;
                    if read == 0 {
                        break;
                    }
                    bytes_read = bytes_read.saturating_add(read as u64);
                    if bytes_read > size {
                        return Err(GitError::Blocked {
                            message: format!(
                                "recovery is blocked because relevant file {} changed while it was fingerprinted",
                                relative.display()
                            ),
                        });
                    }
                    hasher.update(&buffer[..read]);
                }
                ensure_recovery_regular_file_path_unchanged(&path, &relative, &opened_metadata)?;
                RecoveryWorktreeValue::File {
                    size,
                    mode: recovery_file_mode(&opened_metadata),
                    identity: recovery_file_identity(&opened_metadata),
                    digest: hasher.finalize().into(),
                }
            } else if file_type.is_symlink() {
                RecoveryWorktreeValue::Symlink(
                    fs::read_link(&path)
                        .map_err(|source| recovery_metadata_io_error(&relative, source))?,
                )
            } else if file_type.is_dir() {
                let children = fs::read_dir(&path)
                    .map_err(|source| recovery_metadata_io_error(&relative, source))?;
                for child in children {
                    let child =
                        child.map_err(|source| recovery_metadata_io_error(&relative, source))?;
                    let child = relative.join(child.file_name());
                    if paths.insert(child.clone()) {
                        if path_count == MAX_RECOVERY_RELEVANT_FILES {
                            return Err(recovery_bound_error(format!(
                                "relevant file count exceeds {MAX_RECOVERY_RELEVANT_FILES}"
                            )));
                        }
                        path_count += 1;
                        let child_bytes = child.as_os_str().as_encoded_bytes().len();
                        path_bytes = path_bytes.saturating_add(child_bytes);
                        if path_bytes > MAX_RECOVERY_PATH_BYTES {
                            return Err(recovery_bound_error(format!(
                                "relevant path bytes exceed {MAX_RECOVERY_PATH_BYTES}"
                            )));
                        }
                        capture.retain(child_bytes.saturating_add(96), "relevant paths")?;
                    }
                }
                RecoveryWorktreeValue::Directory {
                    mode: recovery_file_mode(&metadata),
                    identity: recovery_file_identity(&metadata),
                }
            } else {
                return Err(GitError::Blocked {
                    message: format!(
                        "recovery is blocked because relevant path {} has an unsupported filesystem type",
                        relative.display()
                    ),
                });
            };
            entries.push(RecoveryWorktreeEntry {
                path: relative,
                value,
            });
        }
        capture.retain(entries.len() * 80, "worktree fingerprints")?;
        Ok(entries)
    }

    fn recovery_metadata(
        &self,
        git_dir: &Path,
        capture: &mut RecoveryCapture,
    ) -> Result<Vec<RecoveryMetadataEntry>, GitError> {
        snapshot_recovery_metadata(git_dir, capture)
    }

    fn recovery_refs(
        &self,
        operation: Option<RepositoryOperation>,
        metadata: &[RecoveryMetadataEntry],
        capture: &mut RecoveryCapture,
    ) -> Result<Vec<u8>, GitError> {
        if operation != Some(RepositoryOperation::Rebase) {
            return Ok(Vec::new());
        }

        let mut references = BTreeSet::new();
        for relative in ["rebase-merge/head-name", "rebase-apply/head-name"] {
            if let Some(reference) = recovery_metadata_file(metadata, relative) {
                let reference = strip_byte_line_ending(reference);
                if reference.starts_with(b"refs/") {
                    references.insert(path_from_bytes(reference).into_os_string());
                }
            }
        }
        for relative in ["rebase-merge/update-refs", "rebase-apply/update-refs"] {
            if let Some(contents) = recovery_metadata_file(metadata, relative) {
                references.extend(
                    contents
                        .split(|byte| *byte == b'\n')
                        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
                        .filter(|line| line.starts_with(b"refs/"))
                        .map(|line| path_from_bytes(line).into_os_string()),
                );
            }
        }

        let mut args = vec![
            OsString::from("for-each-ref"),
            OsString::from("--format=%(refname)%00%(objectname)%00%(symref)"),
            OsString::from("refs/rewritten"),
        ];
        args.extend(references);

        capture.required_os(self, args)
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

    fn run_args(&self, args: Vec<String>) -> Result<GitOutput, GitError> {
        self.run_args_with_editor(args, false)
    }

    #[cfg(test)]
    fn run_recovery_args(&self, args: Vec<String>) -> Result<GitOutput, GitError> {
        self.run_args_with_editor(args, true)
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

struct RecoveryTransaction {
    root: PathBuf,
    candidate: PathBuf,
    backup_pointer: PathBuf,
    baseline: RecoveryGeneration,
    keep_candidate: bool,
}

impl RecoveryTransaction {
    fn prepare(git: &Git) -> Result<Self, GitError> {
        let root = Self::supported_root(git)?;
        let parent = root.parent().ok_or_else(|| {
            recovery_transaction_blocked("repository root has no parent for isolated recovery")
        })?;
        remove_previous_recovery_backup(&root)?;
        let baseline = RecoveryGeneration::capture(&root)?;
        let (candidate, backup_identity) = create_recovery_candidate(parent, &root)?;
        let backup_pointer = recovery_backup_pointer_path(&candidate)?;
        let mut pointer_contents = candidate.as_os_str().as_encoded_bytes().to_vec();
        pointer_contents.push(0);
        pointer_contents.extend_from_slice(&backup_identity);
        let mut transaction = Self {
            root,
            candidate,
            backup_pointer,
            baseline,
            keep_candidate: false,
        };
        transaction.copy_repository()?;

        let live_after_copy = RecoveryGeneration::capture(&transaction.root)?;
        let copied = RecoveryGeneration::capture(&transaction.candidate)?;
        if live_after_copy != transaction.baseline || copied != transaction.baseline {
            return Err(recovery_transaction_blocked(
                "repository changed while isolated recovery state was prepared",
            ));
        }
        run_recovery_sidecar_hook(&recovery_backup_pointer_path(&transaction.root)?);
        write_new_recovery_sidecar(
            &transaction.backup_pointer,
            &pointer_contents,
            "record the retained recovery backup pointer",
        )?;
        Ok(transaction)
    }

    fn supported_root(git: &Git) -> Result<PathBuf, GitError> {
        let root = git
            .repo_root()?
            .canonicalize()
            .map_err(|source| recovery_transaction_io("resolve repository root", source))?;
        let git_dir = git
            .git_path("")?
            .canonicalize()
            .map_err(|source| recovery_transaction_io("resolve Git directory", source))?;
        let embedded_git_dir = root.join(".git");
        if !embedded_git_dir.is_dir()
            || embedded_git_dir.canonicalize().map_err(|source| {
                recovery_transaction_io("resolve embedded Git directory", source)
            })? != git_dir
        {
            return Err(recovery_transaction_blocked(
                "atomic recovery requires a standalone repository with an embedded .git directory",
            ));
        }
        let common_dir = path_from_bytes(strip_byte_line_ending(
            &git.run_raw(["rev-parse", "--path-format=absolute", "--git-common-dir"])?
                .stdout,
        ))
        .canonicalize()
        .map_err(|source| recovery_transaction_io("resolve common Git directory", source))?;
        if common_dir != git_dir {
            return Err(recovery_transaction_blocked(
                "atomic recovery does not support a shared common Git directory",
            ));
        }
        ensure_recovery_git_storage_isolated(&root)?;
        let parent = root.parent().ok_or_else(|| {
            recovery_transaction_blocked("repository root has no parent for isolated recovery")
        })?;
        run_recovery_capability_hook(&root)?;
        ensure_recovery_platform_capabilities(parent)?;
        Ok(root)
    }

    fn copy_repository(&mut self) -> Result<(), GitError> {
        let output = Command::new("cp")
            .args(["-a", "--reflink=auto", "--"])
            .arg(self.root.join("."))
            .arg(&self.candidate)
            .output()
            .map_err(|source| recovery_transaction_io("start isolated repository copy", source))?;
        if !output.status.success() {
            return Err(recovery_transaction_blocked(format!(
                "isolated repository copy failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        fs::set_permissions(
            &self.candidate,
            fs::metadata(&self.root)
                .map_err(|source| {
                    recovery_transaction_io("read repository root permissions", source)
                })?
                .permissions(),
        )
        .map_err(|source| {
            recovery_transaction_io("preserve repository root permissions", source)
        })?;
        Ok(())
    }

    fn run_recovery(&self, git: &Git, args: Vec<String>) -> Result<GitOutput, GitError> {
        let mut command = Command::new("unshare");
        command
            .args([
                "--user",
                "--map-root-user",
                "--mount",
                "--fork",
                "sh",
                "-c",
                "mount --bind \"$1\" \"$2\" || { printf '%s\\n' 'bitbygit: recovery namespace setup failed' >&2; exit 125; }; cd \"$2\" || { printf '%s\\n' 'bitbygit: recovery namespace setup failed' >&2; exit 125; }; shift 2; exec git \"$@\"",
                "bitbygit-recovery",
            ])
            .arg(&self.candidate)
            .arg(&self.root)
            .args(&args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("SSH_ASKPASS", "")
            .env("SSH_ASKPASS_REQUIRE", "never")
            .env("GIT_EDITOR", "true")
            .env("GIT_SEQUENCE_EDITOR", "true");
        for variable in RECOVERY_GIT_ENVIRONMENT {
            command.env_remove(variable);
        }
        if git.ssh_executable.is_some() || env::var_os("GIT_SSH_COMMAND").is_none() {
            let ssh_executable = git
                .ssh_executable
                .as_ref()
                .map(|path| shell_quote(&path.to_string_lossy()))
                .unwrap_or_else(|| "ssh".to_owned());
            command.env("GIT_SSH_COMMAND", format!("{ssh_executable} {SSH_OPTIONS}"));
        }
        let output = run_bounded_recovery_command(&mut command, args.clone())?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            if String::from_utf8_lossy(&output.stderr).lines().any(|line| {
                line.starts_with("unshare: ") || line == "bitbygit: recovery namespace setup failed"
            }) {
                return Err(recovery_capability_unavailable(format!(
                    "the recovery namespace could not be established: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
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

    fn has_advanced_rebase_conflict(&self, expected_state: &RecoveryState) -> bool {
        let candidate_git = Git::new(&self.candidate);
        let Ok(candidate_state) = candidate_git.recovery_state() else {
            return false;
        };
        let Ok(candidate_status) = candidate_git.status() else {
            return false;
        };
        candidate_state.operation == Some(RepositoryOperation::Rebase)
            && candidate_status.operation == Some(RepositoryOperation::Rebase)
            && !candidate_status.conflicted_files().is_empty()
            && candidate_state.rebase_progressed_from(expected_state)
    }

    fn promote(
        &mut self,
        live_git: &Git,
        expected_state: &RecoveryState,
    ) -> Result<PathBuf, GitError> {
        if live_git.recovery_state()? != *expected_state
            || RecoveryGeneration::capture(&self.root)? != self.baseline
        {
            return Err(recovery_transaction_blocked(
                "repository changed while recovery executed in isolation",
            ));
        }

        run_recovery_promotion_hook(&self.root);
        atomic_exchange_directories(&self.root, &self.candidate)?;
        self.keep_candidate = true;
        let old_generation = RecoveryGeneration::capture(&self.candidate);
        if !matches!(&old_generation, Ok(generation) if generation == &self.baseline) {
            if let Err(error) = atomic_exchange_directories(&self.root, &self.candidate) {
                return Err(recovery_transaction_blocked(format!(
                    "repository changed during atomic recovery promotion and rollback failed; both complete generations were retained: {error}"
                )));
            }
            self.keep_candidate = false;
            return Err(recovery_transaction_blocked(match old_generation {
                Ok(_) => "repository changed during atomic recovery promotion".to_owned(),
                Err(error) => {
                    format!("repository metadata changed during atomic recovery promotion: {error}")
                }
            }));
        }
        Ok(self.candidate.clone())
    }
}

impl Drop for RecoveryTransaction {
    fn drop(&mut self) {
        if !self.keep_candidate {
            let _result = fs::remove_dir_all(&self.candidate);
            let _result = fs::remove_file(recovery_candidate_owner_path(&self.candidate));
            let _result = fs::remove_file(&self.backup_pointer);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RecoveryGeneration {
    root: RecoveryFilesystemMetadata,
    entries: Vec<RecoveryGenerationEntry>,
}

impl RecoveryGeneration {
    fn capture(root: &Path) -> Result<Self, GitError> {
        let root_metadata = fs::symlink_metadata(root)
            .map_err(|source| recovery_transaction_io("inspect repository root", source))?;
        if !root_metadata.file_type().is_dir() {
            return Err(recovery_transaction_blocked(
                "repository root changed while its generation was captured",
            ));
        }
        let root_value = recovery_filesystem_metadata(root, Path::new("."), &root_metadata, None)?;
        let mut entries = Vec::new();
        let mut pending = vec![PathBuf::new()];
        let mut path_bytes = 0_usize;
        let mut file_bytes = 0_u64;
        while let Some(relative_dir) = pending.pop() {
            let directory = root.join(&relative_dir);
            let mut children = fs::read_dir(&directory)
                .map_err(|source| recovery_transaction_io("read repository generation", source))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|source| recovery_transaction_io("read repository generation", source))?;
            children.sort_by_key(|entry| entry.file_name());
            for child in children.into_iter().rev() {
                let relative = relative_dir.join(child.file_name());
                path_bytes =
                    path_bytes.saturating_add(relative.as_os_str().as_encoded_bytes().len());
                if entries.len() == MAX_RECOVERY_GENERATION_ENTRIES
                    || path_bytes > MAX_RECOVERY_GENERATION_PATH_BYTES
                {
                    return Err(recovery_transaction_blocked(
                        "repository generation exceeds atomic recovery entry or path bounds",
                    ));
                }
                let path = child.path();
                let metadata = fs::symlink_metadata(&path).map_err(|source| {
                    recovery_transaction_io("inspect repository generation entry", source)
                })?;
                let file_type = metadata.file_type();
                let value = if file_type.is_dir() {
                    pending.push(relative.clone());
                    RecoveryGenerationValue::Directory {
                        metadata: recovery_filesystem_metadata(&path, &relative, &metadata, None)?,
                    }
                } else if file_type.is_file() {
                    let (mut file, opened_metadata) =
                        open_recovery_regular_file(&path, &relative, &metadata)?;
                    file_bytes = file_bytes.saturating_add(opened_metadata.len());
                    if file_bytes > MAX_RECOVERY_GENERATION_FILE_BYTES {
                        return Err(recovery_transaction_blocked(
                            "repository generation exceeds atomic recovery content bound",
                        ));
                    }
                    let file_metadata = recovery_filesystem_metadata(
                        &path,
                        &relative,
                        &opened_metadata,
                        Some(&file),
                    )?;
                    run_recovery_generation_file_read_hook(&path);
                    let mut hasher = Sha256::new();
                    {
                        let mut reader = (&mut file).take(opened_metadata.len().saturating_add(1));
                        let mut buffer = [0_u8; 64 * 1024];
                        let mut bytes_read = 0_u64;
                        loop {
                            let read = reader.read(&mut buffer).map_err(|source| {
                                recovery_transaction_io("hash repository generation file", source)
                            })?;
                            if read == 0 {
                                break;
                            }
                            bytes_read = bytes_read.saturating_add(read as u64);
                            if bytes_read > opened_metadata.len() {
                                return Err(recovery_transaction_blocked(format!(
                                    "repository generation file {} grew while it was captured",
                                    relative.display()
                                )));
                            }
                            hasher.update(&buffer[..read]);
                        }
                    }
                    let current = fs::symlink_metadata(&path).map_err(|source| {
                        recovery_transaction_io("recheck repository generation file", source)
                    })?;
                    if !current.is_file() || current.len() != opened_metadata.len() {
                        return Err(recovery_transaction_blocked(
                            "repository generation changed while it was captured",
                        ));
                    }
                    let current_metadata =
                        recovery_filesystem_metadata(&path, &relative, &current, Some(&file))?;
                    if current_metadata != file_metadata {
                        return Err(recovery_transaction_blocked(
                            "repository generation changed while it was captured",
                        ));
                    }
                    RecoveryGenerationValue::File {
                        metadata: file_metadata,
                        size: opened_metadata.len(),
                        digest: hasher.finalize().into(),
                    }
                } else if file_type.is_symlink() {
                    if relative.starts_with(".git") {
                        return Err(recovery_transaction_blocked(format!(
                            "atomic recovery does not support symlinked Git storage {}",
                            relative.display()
                        )));
                    }
                    RecoveryGenerationValue::Symlink {
                        metadata: recovery_filesystem_metadata(&path, &relative, &metadata, None)?,
                        target: fs::read_link(&path).map_err(|source| {
                            recovery_transaction_io("read repository generation symlink", source)
                        })?,
                    }
                } else {
                    return Err(recovery_transaction_blocked(format!(
                        "atomic recovery does not support special filesystem entry {}",
                        relative.display()
                    )));
                };
                entries.push(RecoveryGenerationEntry {
                    path: relative,
                    value,
                });
            }
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let current_root = fs::symlink_metadata(root)
            .map_err(|source| recovery_transaction_io("recheck repository root", source))?;
        let current_root_value =
            recovery_filesystem_metadata(root, Path::new("."), &current_root, None)?;
        if current_root_value != root_value {
            return Err(recovery_transaction_blocked(
                "repository root changed while its generation was captured",
            ));
        }
        Ok(Self {
            root: root_value,
            entries,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RecoveryGenerationEntry {
    path: PathBuf,
    value: RecoveryGenerationValue,
}

#[derive(Debug, PartialEq, Eq)]
enum RecoveryGenerationValue {
    Directory {
        metadata: RecoveryFilesystemMetadata,
    },
    File {
        metadata: RecoveryFilesystemMetadata,
        size: u64,
        digest: [u8; 32],
    },
    Symlink {
        metadata: RecoveryFilesystemMetadata,
        target: PathBuf,
    },
}

#[derive(Debug, PartialEq, Eq)]
struct RecoveryFilesystemMetadata {
    mode: u32,
    owner: (u32, u32),
    modified: (i64, i64),
    inode_flags: u32,
}

fn recovery_filesystem_metadata(
    path: &Path,
    relative: &Path,
    metadata: &fs::Metadata,
    opened: Option<&fs::File>,
) -> Result<RecoveryFilesystemMetadata, GitError> {
    ensure_no_recovery_extended_attributes(path, relative)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.file_type().is_file() && metadata.nlink() != 1 {
            return Err(recovery_transaction_blocked(format!(
                "atomic recovery does not support hard-linked file {}",
                relative.display()
            )));
        }
        Ok(RecoveryFilesystemMetadata {
            mode: recovery_file_mode(metadata),
            owner: (metadata.uid(), metadata.gid()),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            inode_flags: recovery_inode_flags(path, relative, metadata, opened)?,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (path, relative, opened);
        Ok(RecoveryFilesystemMetadata {
            mode: recovery_file_mode(metadata),
            owner: (0, 0),
            modified: (0, 0),
            inode_flags: 0,
        })
    }
}

#[cfg(target_os = "linux")]
fn recovery_inode_flags(
    path: &Path,
    relative: &Path,
    expected: &fs::Metadata,
    opened: Option<&fs::File>,
) -> Result<u32, GitError> {
    use std::os::unix::fs::OpenOptionsExt;

    let owned;
    let file = if let Some(file) = opened {
        file
    } else if expected.file_type().is_symlink() {
        return Ok(0);
    } else {
        owned = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|source| {
                recovery_transaction_blocked(format!(
                    "repository metadata {} changed while it was opened: {source}",
                    relative.display()
                ))
            })?;
        let current = owned.metadata().map_err(|source| {
            recovery_transaction_io("inspect opened repository metadata", source)
        })?;
        if recovery_file_identity(&current) != recovery_file_identity(expected)
            || current.file_type() != expected.file_type()
        {
            return Err(recovery_transaction_blocked(format!(
                "repository metadata {} changed while it was opened",
                relative.display()
            )));
        }
        &owned
    };
    Ok(rustix::fs::ioctl_getflags(file)
        .map(|flags| flags.bits() & rustix::fs::IFlags::all().bits())
        .unwrap_or(0))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn recovery_inode_flags(
    _path: &Path,
    _relative: &Path,
    _expected: &fs::Metadata,
    _opened: Option<&fs::File>,
) -> Result<u32, GitError> {
    Ok(0)
}

#[cfg(target_os = "linux")]
fn ensure_no_recovery_extended_attributes(path: &Path, relative: &Path) -> Result<(), GitError> {
    let mut attributes = xattr::list(path).map_err(|source| {
        recovery_transaction_io("inspect repository extended attributes", source)
    })?;
    if attributes.next().is_some() {
        return Err(recovery_transaction_blocked(format!(
            "atomic recovery does not support extended attributes or ACLs on {}",
            relative.display()
        )));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn ensure_no_recovery_extended_attributes(_path: &Path, _relative: &Path) -> Result<(), GitError> {
    Ok(())
}

fn ensure_recovery_git_storage_isolated(root: &Path) -> Result<(), GitError> {
    let git_dir = root.join(".git");
    let mut pending = vec![git_dir];
    let mut entries = 0_usize;
    let mut path_bytes = 0_usize;
    while let Some(directory) = pending.pop() {
        for child in fs::read_dir(&directory)
            .map_err(|source| recovery_transaction_io("inspect Git storage", source))?
        {
            let child =
                child.map_err(|source| recovery_transaction_io("inspect Git storage", source))?;
            let path = child.path();
            entries = entries.saturating_add(1);
            path_bytes = path_bytes.saturating_add(path.as_os_str().as_encoded_bytes().len());
            if entries > MAX_RECOVERY_GENERATION_ENTRIES
                || path_bytes > MAX_RECOVERY_GENERATION_PATH_BYTES
            {
                return Err(recovery_transaction_blocked(
                    "Git storage exceeds atomic recovery entry or path bounds",
                ));
            }
            let metadata = fs::symlink_metadata(&path)
                .map_err(|source| recovery_transaction_io("inspect Git storage", source))?;
            if metadata.file_type().is_symlink() {
                return Err(recovery_transaction_blocked(format!(
                    "atomic recovery does not support symlinked Git storage {}",
                    path.strip_prefix(root).unwrap_or(&path).display()
                )));
            }
            if metadata.file_type().is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(())
}

fn write_new_recovery_sidecar(
    path: impl AsRef<Path>,
    contents: &[u8],
    action: &str,
) -> Result<(), GitError> {
    let path = path.as_ref();
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|source| recovery_transaction_io(action, source))?;
    if let Err(source) = file.write_all(contents) {
        let _ = fs::remove_file(path);
        return Err(recovery_transaction_io(action, source));
    }
    Ok(())
}

fn create_recovery_candidate(parent: &Path, root: &Path) -> Result<(PathBuf, Vec<u8>), GitError> {
    for attempt in 0..100_u32 {
        let candidate = parent.join(format!(
            "{RECOVERY_CANDIDATE_PREFIX}{}-{}-{attempt}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => {
                let root_identity =
                    recovery_file_identity(&fs::symlink_metadata(root).map_err(|source| {
                        recovery_transaction_io("identify repository generation", source)
                    })?);
                let candidate_identity =
                    recovery_file_identity(&fs::symlink_metadata(&candidate).map_err(
                        |source| recovery_transaction_io("identify isolated repository", source),
                    )?);
                let mut identity = Sha256::new();
                identity.update(root.as_os_str().as_encoded_bytes());
                identity.update(candidate.as_os_str().as_encoded_bytes());
                for value in [
                    root_identity.0,
                    root_identity.1,
                    candidate_identity.0,
                    candidate_identity.1,
                ] {
                    identity.update(value.to_le_bytes());
                }
                let mut backup_identity = Vec::with_capacity(48);
                backup_identity.extend_from_slice(&root_identity.0.to_le_bytes());
                backup_identity.extend_from_slice(&root_identity.1.to_le_bytes());
                backup_identity.extend_from_slice(&identity.finalize());
                if let Err(error) = write_new_recovery_sidecar(
                    recovery_candidate_owner_path(&candidate),
                    &backup_identity,
                    "record isolated repository ownership",
                ) {
                    let _result = fs::remove_dir(&candidate);
                    return Err(error);
                }
                return Ok((candidate, backup_identity));
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(recovery_transaction_io(
                    "create isolated repository",
                    source,
                ));
            }
        }
    }
    Err(recovery_transaction_blocked(
        "could not reserve an isolated repository path",
    ))
}

fn recovery_candidate_owner_path(candidate: &Path) -> PathBuf {
    let mut owner = candidate.as_os_str().to_os_string();
    owner.push(".owner");
    PathBuf::from(owner)
}

fn recovery_backup_pointer_path(root: &Path) -> Result<PathBuf, GitError> {
    Ok(root.join(".git").join(RECOVERY_BACKUP_POINTER))
}

fn recovery_backup_from_pointer(expected_root: &Path) -> Result<Option<PathBuf>, GitError> {
    let pointer = recovery_backup_pointer_path(expected_root)?;
    let bytes = match fs::read(&pointer) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(recovery_transaction_io(
                "read the retained recovery backup pointer",
                source,
            ));
        }
    };
    let Some(separator) = bytes.iter().position(|byte| *byte == 0) else {
        return Err(recovery_transaction_blocked(
            "the retained recovery backup pointer is invalid; refusing to remove it",
        ));
    };
    let backup = path_from_bytes(&bytes[..separator]);
    let identity = &bytes[separator + 1..];
    let valid_location = backup.is_absolute()
        && backup.file_name().is_some_and(|name| {
            name.as_encoded_bytes()
                .starts_with(RECOVERY_CANDIDATE_PREFIX.as_bytes())
        });
    let backup_metadata = fs::symlink_metadata(&backup);
    let valid_directory = backup_metadata.as_ref().is_ok_and(|metadata| {
        metadata.file_type().is_dir()
            && identity.len() >= 16
            && recovery_file_identity(metadata).0.to_le_bytes() == identity[..8]
            && recovery_file_identity(metadata).1.to_le_bytes() == identity[8..16]
    });
    let missing_directory = backup_metadata
        .as_ref()
        .is_err_and(|source| source.kind() == std::io::ErrorKind::NotFound);
    let owner = recovery_candidate_owner_path(&backup);
    let valid_owner = fs::symlink_metadata(&owner)
        .is_ok_and(|metadata| metadata.file_type().is_file())
        && fs::read(&owner).is_ok_and(|contents| contents == identity);
    if !valid_location || (!valid_directory && !missing_directory) || !valid_owner {
        return Err(recovery_transaction_blocked(
            "the retained recovery backup pointer is invalid; refusing to remove it",
        ));
    }
    Ok(Some(backup))
}

fn remove_previous_recovery_backup(root: &Path) -> Result<(), GitError> {
    let Some(backup) = recovery_backup_from_pointer(root)? else {
        return Ok(());
    };
    if let Err(source) = fs::remove_dir_all(&backup)
        && source.kind() != std::io::ErrorKind::NotFound
    {
        return Err(recovery_transaction_io(
            "remove the previous retained recovery backup",
            source,
        ));
    }
    fs::remove_file(recovery_backup_pointer_path(root)?)
        .map_err(|source| recovery_transaction_io("remove the previous backup pointer", source))?;
    let _result = fs::remove_file(recovery_candidate_owner_path(&backup));
    Ok(())
}

fn ensure_recovery_environment_isolated() -> Result<(), GitError> {
    if let Some(variable) = RECOVERY_GIT_ENVIRONMENT
        .iter()
        .find(|variable| env::var_os(variable).is_some())
    {
        return Err(recovery_transaction_blocked(format!(
            "atomic recovery does not support inherited {variable} repository state"
        )));
    }
    Ok(())
}

fn append_recovery_backup_notice(output: &mut String, backup: &Path) {
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&format!(
        "bitbygit: previous repository generation retained at {}; it will be removed before the next recovery attempt\n",
        backup.display()
    ));
}

#[cfg(target_os = "linux")]
fn ensure_recovery_platform_capabilities(parent: &Path) -> Result<(), GitError> {
    let source = create_recovery_probe_directory(parent)?;
    let target = match create_recovery_probe_directory(parent) {
        Ok(target) => target,
        Err(error) => {
            let _ = fs::remove_dir(&source);
            return Err(error);
        }
    };
    let result = (|| {
        let output = Command::new("unshare")
            .args([
                "--user",
                "--map-root-user",
                "--mount",
                "--fork",
                "sh",
                "-c",
                "mount --bind \"$1\" \"$2\"",
                "bitbygit-recovery-capability",
            ])
            .arg(&source)
            .arg(&target)
            .output()
            .map_err(|source| {
                recovery_capability_unavailable(format!(
                    "the Linux user/mount namespace probe could not start: {source}"
                ))
            })?;
        if !output.status.success() {
            let detail = if output.stderr.is_empty() {
                format!("status {}", output.status)
            } else {
                String::from_utf8_lossy(&output.stderr).trim().to_owned()
            };
            return Err(recovery_capability_unavailable(format!(
                "the Linux user/mount namespace probe failed: {detail}"
            )));
        }
        atomic_exchange_directories(&source, &target).map_err(|error| {
            recovery_capability_unavailable(format!(
                "same-filesystem atomic directory exchange is not available: {error}"
            ))
        })
    })();
    let _ = fs::remove_dir_all(&source);
    let _ = fs::remove_dir_all(&target);
    result
}

#[cfg(not(target_os = "linux"))]
fn ensure_recovery_platform_capabilities(_parent: &Path) -> Result<(), GitError> {
    Err(recovery_capability_unavailable(
        "Linux user/mount namespaces and atomic directory exchange are required",
    ))
}

fn create_recovery_probe_directory(parent: &Path) -> Result<PathBuf, GitError> {
    for attempt in 0..100_u32 {
        let candidate = parent.join(format!(
            ".bitbygit-recovery-capability-{}-{}-{attempt}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(recovery_capability_unavailable(format!(
                    "a same-filesystem capability probe directory could not be created: {source}"
                )));
            }
        }
    }
    Err(recovery_capability_unavailable(
        "a same-filesystem capability probe directory could not be reserved",
    ))
}

#[cfg(target_os = "linux")]
fn atomic_exchange_directories(left: &Path, right: &Path) -> Result<(), GitError> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};

    renameat_with(CWD, left, CWD, right, RenameFlags::EXCHANGE).map_err(|source| {
        recovery_transaction_blocked(format!("atomic directory exchange failed: {source}"))
    })
}

#[cfg(not(target_os = "linux"))]
fn atomic_exchange_directories(_left: &Path, _right: &Path) -> Result<(), GitError> {
    Err(recovery_transaction_blocked(
        "atomic recovery is not supported on this platform",
    ))
}

fn recovery_transaction_io(action: &str, source: std::io::Error) -> GitError {
    recovery_transaction_blocked(format!("atomic recovery could not {action}: {source}"))
}

fn recovery_transaction_blocked(message: impl Into<String>) -> GitError {
    GitError::Blocked {
        message: message.into(),
    }
}

fn recovery_capability_unavailable(detail: impl Display) -> GitError {
    recovery_transaction_blocked(format!("{RECOVERY_CAPABILITY_UNAVAILABLE}: {detail}"))
}

#[cfg(test)]
static RECOVERY_CAPABILITY_FAILURES: std::sync::Mutex<Vec<PathBuf>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn run_recovery_capability_hook(root: &Path) -> Result<(), GitError> {
    if let Ok(mut failures) = RECOVERY_CAPABILITY_FAILURES.lock()
        && let Some(index) = failures.iter().position(|target| target == root)
    {
        failures.swap_remove(index);
        return Err(recovery_capability_unavailable(
            "test platform capability failure",
        ));
    }
    Ok(())
}

#[cfg(not(test))]
fn run_recovery_capability_hook(_root: &Path) -> Result<(), GitError> {
    Ok(())
}

#[cfg(test)]
static RECOVERY_PROMOTION_HOOKS: std::sync::Mutex<Vec<(PathBuf, RecoveryCaptureHook)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn run_recovery_promotion_hook(root: &Path) {
    if let Ok(mut hooks) = RECOVERY_PROMOTION_HOOKS.lock()
        && let Some(index) = hooks.iter().position(|(target, _)| target == root)
    {
        let (_, hook) = hooks.swap_remove(index);
        hook();
    }
}

#[cfg(not(test))]
fn run_recovery_promotion_hook(_root: &Path) {}

#[cfg(test)]
static RECOVERY_SIDECAR_HOOKS: std::sync::Mutex<Vec<(PathBuf, RecoveryCaptureHook)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn run_recovery_sidecar_hook(path: &Path) {
    if let Ok(mut hooks) = RECOVERY_SIDECAR_HOOKS.lock()
        && let Some(index) = hooks.iter().position(|(target, _)| target == path)
    {
        let (_, hook) = hooks.swap_remove(index);
        hook();
    }
}

#[cfg(not(test))]
fn run_recovery_sidecar_hook(_path: &Path) {}

#[derive(Default)]
struct RecoveryCapture {
    output_bytes: usize,
    retained_bytes: usize,
    file_bytes_read: u64,
    metadata_bytes: usize,
    metadata_entries: usize,
    subprocesses: usize,
}

impl RecoveryCapture {
    fn required(&mut self, git: &Git, args: &[&str]) -> Result<Vec<u8>, GitError> {
        self.command(git, args.iter().map(OsString::from).collect(), false)?
            .ok_or_else(|| GitError::Parse {
                message: "required recovery guard command returned no output".to_owned(),
            })
    }

    fn optional(&mut self, git: &Git, args: &[&str]) -> Result<Option<Vec<u8>>, GitError> {
        self.command(git, args.iter().map(OsString::from).collect(), true)
    }

    fn required_os(&mut self, git: &Git, args: Vec<OsString>) -> Result<Vec<u8>, GitError> {
        self.command(git, args, false)?
            .ok_or_else(|| GitError::Parse {
                message: "required recovery guard command returned no output".to_owned(),
            })
    }

    fn command(
        &mut self,
        git: &Git,
        args: Vec<OsString>,
        allow_missing: bool,
    ) -> Result<Option<Vec<u8>>, GitError> {
        self.subprocesses += 1;
        if self.subprocesses > MAX_RECOVERY_SUBPROCESSES {
            return Err(recovery_bound_error(format!(
                "subprocess count exceeds {MAX_RECOVERY_SUBPROCESSES}"
            )));
        }
        let display_args = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let mut command = Command::new("git");
        command
            .current_dir(&git.cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("SSH_ASKPASS", "")
            .env("SSH_ASKPASS_REQUIRE", "never")
            .args(&args);
        let output = run_bounded_recovery_command(&mut command, display_args.clone())?;
        let bytes = output.stdout.len().saturating_add(output.stderr.len());
        self.output_bytes = self.output_bytes.saturating_add(bytes);
        if self.output_bytes > MAX_RECOVERY_OUTPUT_BYTES {
            return Err(recovery_bound_error(format!(
                "Git output bytes exceed {MAX_RECOVERY_OUTPUT_BYTES}"
            )));
        }
        if output.status.success() {
            return Ok(Some(output.stdout));
        }
        if allow_missing && output.status.code() == Some(1) {
            return Ok(None);
        }
        Err(GitError::GitFailed {
            args: display_args,
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn retain(&mut self, bytes: usize, description: &str) -> Result<(), GitError> {
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        if self.retained_bytes > MAX_RECOVERY_RETAINED_BYTES {
            return Err(recovery_bound_error(format!(
                "retained memory for {description} exceeds {MAX_RECOVERY_RETAINED_BYTES} bytes"
            )));
        }
        Ok(())
    }

    fn reserve_file_bytes(&mut self, bytes: u64) -> Result<(), GitError> {
        self.file_bytes_read = self.file_bytes_read.saturating_add(bytes);
        if self.file_bytes_read > MAX_RECOVERY_FILE_BYTES_READ {
            return Err(recovery_bound_error(format!(
                "file content bytes read exceed {MAX_RECOVERY_FILE_BYTES_READ}"
            )));
        }
        Ok(())
    }

    fn reserve_metadata_entry(&mut self) -> Result<(), GitError> {
        if self.metadata_entries == MAX_RECOVERY_METADATA_ENTRIES {
            return Err(recovery_bound_error(format!(
                "metadata entries exceed {MAX_RECOVERY_METADATA_ENTRIES}"
            )));
        }
        self.metadata_entries += 1;
        Ok(())
    }

    fn reserve_metadata_bytes(&mut self, bytes: usize) -> Result<(), GitError> {
        self.metadata_bytes = self.metadata_bytes.saturating_add(bytes);
        if self.metadata_bytes > MAX_RECOVERY_METADATA_BYTES {
            return Err(recovery_bound_error(format!(
                "metadata bytes exceed {MAX_RECOVERY_METADATA_BYTES}"
            )));
        }
        self.retain(bytes.saturating_add(80), "recovery metadata")
    }
}

fn run_bounded_recovery_command(
    command: &mut Command,
    args: Vec<String>,
) -> Result<RawProcessOutput, GitError> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|source| GitError::Io {
        args: args.clone(),
        source,
    })?;
    let stdout = child.stdout.take().ok_or_else(|| GitError::Io {
        args: args.clone(),
        source: std::io::Error::other("recovery command could not capture stdout"),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| GitError::Io {
        args: args.clone(),
        source: std::io::Error::other("recovery command could not capture stderr"),
    })?;
    let output_bytes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stdout_bytes = std::sync::Arc::clone(&output_bytes);
    let stderr_bytes = std::sync::Arc::clone(&output_bytes);
    let stdout_reader =
        std::thread::spawn(move || read_bounded_recovery_output(stdout, stdout_bytes));
    let stderr_reader =
        std::thread::spawn(move || read_bounded_recovery_output(stderr, stderr_bytes));
    let status = loop {
        if output_bytes.load(std::sync::atomic::Ordering::Relaxed) > MAX_RECOVERY_OUTPUT_BYTES {
            let _ = child.kill();
            break child.wait();
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(1)),
            Err(source) => break Err(source),
        }
    }
    .map_err(|source| GitError::Io {
        args: args.clone(),
        source,
    })?;
    let stdout = stdout_reader
        .join()
        .map_err(|_| GitError::Io {
            args: args.clone(),
            source: std::io::Error::other("recovery stdout reader panicked"),
        })?
        .map_err(|source| GitError::Io {
            args: args.clone(),
            source,
        })?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| GitError::Io {
            args: args.clone(),
            source: std::io::Error::other("recovery stderr reader panicked"),
        })?
        .map_err(|source| GitError::Io {
            args: args.clone(),
            source,
        })?;
    if output_bytes.load(std::sync::atomic::Ordering::Relaxed) > MAX_RECOVERY_OUTPUT_BYTES {
        return Err(recovery_bound_error(format!(
            "Git output bytes exceed {MAX_RECOVERY_OUTPUT_BYTES}"
        )));
    }
    Ok(RawProcessOutput {
        args,
        status,
        stdout,
        stderr,
    })
}

fn read_bounded_recovery_output(
    mut stream: impl Read,
    total: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> std::io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Ok(retained);
        }
        let previous = total.fetch_add(read, std::sync::atomic::Ordering::Relaxed);
        if previous <= MAX_RECOVERY_OUTPUT_BYTES {
            let keep = read.min(MAX_RECOVERY_OUTPUT_BYTES + 1 - previous);
            retained.extend_from_slice(&buffer[..keep]);
        }
        if previous.saturating_add(read) > MAX_RECOVERY_OUTPUT_BYTES {
            return Ok(retained);
        }
    }
}

fn recovery_bound_error(detail: String) -> GitError {
    GitError::Blocked {
        message: format!("recovery is blocked because the recovery guard {detail}"),
    }
}

fn parse_recovery_changed_paths(
    output: &[u8],
    paths: &mut BTreeSet<PathBuf>,
) -> Result<(), GitError> {
    let mut expected_paths = 0;
    for record in output.split(|byte| *byte == 0) {
        if expected_paths > 0 {
            if paths.len() == MAX_RECOVERY_RELEVANT_FILES {
                return Err(recovery_bound_error(format!(
                    "relevant file count exceeds {MAX_RECOVERY_RELEVANT_FILES}"
                )));
            }
            paths.insert(path_from_bytes(record));
            expected_paths -= 1;
            continue;
        }
        let header = record.strip_prefix(b"\n").unwrap_or(record);
        if !header.starts_with(b":") {
            continue;
        }
        let status = header
            .rsplit(|byte| *byte == b' ')
            .next()
            .and_then(|status| status.first())
            .ok_or_else(|| GitError::Parse {
                message: "recovery changed-path record has no status".to_owned(),
            })?;
        expected_paths = usize::from(matches!(status, b'R' | b'C')) + 1;
    }
    if expected_paths != 0 {
        return Err(GitError::Parse {
            message: "recovery changed-path output ended before its path".to_owned(),
        });
    }
    Ok(())
}

fn recovery_operation_at(git_dir: &Path) -> Option<RepositoryOperation> {
    let rebase_apply = git_dir.join("rebase-apply");
    if git_dir.join("rebase-merge").exists()
        || (rebase_apply.exists() && !rebase_apply.join("applying").exists())
    {
        Some(RepositoryOperation::Rebase)
    } else if git_dir.join("MERGE_HEAD").exists() {
        Some(RepositoryOperation::Merge)
    } else {
        None
    }
}

#[cfg(unix)]
fn recovery_file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn recovery_file_mode(metadata: &fs::Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
}

#[cfg(unix)]
fn recovery_file_identity(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;

    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn recovery_file_identity(_metadata: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}

fn open_recovery_regular_file(
    path: &Path,
    relative: &Path,
    expected: &fs::Metadata,
) -> Result<(fs::File, fs::Metadata), GitError> {
    run_recovery_file_open_hook(path);

    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;

        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
    };
    #[cfg(not(unix))]
    let file = fs::File::open(path);

    let file = file.map_err(|source| GitError::Blocked {
        message: format!(
            "recovery is blocked because regular file {} changed before it could be opened: {source}",
            relative.display()
        ),
    })?;
    let opened = file
        .metadata()
        .map_err(|source| recovery_metadata_io_error(relative, source))?;
    if !opened.file_type().is_file()
        || opened.len() != expected.len()
        || recovery_file_mode(&opened) != recovery_file_mode(expected)
        || recovery_file_identity(&opened) != recovery_file_identity(expected)
    {
        return Err(GitError::Blocked {
            message: format!(
                "recovery is blocked because regular file {} changed while it was opened",
                relative.display()
            ),
        });
    }
    Ok((file, opened))
}

fn ensure_recovery_regular_file_path_unchanged(
    path: &Path,
    relative: &Path,
    opened: &fs::Metadata,
) -> Result<(), GitError> {
    let current = fs::symlink_metadata(path).map_err(|source| GitError::Blocked {
        message: format!(
            "recovery is blocked because regular file {} changed while it was read: {source}",
            relative.display()
        ),
    })?;
    if !current.file_type().is_file()
        || current.len() != opened.len()
        || recovery_file_mode(&current) != recovery_file_mode(opened)
        || recovery_file_identity(&current) != recovery_file_identity(opened)
    {
        return Err(GitError::Blocked {
            message: format!(
                "recovery is blocked because regular file {} changed while it was read",
                relative.display()
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
type RecoveryCaptureHook = Box<dyn FnOnce() + Send>;

#[cfg(test)]
static RECOVERY_CAPTURE_HOOK: std::sync::Mutex<Option<(PathBuf, RecoveryCaptureHook)>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn run_recovery_capture_hook(cwd: &Path) {
    if let Ok(mut hook) = RECOVERY_CAPTURE_HOOK.lock() {
        if hook.as_ref().is_some_and(|(target, _)| target == cwd) {
            let Some((_, hook)) = hook.take() else {
                return;
            };
            hook();
        }
    }
}

#[cfg(not(test))]
fn run_recovery_capture_hook(_cwd: &Path) {}

#[cfg(test)]
static RECOVERY_FILE_OPEN_HOOKS: std::sync::Mutex<Vec<(PathBuf, RecoveryCaptureHook)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn run_recovery_file_open_hook(path: &Path) {
    if let Ok(mut hooks) = RECOVERY_FILE_OPEN_HOOKS.lock() {
        if let Some(index) = hooks.iter().position(|(target, _)| target == path) {
            let (_, hook) = hooks.swap_remove(index);
            hook();
        }
    }
}

#[cfg(not(test))]
fn run_recovery_file_open_hook(_path: &Path) {}

#[cfg(test)]
static RECOVERY_GENERATION_FILE_READ_HOOKS: std::sync::Mutex<Vec<(PathBuf, RecoveryCaptureHook)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn run_recovery_generation_file_read_hook(path: &Path) {
    if let Ok(mut hooks) = RECOVERY_GENERATION_FILE_READ_HOOKS.lock()
        && let Some(index) = hooks.iter().position(|(target, _)| target == path)
    {
        let (_, hook) = hooks.swap_remove(index);
        hook();
    }
}

#[cfg(not(test))]
fn run_recovery_generation_file_read_hook(_path: &Path) {}

fn recovery_metadata_file<'a>(
    metadata: &'a [RecoveryMetadataEntry],
    relative: &str,
) -> Option<&'a [u8]> {
    metadata.iter().find_map(|entry| {
        if entry.path == Path::new(relative) {
            if let RecoveryMetadataValue::File(contents) = &entry.value {
                return Some(contents.as_slice());
            }
        }
        None
    })
}

fn rebase_original_head(metadata: &[RecoveryMetadataEntry]) -> Result<String, GitError> {
    let backends = ["rebase-merge", "rebase-apply"]
        .into_iter()
        .filter(|backend| {
            metadata.iter().any(|entry| {
                entry.path == Path::new(backend) && entry.value == RecoveryMetadataValue::Directory
            })
        })
        .collect::<Vec<_>>();
    let [backend] = backends.as_slice() else {
        return Err(GitError::Blocked {
            message: "recovery is blocked because the active rebase backend is ambiguous"
                .to_owned(),
        });
    };
    let relative = format!("{backend}/orig-head");
    let Some(contents) = recovery_metadata_file(metadata, &relative) else {
        return Err(GitError::Blocked {
            message: format!("recovery is blocked because {relative} is unavailable"),
        });
    };
    let oid = strip_byte_line_ending(contents);
    if !matches!(oid.len(), 40 | 64) || !oid.iter().all(u8::is_ascii_hexdigit) {
        return Err(GitError::Blocked {
            message: format!("recovery is blocked because {relative} is not a valid object id"),
        });
    }
    Ok(String::from_utf8_lossy(oid).into_owned())
}

fn snapshot_recovery_metadata(
    git_dir: &Path,
    capture: &mut RecoveryCapture,
) -> Result<Vec<RecoveryMetadataEntry>, GitError> {
    let mut entries = Vec::new();
    let mut pending = Vec::new();
    for relative in RECOVERY_METADATA_PATHS.iter().rev() {
        capture.reserve_metadata_entry()?;
        pending.push((PathBuf::from(relative), git_dir.join(relative)));
    }

    while let Some((relative, path)) = pending.pop() {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                capture.reserve_metadata_bytes(relative.as_os_str().as_encoded_bytes().len())?;
                entries.push(RecoveryMetadataEntry {
                    path: relative,
                    value: RecoveryMetadataValue::Missing,
                });
                continue;
            }
            Err(source) => return Err(recovery_metadata_io_error(&relative, source)),
        };
        let file_type = metadata.file_type();
        let value = if file_type.is_dir() {
            capture.reserve_metadata_bytes(relative.as_os_str().as_encoded_bytes().len())?;
            RecoveryMetadataValue::Directory
        } else if file_type.is_file() {
            let maximum = MAX_RECOVERY_METADATA_BYTES.saturating_sub(capture.metadata_bytes);
            let mut contents = Vec::new();
            let (file, opened_metadata) = open_recovery_regular_file(&path, &relative, &metadata)?;
            file.take(maximum.saturating_add(1) as u64)
                .read_to_end(&mut contents)
                .map_err(|source| recovery_metadata_io_error(&relative, source))?;
            ensure_recovery_regular_file_path_unchanged(&path, &relative, &opened_metadata)?;
            if contents.len() > maximum {
                return Err(recovery_bound_error(format!(
                    "metadata bytes exceed {MAX_RECOVERY_METADATA_BYTES}"
                )));
            }
            capture.reserve_metadata_bytes(
                relative
                    .as_os_str()
                    .as_encoded_bytes()
                    .len()
                    .saturating_add(contents.len()),
            )?;
            RecoveryMetadataValue::File(contents)
        } else if file_type.is_symlink() {
            let target = fs::read_link(&path)
                .map_err(|source| recovery_metadata_io_error(&relative, source))?;
            capture.reserve_metadata_bytes(
                relative
                    .as_os_str()
                    .as_encoded_bytes()
                    .len()
                    .saturating_add(target.as_os_str().as_encoded_bytes().len()),
            )?;
            RecoveryMetadataValue::Symlink(target)
        } else {
            return Err(GitError::Blocked {
                message: format!(
                    "recovery is blocked because control metadata {} has an unsupported filesystem type",
                    relative.display()
                ),
            });
        };
        entries.push(RecoveryMetadataEntry {
            path: relative.clone(),
            value,
        });

        if file_type.is_dir() {
            for child in fs::read_dir(&path)
                .map_err(|source| recovery_metadata_io_error(&relative, source))?
            {
                let child =
                    child.map_err(|source| recovery_metadata_io_error(&relative, source))?;
                capture.reserve_metadata_entry()?;
                pending.push((relative.join(child.file_name()), child.path()));
            }
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

fn recovery_metadata_io_error(path: &Path, source: std::io::Error) -> GitError {
    GitError::Io {
        args: vec![
            "snapshot-recovery-metadata".to_owned(),
            path.to_string_lossy().into_owned(),
        ],
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
            Self::GitFailed { .. } | Self::Blocked { .. } | Self::Parse { .. } => None,
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
    operation: Option<RepositoryOperation>,
    head: HeadTarget,
    index: Vec<u8>,
    worktree: Vec<RecoveryWorktreeEntry>,
    metadata: Vec<RecoveryMetadataEntry>,
    refs: Vec<u8>,
}

impl RecoveryState {
    fn rebase_progressed_from(&self, expected: &Self) -> bool {
        self.head != expected.head
            || REBASE_PROGRESS_PATHS.iter().any(|path| {
                let current = recovery_metadata_file(&self.metadata, path)
                    .and_then(parse_rebase_progress_marker);
                let previous = recovery_metadata_file(&expected.metadata, path)
                    .and_then(parse_rebase_progress_marker);
                matches!((current, previous), (Some(current), Some(previous)) if current > previous)
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryWorktreeEntry {
    path: PathBuf,
    value: RecoveryWorktreeValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecoveryWorktreeValue {
    Missing,
    File {
        size: u64,
        mode: u32,
        identity: (u64, u64),
        digest: [u8; 32],
    },
    Directory {
        mode: u32,
        identity: (u64, u64),
    },
    Symlink(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryMetadataEntry {
    path: PathBuf,
    value: RecoveryMetadataValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecoveryMetadataValue {
    Missing,
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
}

fn parse_rebase_progress_marker(contents: &[u8]) -> Option<u64> {
    std::str::from_utf8(contents).ok()?.trim().parse().ok()
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

    fn recovery_capabilities_or_verify_fail_closed(
        git: &Git,
        operation: RepositoryOperation,
        action: RecoveryAction,
        expected: &RecoveryState,
    ) -> Result<bool, Box<dyn Error>> {
        match git.ensure_recovery_supported() {
            Ok(()) => Ok(true),
            Err(GitError::Blocked { message })
                if message.starts_with(RECOVERY_CAPABILITY_UNAVAILABLE) =>
            {
                if env::var_os("BITBYGIT_REQUIRE_RECOVERY_SUCCESS").is_some() {
                    return Err(format!(
                        "production recovery capabilities are required for this test: {message}"
                    )
                    .into());
                }
                let root = git.repo_root()?.canonicalize()?;
                let before = RecoveryGeneration::capture(&root)?;
                let Err(error) = git.recover_exact(operation, action, expected) else {
                    return Err("recovery ran without its required platform capabilities".into());
                };
                assert!(
                    matches!(&error, GitError::Blocked { message } if message.starts_with(RECOVERY_CAPABILITY_UNAVAILABLE)),
                    "{error}"
                );
                assert_eq!(RecoveryGeneration::capture(&root)?, before);
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }

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

    #[test]
    fn exact_recovery_executes_every_supported_action() -> Result<(), Box<dyn Error>> {
        let (merge_continue_repo, _original_head) = prepare_merge_conflict()?;
        merge_continue_repo.write("conflict.txt", "resolved\n")?;
        merge_continue_repo.run(["add", "conflict.txt"])?;
        let git = Git::new(merge_continue_repo.path());
        let state = git.recovery_state()?;
        if !recovery_capabilities_or_verify_fail_closed(
            &git,
            RepositoryOperation::Merge,
            RecoveryAction::Continue,
            &state,
        )? {
            return Ok(());
        }
        git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Continue, &state)?;
        assert_eq!(git.status()?.operation, None);

        let (merge_abort_repo, original_head) = prepare_merge_conflict()?;
        let git = Git::new(merge_abort_repo.path());
        let state = git.recovery_state()?;
        git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &state)?;
        assert_eq!(git.status()?.operation, None);
        assert_eq!(
            merge_abort_repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );

        let (rebase_continue_repo, _original_head) = prepare_rebase_conflict()?;
        rebase_continue_repo.write("conflict.txt", "resolved\n")?;
        rebase_continue_repo.run(["add", "conflict.txt"])?;
        let git = Git::new(rebase_continue_repo.path());
        let state = git.recovery_state()?;
        git.recover_exact(
            RepositoryOperation::Rebase,
            RecoveryAction::Continue,
            &state,
        )?;
        assert_eq!(git.status()?.operation, None);

        let (rebase_abort_repo, original_head) = prepare_rebase_conflict()?;
        let git = Git::new(rebase_abort_repo.path());
        let state = git.recovery_state()?;
        git.recover_exact(RepositoryOperation::Rebase, RecoveryAction::Abort, &state)?;
        assert_eq!(git.status()?.operation, None);
        assert_eq!(
            rebase_abort_repo.git_stdout(["rev-parse", "HEAD"])?.trim(),
            original_head
        );

        let (rebase_skip_repo, _original_head) = prepare_rebase_conflict()?;
        let git = Git::new(rebase_skip_repo.path());
        let state = git.recovery_state()?;
        git.recover_exact(RepositoryOperation::Rebase, RecoveryAction::Skip, &state)?;
        assert_eq!(git.status()?.operation, None);
        Ok(())
    }

    #[test]
    fn exact_rebase_promotes_and_reports_subsequent_conflicts() -> Result<(), Box<dyn Error>> {
        let continue_repo = prepare_two_conflict_rebase()?;
        continue_repo.write("first.txt", "topic first\n")?;
        continue_repo.run(["add", "first.txt"])?;
        let continue_git = Git::new(continue_repo.path());
        let state = continue_git.recovery_state()?;
        if !recovery_capabilities_or_verify_fail_closed(
            &continue_git,
            RepositoryOperation::Rebase,
            RecoveryAction::Continue,
            &state,
        )? {
            return Ok(());
        }

        let Err(error) = continue_git.recover_exact(
            RepositoryOperation::Rebase,
            RecoveryAction::Continue,
            &state,
        ) else {
            return Err("expected exact continue to stop at the next conflict".into());
        };
        let GitError::GitFailed { stdout, stderr, .. } = error else {
            return Err("expected the next conflict to remain a Git failure".into());
        };
        assert!(format!("{stdout}\n{stderr}").contains("second.txt"));
        assert!(stderr.contains("previous repository generation retained at"));
        let status = continue_git.status()?;
        assert_eq!(status.operation, Some(RepositoryOperation::Rebase));
        assert_eq!(
            status.conflicted_files()[0].path,
            PathBuf::from("second.txt")
        );
        let first_backup = recovery_backup_from_pointer(&continue_repo.path().canonicalize()?)?
            .ok_or("missing retained recovery backup")?;
        assert!(first_backup.is_dir());

        continue_repo.write("second.txt", "topic second\n")?;
        continue_repo.run(["add", "second.txt"])?;
        let state = continue_git.recovery_state()?;
        let output = continue_git.recover_exact(
            RepositoryOperation::Rebase,
            RecoveryAction::Continue,
            &state,
        )?;
        assert_eq!(continue_git.status()?.operation, None);
        assert!(
            output
                .stderr
                .contains("previous repository generation retained at")
        );
        let second_backup = recovery_backup_from_pointer(&continue_repo.path().canonicalize()?)?
            .ok_or("missing replacement recovery backup")?;
        assert_ne!(second_backup, first_backup);
        assert!(!first_backup.exists());
        assert!(second_backup.is_dir());

        let skip_repo = prepare_two_conflict_rebase()?;
        let skip_git = Git::new(skip_repo.path());
        let state = skip_git.recovery_state()?;
        let Err(error) =
            skip_git.recover_exact(RepositoryOperation::Rebase, RecoveryAction::Skip, &state)
        else {
            return Err("expected exact skip to stop at the next conflict".into());
        };
        let GitError::GitFailed { stdout, stderr, .. } = error else {
            return Err("expected the conflict after skip to remain a Git failure".into());
        };
        assert!(format!("{stdout}\n{stderr}").contains("second.txt"));
        assert!(stderr.contains("previous repository generation retained at"));
        let status = skip_git.status()?;
        assert_eq!(status.operation, Some(RepositoryOperation::Rebase));
        assert_eq!(
            status.conflicted_files()[0].path,
            PathBuf::from("second.txt")
        );
        Ok(())
    }

    #[test]
    fn failed_rebase_skip_without_progress_does_not_promote() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_rebase_conflict()?;
        let git = Git::new(repo.path());
        let lock = git.git_path("index.lock")?;
        fs::write(&lock, "block skip before it advances\n")?;
        let expected = git.recovery_state()?;
        if !recovery_capabilities_or_verify_fail_closed(
            &git,
            RepositoryOperation::Rebase,
            RecoveryAction::Skip,
            &expected,
        )? {
            return Ok(());
        }
        let root = repo.path().canonicalize()?;
        let before = RecoveryGeneration::capture(&root)?;

        let Err(error) =
            git.recover_exact(RepositoryOperation::Rebase, RecoveryAction::Skip, &expected)
        else {
            return Err("expected locked rebase skip to fail".into());
        };

        assert!(matches!(error, GitError::GitFailed { .. }), "{error}");
        assert_eq!(git.recovery_state()?, expected);
        assert_eq!(RecoveryGeneration::capture(&root)?, before);
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        fs::remove_file(lock)?;
        Ok(())
    }

    #[test]
    fn recovery_bounds_hook_output_without_promoting() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let (repo, _original_head) = prepare_merge_conflict()?;
        repo.write("conflict.txt", "resolved\n")?;
        repo.run(["add", "conflict.txt"])?;
        let hook = repo.path().join(".git/hooks/commit-msg");
        fs::write(
            &hook,
            "#!/bin/sh\ndd if=/dev/zero bs=1048576 count=5 2>/dev/null\n",
        )?;
        let mut permissions = fs::metadata(&hook)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions)?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        if !recovery_capabilities_or_verify_fail_closed(
            &git,
            RepositoryOperation::Merge,
            RecoveryAction::Continue,
            &expected,
        )? {
            return Ok(());
        }

        let Err(error) = git.recover_exact(
            RepositoryOperation::Merge,
            RecoveryAction::Continue,
            &expected,
        ) else {
            return Err("expected noisy recovery hook to exceed the output bound".into());
        };

        assert!(error.to_string().contains("Git output bytes exceed"));
        assert_eq!(git.recovery_state()?, expected);
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        Ok(())
    }

    #[test]
    fn retained_backup_follows_repository_rename() -> Result<(), Box<dyn Error>> {
        let (mut repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        if !recovery_capabilities_or_verify_fail_closed(
            &git,
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            &expected,
        )? {
            return Ok(());
        }
        git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)?;
        let first_backup = recovery_backup_from_pointer(&repo.path().canonicalize()?)?
            .ok_or("missing retained recovery backup")?;
        let renamed = repo.path().with_extension("renamed");
        fs::rename(repo.path(), &renamed)?;
        repo.path = renamed;
        repo.run_allow_failure(["merge", "other"])?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;

        git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)?;

        assert!(!first_backup.exists());
        assert!(recovery_backup_from_pointer(&repo.path().canonicalize()?)?.is_some());
        Ok(())
    }

    #[test]
    fn same_path_replacement_does_not_own_retained_backup() -> Result<(), Box<dyn Error>> {
        let (mut repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        if !recovery_capabilities_or_verify_fail_closed(
            &git,
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            &expected,
        )? {
            return Ok(());
        }
        git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)?;
        let root = repo.path().canonicalize()?;
        let backup =
            recovery_backup_from_pointer(&root)?.ok_or("missing retained recovery backup")?;
        let former = root.with_extension("former");
        fs::rename(&root, &former)?;
        repo.path = former.clone();
        let output = Command::new("git")
            .args(["init", "-b", "main"])
            .arg(&root)
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "failed to create replacement repository: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }

        remove_previous_recovery_backup(&root.canonicalize()?)?;

        assert!(backup.is_dir());
        fs::remove_dir_all(&root)?;
        fs::rename(&former, &root)?;
        repo.path = root;
        Ok(())
    }

    #[test]
    fn exact_recovery_fails_closed_when_platform_capabilities_are_unavailable()
    -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        let root = repo.path().canonicalize()?;
        let before = RecoveryGeneration::capture(&root)?;
        RECOVERY_CAPABILITY_FAILURES
            .lock()
            .map_err(|_| "recovery capability failure lock poisoned")?
            .push(root.clone());

        let Err(error) =
            git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)
        else {
            return Err("expected unavailable recovery capabilities to block execution".into());
        };

        assert!(
            matches!(&error, GitError::Blocked { message } if message.starts_with(RECOVERY_CAPABILITY_UNAVAILABLE)),
            "{error}"
        );
        assert_eq!(RecoveryGeneration::capture(&root)?, before);
        assert_eq!(git.recovery_state()?, expected);
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_symlinked_mutable_git_storage() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        for relative in ["objects", "refs", "index"] {
            let (repo, _original_head) = prepare_merge_conflict()?;
            let storage = repo.path().join(".git").join(relative);
            let external = repo.path().with_extension(format!("external-{relative}"));
            fs::rename(&storage, &external)?;
            symlink(&external, &storage)?;

            let Err(error) = Git::new(repo.path()).ensure_recovery_supported() else {
                return Err(format!("expected symlinked .git/{relative} to block recovery").into());
            };
            assert!(error.to_string().contains("symlinked Git storage"));
            assert!(external.exists());

            fs::remove_file(&storage)?;
            fs::rename(&external, &storage)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_pointer_race_cannot_overwrite_symlink_target() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let (repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        if !recovery_capabilities_or_verify_fail_closed(
            &git,
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            &expected,
        )? {
            return Ok(());
        }
        let root = repo.path().canonicalize()?;
        let pointer = recovery_backup_pointer_path(&root)?;
        let victim = repo.path().with_extension("pointer-victim");
        fs::write(&victim, "must remain unchanged\n")?;
        let hook_pointer = pointer.clone();
        let hook_victim = victim.clone();
        let hook: RecoveryCaptureHook = Box::new(move || {
            let Some(root) = hook_pointer.parent().and_then(Path::parent) else {
                return;
            };
            let Some(parent) = root.parent() else {
                return;
            };
            let Ok(entries) = fs::read_dir(parent) else {
                return;
            };
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .as_encoded_bytes()
                    .starts_with(RECOVERY_CANDIDATE_PREFIX.as_bytes())
                {
                    let _ = symlink(
                        &hook_victim,
                        entry.path().join(".git").join(RECOVERY_BACKUP_POINTER),
                    );
                }
            }
        });
        RECOVERY_SIDECAR_HOOKS
            .lock()
            .map_err(|_| "recovery sidecar hook lock poisoned")?
            .push((pointer.clone(), hook));

        let Err(error) =
            git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)
        else {
            return Err("expected hostile backup pointer to block recovery".into());
        };
        assert!(error.to_string().contains("backup pointer"));
        assert_eq!(fs::read_to_string(&victim)?, "must remain unchanged\n");
        assert!(!pointer.exists());
        fs::remove_file(victim)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_owner_sidecar_cannot_overwrite_symlink_target() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let repo = initialized_repo()?;
        let owner = repo.path().with_extension("candidate.owner");
        let victim = repo.path().with_extension("owner-victim");
        fs::write(&victim, "must remain unchanged\n")?;
        symlink(&victim, &owner)?;

        let Err(error) = write_new_recovery_sidecar(&owner, b"replacement", "record owner") else {
            return Err("expected hostile owner sidecar to be rejected".into());
        };
        assert!(error.to_string().contains("record owner"));
        assert_eq!(fs::read_to_string(&victim)?, "must remain unchanged\n");
        fs::remove_file(owner)?;
        fs::remove_file(victim)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_generation_does_not_open_fifo_or_symlink_replacements() -> Result<(), Box<dyn Error>>
    {
        use std::os::unix::fs::{FileTypeExt, symlink};

        for replacement_kind in ["fifo", "symlink"] {
            let repo = initialized_repo()?;
            let path = repo.path().join("tracked.txt");
            fs::write(&path, "tracked\n")?;
            let replacement = repo.path().join(format!("replacement-{replacement_kind}"));
            if replacement_kind == "fifo" {
                if !Command::new("mkfifo").arg(&replacement).status()?.success() {
                    return Err("mkfifo failed".into());
                }
            } else {
                symlink(repo.path().join(".git/config"), &replacement)?;
            }
            let destination = path.clone();
            let hook: RecoveryCaptureHook = Box::new(move || {
                let _ = fs::rename(replacement, destination);
            });
            RECOVERY_FILE_OPEN_HOOKS
                .lock()
                .map_err(|_| "recovery file-open hook lock poisoned")?
                .push((path.clone(), hook));

            let Err(error) = RecoveryGeneration::capture(&repo.path()) else {
                return Err(
                    format!("expected {replacement_kind} replacement to be rejected").into(),
                );
            };
            assert!(matches!(error, GitError::Blocked { .. }));
            let file_type = fs::symlink_metadata(path)?.file_type();
            assert!(if replacement_kind == "fifo" {
                file_type.is_fifo()
            } else {
                file_type.is_symlink()
            });
        }
        Ok(())
    }

    #[test]
    fn recovery_generation_bounds_file_growth_after_open() -> Result<(), Box<dyn Error>> {
        let repo = initialized_repo()?;
        let path = repo.path().join("growing.txt");
        fs::write(&path, "before\n")?;
        let growing = path.clone();
        let hook: RecoveryCaptureHook = Box::new(move || {
            if let Ok(mut file) = fs::OpenOptions::new().append(true).open(growing) {
                let _ = file.write_all(b"after\n");
            }
        });
        RECOVERY_GENERATION_FILE_READ_HOOKS
            .lock()
            .map_err(|_| "recovery generation read hook lock poisoned")?
            .push((path, hook));

        let Err(error) = RecoveryGeneration::capture(&repo.path()) else {
            return Err("expected file growth during generation capture to be rejected".into());
        };
        assert!(error.to_string().contains("grew while it was captured"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_generation_rejects_hard_links() -> Result<(), Box<dyn Error>> {
        let repo = initialized_repo()?;
        let original = repo.path().join("original.txt");
        fs::write(&original, "linked\n")?;
        fs::hard_link(&original, repo.path().join("linked.txt"))?;

        let Err(error) = RecoveryGeneration::capture(&repo.path()) else {
            return Err("expected hard-linked files to block atomic recovery".into());
        };
        assert!(error.to_string().contains("hard-linked file"));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn atomic_promotion_rolls_back_concurrent_xattr_change() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        if !recovery_capabilities_or_verify_fail_closed(
            &git,
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            &expected,
        )? {
            return Ok(());
        }
        let conflict = repo.path().join("conflict.txt");
        let changed = conflict.clone();
        let hook: RecoveryCaptureHook = Box::new(move || {
            let _ = xattr::set(changed, "user.bitbygit-test", b"changed");
        });
        RECOVERY_PROMOTION_HOOKS
            .lock()
            .map_err(|_| "recovery promotion hook lock poisoned")?
            .push((repo.path().canonicalize()?, hook));

        let Err(error) =
            git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)
        else {
            return Err("expected concurrent xattr change to roll back recovery".into());
        };
        assert!(
            error
                .to_string()
                .contains("metadata changed during atomic recovery promotion")
        );
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        assert_eq!(
            xattr::get(conflict, "user.bitbygit-test")?,
            Some(b"changed".to_vec())
        );
        Ok(())
    }

    #[test]
    fn recovery_rejects_external_git_storage_paths() -> Result<(), Box<dyn Error>> {
        if let (Some(repo), Some(variable)) = (
            env::var_os("BITBYGIT_TEST_EXTERNAL_GIT_REPO"),
            env::var_os("BITBYGIT_TEST_EXTERNAL_GIT_VARIABLE"),
        ) {
            let variable = variable.to_string_lossy();
            let irrelevant_state = RecoveryState {
                operation: Some(RepositoryOperation::Merge),
                head: HeadTarget {
                    oid: None,
                    reference: None,
                },
                index: Vec::new(),
                worktree: Vec::new(),
                metadata: Vec::new(),
                refs: Vec::new(),
            };
            let Err(error) = Git::new(repo).recover_exact(
                RepositoryOperation::Merge,
                RecoveryAction::Abort,
                &irrelevant_state,
            ) else {
                return Err(format!("expected inherited {variable} to block recovery").into());
            };
            assert!(error.to_string().contains(variable.as_ref()));
            return Ok(());
        }

        let (repo, _original_head) = prepare_merge_conflict()?;
        let external_index = repo.path().with_extension("external-index");
        fs::copy(repo.path().join(".git/index"), &external_index)?;
        let index_before = fs::read(&external_index)?;
        let external_objects = repo.path().with_extension("external-objects");
        fs::create_dir(&external_objects)?;
        fs::write(external_objects.join("sentinel"), "unchanged\n")?;
        let objects_before = RecoveryGeneration::capture(&external_objects)?;

        for (variable, external_path) in [
            ("GIT_INDEX_FILE", external_index.as_path()),
            ("GIT_OBJECT_DIRECTORY", external_objects.as_path()),
        ] {
            let output = Command::new(env::current_exe()?)
                .args([
                    "--exact",
                    "tests::recovery_rejects_external_git_storage_paths",
                    "--nocapture",
                ])
                .env("BITBYGIT_TEST_EXTERNAL_GIT_REPO", repo.path())
                .env("BITBYGIT_TEST_EXTERNAL_GIT_VARIABLE", variable)
                .env(variable, external_path)
                .output()?;
            assert!(
                output.status.success(),
                "child test for {variable} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        assert_eq!(fs::read(external_index)?, index_before);
        assert_eq!(
            RecoveryGeneration::capture(&external_objects)?,
            objects_before
        );
        fs::remove_file(repo.path().with_extension("external-index"))?;
        fs::remove_dir_all(external_objects)?;
        Ok(())
    }

    #[test]
    fn exact_recovery_rejects_state_changed_after_preview() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        repo.write("conflict.txt", "changed after preview\n")?;

        let Err(error) =
            git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)
        else {
            return Err("expected changed recovery state to be rejected".into());
        };

        assert!(error.to_string().contains("state changed after preview"));
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        Ok(())
    }

    #[test]
    fn exact_recovery_rejects_changed_merge_metadata() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        fs::write(git.git_path("MERGE_MSG")?, "changed merge message\n")?;

        let Err(error) =
            git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)
        else {
            return Err("expected changed merge metadata to be rejected".into());
        };

        assert!(error.to_string().contains("state changed after preview"));
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        Ok(())
    }

    #[test]
    fn exact_recovery_rejects_changed_rebase_metadata() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_rebase_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        let todo = git.git_path("rebase-merge/git-rebase-todo")?;
        let mut changed_todo = fs::read(&todo)?;
        changed_todo.extend_from_slice(b"# changed after preview\n");
        fs::write(todo, changed_todo)?;

        let Err(error) = git.recover_exact(
            RepositoryOperation::Rebase,
            RecoveryAction::Abort,
            &expected,
        ) else {
            return Err("expected changed rebase metadata to be rejected".into());
        };

        assert!(error.to_string().contains("state changed after preview"));
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        Ok(())
    }

    #[test]
    fn exact_rebase_abort_preserves_branch_changed_after_preview() -> Result<(), Box<dyn Error>> {
        let (repo, original_head) = prepare_rebase_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        let newer_head = repo.git_stdout(["rev-parse", "main"])?.trim().to_owned();
        assert_ne!(newer_head, original_head);
        repo.run(["update-ref", "refs/heads/topic", &newer_head])?;

        let Err(error) = git.recover_exact(
            RepositoryOperation::Rebase,
            RecoveryAction::Abort,
            &expected,
        ) else {
            return Err("expected changed rebase branch to block abort".into());
        };

        assert!(error.to_string().contains("state changed after preview"));
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        assert_eq!(
            repo.git_stdout(["rev-parse", "refs/heads/topic"])?.trim(),
            newer_head
        );
        Ok(())
    }

    #[test]
    fn exact_rebase_continue_rejects_changed_update_refs_branch() -> Result<(), Box<dyn Error>> {
        let repo = initialized_repo()?;
        repo.run(["switch", "-c", "topic"])?;
        repo.write("first.txt", "first\n")?;
        repo.run(["add", "first.txt"])?;
        repo.run(["commit", "-m", "first"])?;
        repo.run(["branch", "side"])?;
        repo.write("second.txt", "second\n")?;
        repo.run(["add", "second.txt"])?;
        repo.run(["commit", "-m", "second"])?;
        repo.run(["switch", "main"])?;
        repo.write("upstream.txt", "upstream\n")?;
        repo.run(["add", "upstream.txt"])?;
        repo.run(["commit", "-m", "upstream"])?;
        repo.run(["switch", "topic"])?;
        repo.run_allow_failure(["rebase", "--update-refs", "--exec", "false", "main"])?;

        let git = Git::new(repo.path());
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        let update_refs = fs::read(git.git_path("rebase-merge/update-refs")?)?;
        assert!(
            update_refs
                .split(|byte| *byte == b'\n')
                .any(|line| line == b"refs/heads/side")
        );
        let expected = git.recovery_state()?;
        let preview_head = repo.git_stdout(["rev-parse", "HEAD"])?.trim().to_owned();
        let moved_side = repo.git_stdout(["rev-parse", "main"])?.trim().to_owned();
        repo.run(["update-ref", "refs/heads/side", &moved_side])?;

        let Err(error) = git.recover_exact(
            RepositoryOperation::Rebase,
            RecoveryAction::Continue,
            &expected,
        ) else {
            return Err("expected changed update-refs branch to block rebase continue".into());
        };

        assert!(error.to_string().contains("state changed after preview"));
        assert_eq!(repo.git_stdout(["rev-parse", "HEAD"])?.trim(), preview_head);
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        assert_eq!(
            repo.git_stdout(["rev-parse", "refs/heads/side"])?.trim(),
            moved_side
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn exact_rebase_abort_preserves_ignored_file_chmod_after_preview() -> Result<(), Box<dyn Error>>
    {
        use std::os::unix::fs::PermissionsExt;

        let repo = prepare_rebase_with_ignored_victim()?;

        let git = Git::new(repo.path());
        let victim = repo.path().join("target/victim.bin");
        let expected = git.recovery_state()?;
        let mut permissions = fs::metadata(&victim)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&victim, permissions)?;

        let Err(error) = git.recover_exact(
            RepositoryOperation::Rebase,
            RecoveryAction::Abort,
            &expected,
        ) else {
            return Err("expected ignored executable mode change to block rebase abort".into());
        };

        assert!(error.to_string().contains("state changed after preview"));
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        assert_ne!(fs::metadata(victim)?.permissions().mode() & 0o111, 0);
        Ok(())
    }

    #[test]
    fn exact_rebase_abort_preserves_ignored_directory_child_changed_after_preview()
    -> Result<(), Box<dyn Error>> {
        let repo = prepare_rebase_with_ignored_directory_collision()?;
        let git = Git::new(repo.path());
        let victim = repo.path().join("victim");
        let child = victim.join("data");
        let expected = git.recovery_state()?;
        fs::write(&child, "changed after preview\n")?;

        let Err(error) = git.recover_exact(
            RepositoryOperation::Rebase,
            RecoveryAction::Abort,
            &expected,
        ) else {
            return Err("expected ignored directory child change to block rebase abort".into());
        };

        assert!(error.to_string().contains("state changed after preview"));
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        assert!(victim.is_dir());
        assert_eq!(fs::read_to_string(child)?, "changed after preview\n");
        Ok(())
    }

    #[test]
    fn exact_rebase_uses_backend_orig_head_when_top_level_orig_head_is_wrong()
    -> Result<(), Box<dyn Error>> {
        let repo = prepare_rebase_with_ignored_victim()?;
        let git = Git::new(repo.path());
        let victim = repo.path().join("target/victim.bin");
        repo.run(["update-ref", "ORIG_HEAD", "HEAD"])?;
        assert_ne!(
            fs::read_to_string(git.git_path("rebase-merge/orig-head")?)?.trim(),
            repo.git_stdout(["rev-parse", "ORIG_HEAD"])?.trim()
        );
        let expected = git.recovery_state()?;
        fs::write(&victim, "changed after preview\n")?;

        let Err(error) = git.recover_exact(
            RepositoryOperation::Rebase,
            RecoveryAction::Abort,
            &expected,
        ) else {
            return Err("expected ignored content change to block rebase abort".into());
        };

        assert!(error.to_string().contains("state changed after preview"));
        assert_eq!(fs::read_to_string(victim)?, "changed after preview\n");
        Ok(())
    }

    #[test]
    fn recovery_metadata_entry_bound_applies_before_wide_tree_traversal()
    -> Result<(), Box<dyn Error>> {
        let repo = initialized_repo()?;
        let git = Git::new(repo.path());
        let backend = git.git_path("rebase-merge")?;
        fs::create_dir(&backend)?;
        for index in 0..MAX_RECOVERY_METADATA_ENTRIES {
            fs::write(backend.join(index.to_string()), [])?;
        }

        let Err(error) = git.recovery_state() else {
            return Err("expected wide recovery metadata tree to exceed entry bound".into());
        };
        assert!(error.to_string().contains("metadata entries"));
        Ok(())
    }

    #[test]
    fn changed_path_parser_stops_at_relevant_file_bound() -> Result<(), Box<dyn Error>> {
        let mut output = Vec::new();
        for index in 0..=MAX_RECOVERY_RELEVANT_FILES {
            output.extend_from_slice(b":100644 100644 0000000 0000000 M\0");
            output.extend_from_slice(format!("path-{index}\0").as_bytes());
        }
        let mut paths = BTreeSet::new();

        let Err(error) = parse_recovery_changed_paths(&output, &mut paths) else {
            return Err("expected changed paths to exceed relevant file bound".into());
        };
        assert!(error.to_string().contains("relevant file count"));
        assert_eq!(paths.len(), MAX_RECOVERY_RELEVANT_FILES);
        Ok(())
    }

    #[test]
    fn exact_recovery_ignores_and_preserves_unrelated_untracked_tree() -> Result<(), Box<dyn Error>>
    {
        let (repo, _original_head) = prepare_merge_conflict()?;
        fs::create_dir(repo.path().join("ignored"))?;
        fs::write(repo.path().join(".git/info/exclude"), "ignored/\n")?;
        for index in 0..=MAX_RECOVERY_RELEVANT_FILES {
            repo.write(&format!("ignored/{index}"), "ignored\n")?;
        }
        repo.write("notes.txt", "before preview\n")?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        repo.write("notes.txt", "changed after preview\n")?;

        if !recovery_capabilities_or_verify_fail_closed(
            &git,
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            &expected,
        )? {
            return Ok(());
        }

        git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)?;

        assert_eq!(git.status()?.operation, None);
        assert_eq!(
            fs::read_to_string(repo.path().join("notes.txt"))?,
            "changed after preview\n"
        );
        Ok(())
    }

    #[test]
    fn recovery_state_blocks_large_tracked_binary_before_reading_it() -> Result<(), Box<dyn Error>>
    {
        use std::io::Write;

        let (repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let mut file = fs::File::options()
            .write(true)
            .truncate(true)
            .open(repo.path().join("conflict.txt"))?;
        let mut chunk = [0_u8; 64 * 1024];
        let mut value = 0x9e37_79b9_u32;
        for byte in &mut chunk {
            value ^= value << 13;
            value ^= value >> 17;
            value ^= value << 5;
            *byte = value as u8;
        }
        for _ in 0..(MAX_RECOVERY_FILE_BYTES_READ / chunk.len() as u64) {
            file.write_all(&chunk)?;
        }
        file.write_all(&[1])?;
        file.flush()?;

        let Err(error) = git.recovery_state() else {
            return Err("expected large tracked file to exceed recovery read bound".into());
        };
        assert!(error.to_string().contains("file content bytes read"));
        Ok(())
    }

    #[test]
    fn exact_recovery_blocks_relevant_filesystem_type_replacement() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        let conflict = repo.path().join("conflict.txt");
        fs::remove_file(&conflict)?;
        fs::create_dir(&conflict)?;

        let Err(error) =
            git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)
        else {
            return Err("expected filesystem type replacement to block recovery".into());
        };
        assert!(error.to_string().contains("state changed after preview"));
        assert!(conflict.is_dir());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_fingerprint_does_not_hang_on_fifo_replacement() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::FileTypeExt;

        let (repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let conflict = repo.path().join("conflict.txt");
        let fifo = repo.path().join("replacement-fifo");
        let status = Command::new("mkfifo").arg(&fifo).status()?;
        if !status.success() {
            return Err("mkfifo failed".into());
        }
        let replacement = fifo.clone();
        let destination = conflict.clone();
        let hook: RecoveryCaptureHook = Box::new(move || {
            let _ = fs::rename(replacement, destination);
        });
        RECOVERY_FILE_OPEN_HOOKS
            .lock()
            .map_err(|_| "recovery file-open hook lock poisoned")?
            .push((conflict.clone(), hook));

        let Err(error) = git.recovery_state() else {
            return Err("expected FIFO replacement to block recovery guard".into());
        };
        assert!(matches!(error, GitError::Blocked { .. }));
        assert!(fs::symlink_metadata(conflict)?.file_type().is_fifo());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_fingerprint_does_not_follow_symlink_replacement() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let (repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let conflict = repo.path().join("conflict.txt");
        let secret = repo.path().join("secret.txt");
        let replacement = repo.path().join("replacement-link");
        fs::write(&secret, "must not be fingerprinted\n")?;
        symlink(&secret, &replacement)?;
        let destination = conflict.clone();
        let hook: RecoveryCaptureHook = Box::new(move || {
            let _ = fs::rename(replacement, destination);
        });
        RECOVERY_FILE_OPEN_HOOKS
            .lock()
            .map_err(|_| "recovery file-open hook lock poisoned")?
            .push((conflict.clone(), hook));

        let Err(error) = git.recovery_state() else {
            return Err("expected symlink replacement to block recovery guard".into());
        };
        assert!(matches!(error, GitError::Blocked { .. }));
        assert!(fs::symlink_metadata(conflict)?.file_type().is_symlink());
        assert_eq!(fs::read_to_string(secret)?, "must not be fingerprinted\n");
        Ok(())
    }

    #[test]
    fn exact_recovery_repeats_guard_after_synchronized_concurrent_edit()
    -> Result<(), Box<dyn Error>> {
        use std::sync::mpsc;

        let (repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        if !recovery_capabilities_or_verify_fail_closed(
            &git,
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            &expected,
        )? {
            return Ok(());
        }
        let conflict = repo.path().join("conflict.txt");
        let (scanned_tx, scanned_rx) = mpsc::channel();
        let (edited_tx, edited_rx) = mpsc::channel();
        let editor = std::thread::spawn(move || {
            let _ = scanned_rx.recv();
            let result = fs::write(conflict, "concurrent edit\n");
            let _ = edited_tx.send(result);
        });
        let hook: RecoveryCaptureHook = Box::new(move || {
            let _ = scanned_tx.send(());
            let _ = edited_rx.recv();
        });
        *RECOVERY_CAPTURE_HOOK
            .lock()
            .map_err(|_| "recovery capture hook lock poisoned")? = Some((repo.path(), hook));

        let Err(error) =
            git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)
        else {
            return Err("expected concurrent edit to block recovery".into());
        };
        editor.join().map_err(|_| "editor thread panicked")?;
        assert!(error.to_string().contains("repository changed"));
        assert_eq!(
            fs::read_to_string(repo.path().join("conflict.txt"))?,
            "concurrent edit\n"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn isolated_rebase_abort_preserves_ref_race_during_prepared_hook_without_partial_recovery()
    -> Result<(), Box<dyn Error>> {
        let (repo, original_head) = prepare_rebase_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        if !recovery_capabilities_or_verify_fail_closed(
            &git,
            RepositoryOperation::Rebase,
            RecoveryAction::Abort,
            &expected,
        )? {
            return Ok(());
        }
        let (signal, release) = install_recovery_prepared_barrier(&repo)?;
        let preview_head = repo.git_stdout(["rev-parse", "HEAD"])?;
        let preview_conflict = fs::read(repo.path().join("conflict.txt"))?;
        let newer_head = repo.git_stdout(["rev-parse", "main"])?.trim().to_owned();
        assert_ne!(newer_head, original_head);
        let worker_path = repo.path();
        let worker_state = expected.clone();
        let worker = std::thread::spawn(move || {
            Git::new(worker_path).recover_exact(
                RepositoryOperation::Rebase,
                RecoveryAction::Abort,
                &worker_state,
            )
        });
        wait_for_recovery_barrier(&signal)?;
        fs::write(git.git_path("refs/heads/topic")?, format!("{newer_head}\n"))?;
        fs::write(&release, [])?;

        let Err(error) = worker.join().map_err(|_| "recovery worker panicked")? else {
            return Err("expected exact rebase abort to fail closed".into());
        };
        assert!(
            error
                .to_string()
                .contains("while recovery executed in isolation"),
            "{error}"
        );
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        assert_eq!(repo.git_stdout(["rev-parse", "HEAD"])?, preview_head);
        let current = git.recovery_state()?;
        assert_eq!(current.index, expected.index);
        assert_eq!(current.worktree, expected.worktree);
        assert_eq!(current.metadata, expected.metadata);
        assert_eq!(
            fs::read(repo.path().join("conflict.txt"))?,
            preview_conflict
        );
        assert_eq!(
            repo.git_stdout(["rev-parse", "refs/heads/topic"])?.trim(),
            newer_head
        );
        let _ = fs::remove_file(signal);
        let _ = fs::remove_file(release);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn isolated_recovery_preserves_post_spawn_worktree_race() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_rebase_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        if !recovery_capabilities_or_verify_fail_closed(
            &git,
            RepositoryOperation::Rebase,
            RecoveryAction::Abort,
            &expected,
        )? {
            return Ok(());
        }
        let (signal, release) = install_recovery_prepared_barrier(&repo)?;
        let worker_path = repo.path();
        let worker_state = expected.clone();
        let worker = std::thread::spawn(move || {
            Git::new(worker_path).recover_exact(
                RepositoryOperation::Rebase,
                RecoveryAction::Abort,
                &worker_state,
            )
        });
        wait_for_recovery_barrier(&signal)?;
        fs::write(
            repo.path().join("conflict.txt"),
            "post-spawn worktree edit\n",
        )?;
        fs::write(&release, [])?;

        let Err(error) = worker.join().map_err(|_| "recovery worker panicked")? else {
            return Err("expected late worktree edit to block recovery".into());
        };

        assert!(
            error
                .to_string()
                .contains("while recovery executed in isolation")
        );
        let current = git.recovery_state()?;
        assert_eq!(current.head, expected.head);
        assert_eq!(current.index, expected.index);
        assert_eq!(current.metadata, expected.metadata);
        assert_eq!(current.refs, expected.refs);
        assert_eq!(
            fs::read_to_string(repo.path().join("conflict.txt"))?,
            "post-spawn worktree edit\n"
        );
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        let _ = fs::remove_file(signal);
        let _ = fs::remove_file(release);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn isolated_recovery_preserves_post_spawn_index_race() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_rebase_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        if !recovery_capabilities_or_verify_fail_closed(
            &git,
            RepositoryOperation::Rebase,
            RecoveryAction::Abort,
            &expected,
        )? {
            return Ok(());
        }
        let (signal, release) = install_recovery_prepared_barrier(&repo)?;
        let original_blob = repo
            .git_stdout(["rev-parse", "HEAD:conflict.txt"])?
            .trim()
            .to_owned();
        let worker_path = repo.path();
        let worker_state = expected.clone();
        let worker = std::thread::spawn(move || {
            Git::new(worker_path).recover_exact(
                RepositoryOperation::Rebase,
                RecoveryAction::Abort,
                &worker_state,
            )
        });
        wait_for_recovery_barrier(&signal)?;
        let output = Command::new("git")
            .current_dir(repo.path())
            .args([
                "update-index",
                "--cacheinfo",
                "100644",
                &original_blob,
                "conflict.txt",
            ])
            .output()?;
        assert!(output.status.success());
        fs::write(&release, [])?;

        let Err(error) = worker.join().map_err(|_| "recovery worker panicked")? else {
            return Err("expected late index edit to block recovery".into());
        };

        assert!(
            error
                .to_string()
                .contains("while recovery executed in isolation")
        );
        let current = git.recovery_state()?;
        assert_eq!(current.head, expected.head);
        assert_ne!(current.index, expected.index);
        assert_eq!(current.worktree, expected.worktree);
        assert_eq!(current.metadata, expected.metadata);
        assert_eq!(current.refs, expected.refs);
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Rebase));
        let _ = fs::remove_file(signal);
        let _ = fs::remove_file(release);
        Ok(())
    }

    #[test]
    fn atomic_promotion_rolls_back_race_after_expected_value_check() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_merge_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        if !recovery_capabilities_or_verify_fail_closed(
            &git,
            RepositoryOperation::Merge,
            RecoveryAction::Abort,
            &expected,
        )? {
            return Ok(());
        }
        let conflict = repo.path().join("conflict.txt");
        let hook: RecoveryCaptureHook = Box::new(move || {
            let _ = fs::write(conflict, "promotion race\n");
        });
        RECOVERY_PROMOTION_HOOKS
            .lock()
            .map_err(|_| "recovery promotion hook lock poisoned")?
            .push((repo.path().canonicalize()?, hook));

        let Err(error) =
            git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Abort, &expected)
        else {
            return Err("expected promotion race to roll back recovery".into());
        };

        assert!(
            error
                .to_string()
                .contains("during atomic recovery promotion")
        );
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        let current = git.recovery_state()?;
        assert_eq!(current.head, expected.head);
        assert_eq!(current.index, expected.index);
        assert_eq!(current.metadata, expected.metadata);
        assert_eq!(current.refs, expected.refs);
        assert_eq!(
            fs::read_to_string(repo.path().join("conflict.txt"))?,
            "promotion race\n"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_leaves_configured_hook_namespace_and_config_unchanged() -> Result<(), Box<dyn Error>>
    {
        use std::os::unix::fs::PermissionsExt;

        let (repo, _original_head) = prepare_merge_conflict()?;
        let hooks = repo.path().join("hooks");
        fs::create_dir(&hooks)?;
        let support_files = [
            ("expected-refs", b"configured expected refs\n".as_slice()),
            (
                "transaction-input",
                b"configured transaction input\n".as_slice(),
            ),
        ];
        for (name, contents) in support_files {
            fs::write(hooks.join(name), contents)?;
        }
        let commit_hook = hooks.join("commit-msg");
        fs::write(
            &commit_hook,
            "#!/bin/sh\npwd > configured-commit-hook-ran\n",
        )?;
        let transaction_hook = hooks.join("reference-transaction");
        fs::write(
            &transaction_hook,
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >> configured-reference-hook-ran\n",
        )?;
        for hook in [&commit_hook, &transaction_hook] {
            let mut permissions = fs::metadata(hook)?.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(hook, permissions)?;
        }
        repo.run(["config", "core.hooksPath", "hooks"])?;
        repo.write("conflict.txt", "resolved\n")?;
        repo.run(["add", "conflict.txt"])?;
        let git = Git::new(repo.path());
        let state = git.recovery_state()?;
        let config_path = git.git_path("config")?;
        let config_before = fs::read(&config_path)?;
        let hook_files = [
            hooks.join("expected-refs"),
            hooks.join("transaction-input"),
            commit_hook,
            transaction_hook,
        ];
        let contents_before = hook_files
            .iter()
            .map(fs::read)
            .collect::<Result<Vec<_>, _>>()?;

        if !recovery_capabilities_or_verify_fail_closed(
            &git,
            RepositoryOperation::Merge,
            RecoveryAction::Continue,
            &state,
        )? {
            return Ok(());
        }

        git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Continue, &state)?;

        assert_eq!(git.status()?.operation, None);
        assert_eq!(
            fs::read_to_string(repo.path().join("configured-commit-hook-ran"))?.trim(),
            repo.path().canonicalize()?.to_string_lossy()
        );
        assert!(repo.path().join("configured-reference-hook-ran").exists());
        assert_eq!(fs::read(config_path)?, config_before);
        for (path, contents) in hook_files.iter().zip(contents_before) {
            assert_eq!(fs::read(path)?, contents);
        }
        assert!(fs::read_dir(git.git_path("")?)?.all(|entry| {
            entry.is_ok_and(|entry| {
                !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("bitbygit-recovery-hooks-")
            })
        }));
        Ok(())
    }

    #[test]
    fn exact_rebase_rejects_changed_rewritten_ref() -> Result<(), Box<dyn Error>> {
        let (repo, _original_head) = prepare_rebase_merge_conflict()?;
        let git = Git::new(repo.path());
        let expected = git.recovery_state()?;
        let newer_head = repo.git_stdout(["rev-parse", "main"])?.trim().to_owned();
        repo.run(["update-ref", "refs/rewritten/concurrent", &newer_head])?;

        let Err(error) = git.recover_exact(
            RepositoryOperation::Rebase,
            RecoveryAction::Abort,
            &expected,
        ) else {
            return Err("expected changed rewritten ref to block recovery".into());
        };

        assert!(error.to_string().contains("state changed after preview"));
        assert_eq!(
            repo.git_stdout(["rev-parse", "refs/rewritten/concurrent"])?
                .trim(),
            newer_head
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn merge_continue_runs_hooks_and_does_not_bypass_signing() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let (repo, _original_head) = prepare_merge_conflict()?;
        let hook_marker = repo.path().with_extension("hook-ran");
        let signing_marker = repo.path().with_extension("signing-ran");
        let hooks = repo.path().join("hooks");
        fs::create_dir(&hooks)?;
        let commit_msg_hook = hooks.join("commit-msg");
        fs::write(
            &commit_msg_hook,
            format!(
                "#!/bin/sh\ntouch {}\nprintf '\\377'\n",
                shell_quote(&hook_marker.to_string_lossy())
            ),
        )?;
        let mut permissions = fs::metadata(&commit_msg_hook)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&commit_msg_hook, permissions)?;
        repo.run(["config", "core.hooksPath", "hooks"])?;

        let signing_program = repo.path().join("signing-program");
        fs::write(
            &signing_program,
            format!(
                "#!/bin/sh\ntouch {}\nexit 1\n",
                shell_quote(&signing_marker.to_string_lossy())
            ),
        )?;
        let mut permissions = fs::metadata(&signing_program)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&signing_program, permissions)?;
        repo.run_args(&["config", "gpg.program", &signing_program.to_string_lossy()])?;
        repo.run(["config", "commit.gpgsign", "true"])?;
        repo.write("conflict.txt", "resolved\n")?;
        repo.run(["add", "conflict.txt"])?;
        let git = Git::new(repo.path());

        let state = git.recovery_state()?;
        if !recovery_capabilities_or_verify_fail_closed(
            &git,
            RepositoryOperation::Merge,
            RecoveryAction::Continue,
            &state,
        )? {
            return Ok(());
        }
        let Err(error) =
            git.recover_exact(RepositoryOperation::Merge, RecoveryAction::Continue, &state)
        else {
            return Err("expected configured signing failure".into());
        };
        let GitError::GitFailed {
            args,
            stdout,
            stderr,
            ..
        } = error
        else {
            return Err("expected standard git merge failure".into());
        };
        assert!(args.ends_with(&["merge".to_owned(), "--continue".to_owned()]));
        assert!(format!("{stdout}{stderr}").contains('\u{fffd}'));
        assert!(hook_marker.exists());
        assert!(signing_marker.exists());
        assert_eq!(git.status()?.operation, Some(RepositoryOperation::Merge));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn successful_recovery_preserves_non_utf8_hook_output() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let (repo, _original_head) = prepare_merge_conflict()?;
        let hook = repo.path().join(".git").join("hooks").join("commit-msg");
        fs::write(&hook, "#!/bin/sh\nprintf '\\377'\n")?;
        let mut permissions = fs::metadata(&hook)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions)?;
        repo.write("conflict.txt", "resolved\n")?;
        repo.run(["add", "conflict.txt"])?;
        let git = Git::new(repo.path());

        let output = git.recover(RepositoryOperation::Merge, RecoveryAction::Continue)?;

        assert!(format!("{}{}", output.stdout, output.stderr).contains('\u{fffd}'));
        assert_eq!(git.status()?.operation, None);
        repo.run(["rev-parse", "--verify", "HEAD^2"])?;
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

    fn prepare_rebase_with_ignored_victim() -> Result<TempRepo, Box<dyn Error>> {
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
        fs::create_dir(repo.path().join("target"))?;
        repo.write("target/victim.bin", "before preview\n")?;
        assert_eq!(
            Git::new(repo.path()).status()?.operation,
            Some(RepositoryOperation::Rebase)
        );
        Ok(repo)
    }

    fn prepare_rebase_with_ignored_directory_collision() -> Result<TempRepo, Box<dyn Error>> {
        let repo = initialized_repo()?;
        repo.run(["switch", "-c", "topic"])?;
        repo.write(".gitignore", "victim/\n")?;
        repo.write("first.txt", "first\n")?;
        repo.run(["add", ".gitignore", "first.txt"])?;
        repo.run(["commit", "-m", "ignore victim"])?;
        repo.write(".gitignore", "")?;
        repo.write("victim", "committed file\n")?;
        repo.run(["add", ".gitignore", "victim"])?;
        repo.run(["commit", "-m", "track victim file"])?;
        repo.run(["switch", "main"])?;
        repo.write("upstream.txt", "upstream\n")?;
        repo.run(["add", "upstream.txt"])?;
        repo.run(["commit", "-m", "upstream"])?;
        repo.run(["switch", "topic"])?;
        repo.run_allow_failure(["rebase", "--exec", "false", "main"])?;
        fs::create_dir(repo.path().join("victim"))?;
        repo.write("victim/data", "before preview\n")?;
        assert_eq!(
            Git::new(repo.path()).status()?.operation,
            Some(RepositoryOperation::Rebase)
        );
        Ok(repo)
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

    #[cfg(unix)]
    fn install_recovery_prepared_barrier(
        repo: &TempRepo,
    ) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let repo_path = repo.path();
        let parent = repo_path.parent().ok_or("test repository has no parent")?;
        let name = repo_path
            .file_name()
            .ok_or("test repository has no file name")?
            .to_string_lossy();
        let signal = parent.join(format!("{name}-recovery-child-prepared"));
        let release = parent.join(format!("{name}-recovery-child-release"));
        let _ = fs::remove_file(&signal);
        let _ = fs::remove_file(&release);
        let hook = repo.path().join(".git/hooks/reference-transaction");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nif [ \"$1\" = prepared ]; then\n  touch {}\n  while [ ! -e {} ]; do sleep 0.01; done\nfi\n",
                shell_quote(&signal.to_string_lossy()),
                shell_quote(&release.to_string_lossy())
            ),
        )?;
        let mut permissions = fs::metadata(&hook)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(hook, permissions)?;
        Ok((signal, release))
    }

    #[cfg(unix)]
    fn wait_for_recovery_barrier(signal: &Path) -> Result<(), Box<dyn Error>> {
        for _ in 0..500 {
            if signal.exists() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Err("isolated recovery child did not reach its prepared hook".into())
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
            if let Ok(root) = self.path.canonicalize()
                && let Ok(Some(backup)) = recovery_backup_from_pointer(&root)
            {
                let _result = fs::remove_dir_all(&backup);
                let _result = fs::remove_file(recovery_candidate_owner_path(&backup));
                if let Ok(pointer) = recovery_backup_pointer_path(&root) {
                    let _result = fs::remove_file(pointer);
                }
            }
            let _result = fs::remove_dir_all(&self.path);
        }
    }
}
