use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::OsString;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::string::FromUtf8Error;

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const EMPTY_TREE_OID: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const COMMIT_HOOKS: &[&str] = &[
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
];

#[derive(Debug, Clone)]
pub struct Git {
    cwd: PathBuf,
}

impl Git {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
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

    pub fn push_current_branch(&self, remote: &str, branch: &str) -> Result<GitOutput, GitError> {
        self.run_args(vec![
            "push".to_owned(),
            "--".to_owned(),
            remote.to_owned(),
            format!("HEAD:refs/heads/{branch}"),
        ])
    }

    pub fn push_current_branch_set_upstream(
        &self,
        remote: &str,
        branch: &str,
    ) -> Result<GitOutput, GitError> {
        self.run_args(vec![
            "push".to_owned(),
            "-u".to_owned(),
            "--".to_owned(),
            remote.to_owned(),
            format!("HEAD:refs/heads/{branch}"),
        ])
    }

    pub fn pull(&self) -> Result<GitOutput, GitError> {
        self.run(["pull", "--ff-only"])
    }

    pub fn pull_ff_only_from(
        &self,
        remote: &str,
        branch: &str,
        expected_upstream_oid: Option<&str>,
    ) -> Result<GitOutput, GitError> {
        let fetch = self.fetch_remote_branch(remote, branch)?;
        self.ensure_remote_tracking_unchanged(remote, branch, expected_upstream_oid, "pull")?;
        let merge = self.run_args(vec![
            "merge".to_owned(),
            "--ff-only".to_owned(),
            remote_tracking_ref(remote, branch),
        ])?;
        Ok(combine_outputs(fetch, merge))
    }

    pub fn pull_rebase(&self) -> Result<GitOutput, GitError> {
        self.run(["pull", "--rebase"])
    }

    pub fn pull_rebase_from(
        &self,
        remote: &str,
        branch: &str,
        expected_upstream_oid: Option<&str>,
    ) -> Result<GitOutput, GitError> {
        let fetch = self.fetch_remote_branch(remote, branch)?;
        self.ensure_remote_tracking_unchanged(
            remote,
            branch,
            expected_upstream_oid,
            "pull rebase",
        )?;
        let rebase = self.run_args(vec![
            "rebase".to_owned(),
            remote_tracking_ref(remote, branch),
        ])?;
        Ok(combine_outputs(fetch, rebase))
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

    pub fn remote_tracking_oid(
        &self,
        remote: &str,
        branch: &str,
    ) -> Result<Option<String>, GitError> {
        self.ref_oid(&remote_tracking_ref(remote, branch))
    }

    fn fetch_remote_branch(&self, remote: &str, branch: &str) -> Result<GitOutput, GitError> {
        self.run_args(vec![
            "fetch".to_owned(),
            "--".to_owned(),
            remote.to_owned(),
            format!(
                "refs/heads/{branch}:{}",
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
        parse_status_bytes(&output.stdout)
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
        let output = Command::new("git")
            .current_dir(&self.cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("SSH_ASKPASS", "")
            .env("SSH_ASKPASS_REQUIRE", "never")
            .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
            .args(&args)
            .output()
            .map_err(|source| GitError::Io {
                args: args.clone(),
                source,
            })?;

        let stdout = String::from_utf8(output.stdout).map_err(|source| GitError::Utf8 {
            args: args.clone(),
            stream: OutputStream::Stdout,
            source,
        })?;
        let stderr = String::from_utf8(output.stderr).map_err(|source| GitError::Utf8 {
            args: args.clone(),
            stream: OutputStream::Stderr,
            source,
        })?;

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
        command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
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
            .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
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

    Ok(WorktreeStatus { branch, entries })
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

        assert_eq!(status.conflicted_files().len(), 1);
        assert_eq!(
            status.conflicted_files()[0].path,
            PathBuf::from("conflict.txt")
        );
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

        Git::new(repo.path()).push_current_branch_set_upstream("origin", "main")?;

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
            let _result = fs::remove_dir_all(&self.path);
        }
    }
}
