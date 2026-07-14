use serde::Deserialize;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

pub const INSTALL_GH_GUIDANCE: &str =
    "GitHub CLI (gh) is not installed. Install it from https://cli.github.com/.";
pub const AUTHENTICATE_GH_GUIDANCE: &str =
    "GitHub CLI is not authenticated. Run `gh auth login` and try again.";

#[derive(Debug, Clone)]
pub struct GitHub {
    cwd: PathBuf,
    executable: PathBuf,
    #[cfg(test)]
    executable_args: Vec<String>,
    repository: Option<String>,
}

impl GitHub {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_executable(cwd, "gh")
    }

    pub fn with_executable(cwd: impl Into<PathBuf>, executable: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            executable: executable.into(),
            #[cfg(test)]
            executable_args: Vec::new(),
            repository: None,
        }
    }

    pub fn with_executable_and_repository(
        cwd: impl Into<PathBuf>,
        executable: impl Into<PathBuf>,
        repository: impl Into<String>,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            executable: executable.into(),
            #[cfg(test)]
            executable_args: Vec::new(),
            repository: Some(repository.into()),
        }
    }

    #[cfg(test)]
    fn with_executable_args(mut self, executable_args: Vec<String>) -> Self {
        self.executable_args = executable_args;
        self
    }

    pub fn setup_status(&self) -> Result<GhSetupStatus, GhError> {
        match self.run_status(vec!["--version".to_owned()]) {
            Ok(()) => {}
            Err(GhError::MissingCli) => return Ok(GhSetupStatus::MissingCli),
            Err(error) => return Err(error),
        }

        match self.run_status(vec![
            "auth".to_owned(),
            "status".to_owned(),
            "--active".to_owned(),
            "--hostname".to_owned(),
            "github.com".to_owned(),
        ]) {
            Ok(()) => Ok(GhSetupStatus::Ready),
            Err(GhError::CommandFailed { .. }) => Ok(GhSetupStatus::NotAuthenticated),
            Err(error) => Err(error),
        }
    }

    pub fn repository(&self) -> Result<Repository, GhError> {
        self.ensure_ready()?;
        self.repository_after_ready()
    }

    pub fn existing_pull_requests(&self, head: &str) -> Result<Vec<PullRequest>, GhError> {
        self.ensure_ready()?;
        let repository = self.repository_after_ready()?;
        let (owner, branch) = match head.split_once(':') {
            Some((owner, branch)) => (owner, branch),
            None => (
                repository
                    .name_with_owner
                    .split_once('/')
                    .map_or("", |(owner, _)| owner),
                head,
            ),
        };
        if owner.is_empty() || branch.is_empty() {
            return Err(GhError::InvalidInput {
                name: "pull request head",
            });
        }
        let output = self.run_output(vec![
            "api".to_owned(),
            "--method".to_owned(),
            "GET".to_owned(),
            "--paginate".to_owned(),
            "--slurp".to_owned(),
            format!("repos/{}/pulls", repository.name_with_owner),
            "-f".to_owned(),
            "state=open".to_owned(),
            "-f".to_owned(),
            format!("head={owner}:{branch}"),
            "-f".to_owned(),
            "per_page=100".to_owned(),
            "--hostname".to_owned(),
            "github.com".to_owned(),
        ])?;
        let pages: Vec<Vec<RestPullRequest>> = parse_json(&output, "pull request query")?;
        Ok(pages.into_iter().flatten().map(PullRequest::from).collect())
    }

    pub fn branch_exists(&self, branch: &str) -> Result<bool, GhError> {
        Ok(self.branch_reference(branch)?.is_some())
    }

    pub fn branch_oid(&self, branch: &str) -> Result<Option<String>, GhError> {
        Ok(self
            .branch_reference(branch)?
            .map(|reference| reference.object.sha))
    }

    fn branch_reference(&self, branch: &str) -> Result<Option<RestReference>, GhError> {
        self.ensure_ready()?;
        if branch.trim().is_empty() {
            return Err(GhError::InvalidInput { name: "branch" });
        }
        let repository = self.repository_after_ready()?;
        let output = self.run_output(vec![
            "api".to_owned(),
            "--method".to_owned(),
            "GET".to_owned(),
            "--paginate".to_owned(),
            "--slurp".to_owned(),
            format!(
                "repos/{}/git/matching-refs/heads/{}",
                repository.name_with_owner,
                url_path_component(branch)
            ),
            "--hostname".to_owned(),
            "github.com".to_owned(),
        ])?;
        let pages: Vec<Vec<RestReference>> = parse_json(&output, "branch query")?;
        let expected = format!("refs/heads/{branch}");
        Ok(pages
            .into_iter()
            .flatten()
            .find(|reference| reference.name == expected))
    }

    fn repository_after_ready(&self) -> Result<Repository, GhError> {
        let mut args = vec!["repo".to_owned(), "view".to_owned()];
        if let Some(repository) = &self.repository {
            args.push(repository.clone());
        }
        args.extend([
            "--json".to_owned(),
            "nameWithOwner,defaultBranchRef".to_owned(),
        ]);
        let output = self.run_output(args)?;
        let repository: RepositoryResponse = parse_json(&output, "repository")?;
        let default_branch = repository
            .default_branch_ref
            .map(|branch| branch.name)
            .filter(|branch| !branch.is_empty())
            .ok_or(GhError::InvalidOutput {
                operation: "repository query",
            })?;
        Ok(Repository {
            name_with_owner: repository.name_with_owner,
            default_branch,
        })
    }

    pub fn create_pull_request(
        &self,
        request: &CreatePullRequest,
    ) -> Result<CreatedPullRequest, GhError> {
        self.ensure_ready()?;
        request.validate()?;
        let output = self.run_output(self.with_repository(vec![
            "pr".to_owned(),
            "create".to_owned(),
            "--title".to_owned(),
            request.title.clone(),
            "--body".to_owned(),
            request.body.clone(),
            "--base".to_owned(),
            request.base.clone(),
            "--head".to_owned(),
            request.head.clone(),
        ]))?;
        let url = output
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .ok_or(GhError::InvalidOutput {
                operation: "pull request creation",
            })?;
        Ok(CreatedPullRequest {
            url: url.to_owned(),
        })
    }

    fn ensure_ready(&self) -> Result<(), GhError> {
        match self.setup_status()? {
            GhSetupStatus::Ready => Ok(()),
            GhSetupStatus::MissingCli => Err(GhError::MissingCli),
            GhSetupStatus::NotAuthenticated => Err(GhError::NotAuthenticated),
        }
    }

    fn with_repository(&self, mut args: Vec<String>) -> Vec<String> {
        if let Some(repository) = &self.repository {
            args.push("--repo".to_owned());
            args.push(repository.clone());
        }
        args
    }

    fn run_status(&self, args: Vec<String>) -> Result<(), GhError> {
        let output = self.command(&args).output().map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                GhError::MissingCli
            } else {
                GhError::Io { source }
            }
        })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(GhError::CommandFailed {
                status: output.status,
            })
        }
    }

    fn run_output(&self, args: Vec<String>) -> Result<String, GhError> {
        let output = self.command(&args).output().map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                GhError::MissingCli
            } else {
                GhError::Io { source }
            }
        })?;
        if !output.status.success() {
            return Err(GhError::CommandFailed {
                status: output.status,
            });
        }
        String::from_utf8(output.stdout).map_err(|_| GhError::InvalidOutput {
            operation: "GitHub CLI command",
        })
    }

    fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .current_dir(&self.cwd)
            .env("GH_PROMPT_DISABLED", "1");
        #[cfg(test)]
        command.args(&self.executable_args);
        command.args(args);
        command
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GhSetupStatus {
    Ready,
    MissingCli,
    NotAuthenticated,
}

impl GhSetupStatus {
    pub const fn guidance(self) -> Option<&'static str> {
        match self {
            Self::Ready => None,
            Self::MissingCli => Some(INSTALL_GH_GUIDANCE),
            Self::NotAuthenticated => Some(AUTHENTICATE_GH_GUIDANCE),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub name_with_owner: String,
    pub default_branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub title: String,
    pub base_ref_name: String,
    pub head_ref_name: String,
    pub head_repository: Option<PullRequestRepository>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestRepository {
    pub name_with_owner: String,
}

#[derive(Deserialize)]
struct RestPullRequest {
    number: u64,
    html_url: String,
    title: String,
    base: RestPullRequestBranch,
    head: RestPullRequestBranch,
}

#[derive(Deserialize)]
struct RestPullRequestBranch {
    #[serde(rename = "ref")]
    reference: String,
    repo: Option<RestPullRequestRepository>,
}

#[derive(Deserialize)]
struct RestPullRequestRepository {
    full_name: String,
}

#[derive(Deserialize)]
struct RestReference {
    #[serde(rename = "ref")]
    name: String,
    object: RestReferenceObject,
}

#[derive(Deserialize)]
struct RestReferenceObject {
    sha: String,
}

impl From<RestPullRequest> for PullRequest {
    fn from(pull_request: RestPullRequest) -> Self {
        Self {
            number: pull_request.number,
            url: pull_request.html_url,
            title: pull_request.title,
            base_ref_name: pull_request.base.reference,
            head_ref_name: pull_request.head.reference,
            head_repository: pull_request
                .head
                .repo
                .map(|repository| PullRequestRepository {
                    name_with_owner: repository.full_name,
                }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatePullRequest {
    pub title: String,
    pub body: String,
    pub base: String,
    pub head: String,
}

impl CreatePullRequest {
    fn validate(&self) -> Result<(), GhError> {
        for (name, value) in [
            ("title", &self.title),
            ("base branch", &self.base),
            ("head branch", &self.head),
        ] {
            if value.trim().is_empty() {
                return Err(GhError::InvalidInput { name });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedPullRequest {
    pub url: String,
}

#[derive(Debug)]
pub enum GhError {
    MissingCli,
    NotAuthenticated,
    Io { source: io::Error },
    CommandFailed { status: ExitStatus },
    InvalidInput { name: &'static str },
    InvalidOutput { operation: &'static str },
}

impl Display for GhError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCli => formatter.write_str(INSTALL_GH_GUIDANCE),
            Self::NotAuthenticated => formatter.write_str(AUTHENTICATE_GH_GUIDANCE),
            Self::Io { source } => write!(formatter, "failed to run GitHub CLI: {source}"),
            Self::CommandFailed { status } => {
                write!(formatter, "GitHub CLI failed with status {status}")
            }
            Self::InvalidInput { name } => {
                write!(formatter, "pull request {name} must not be empty")
            }
            Self::InvalidOutput { operation } => {
                write!(
                    formatter,
                    "GitHub CLI returned invalid output for {operation}"
                )
            }
        }
    }
}

impl Error for GhError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source } => Some(source),
            Self::MissingCli
            | Self::NotAuthenticated
            | Self::CommandFailed { .. }
            | Self::InvalidInput { .. }
            | Self::InvalidOutput { .. } => None,
        }
    }
}

#[derive(Deserialize)]
struct RepositoryResponse {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
    #[serde(rename = "defaultBranchRef")]
    default_branch_ref: Option<BranchReference>,
}

#[derive(Deserialize)]
struct BranchReference {
    name: String,
}

fn parse_json<T: for<'de> Deserialize<'de>>(
    output: &str,
    operation: &'static str,
) -> Result<T, GhError> {
    serde_json::from_str(output).map_err(|_| GhError::InvalidOutput { operation })
}

fn url_path_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => {
                encoded.push('%');
                encoded.push(char::from(HEX[usize::from(byte >> 4)]));
                encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
    }
    encoded
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_FAKE_ID: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn missing_cli_has_install_guidance() -> Result<(), Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!(
            "bitbygit-missing-gh-{}-{}",
            std::process::id(),
            NEXT_FAKE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let github = GitHub::with_executable(".", path);

        let status = github.setup_status()?;

        assert_eq!(status, GhSetupStatus::MissingCli);
        assert_eq!(status.guidance(), Some(INSTALL_GH_GUIDANCE));
        Ok(())
    }

    #[test]
    fn auth_failure_has_login_guidance() -> Result<(), Box<dyn Error>> {
        let fake =
            FakeGh::new("case \"$1\" in\n--version) exit 0 ;;\nauth) exit 1 ;;\nesac\nexit 1")?;

        let status = fake.github().setup_status()?;

        assert_eq!(status, GhSetupStatus::NotAuthenticated);
        assert_eq!(status.guidance(), Some(AUTHENTICATE_GH_GUIDANCE));
        assert_eq!(fake.prompt_values()?, "1\n1\n");
        Ok(())
    }

    #[test]
    fn auth_check_ignores_a_stale_account_on_another_host() -> Result<(), Box<dyn Error>> {
        let fake = FakeGh::new(
            "case \"$1:$2\" in\n--version:*) exit 0 ;;\nauth:status) case \"$3:$4:$5\" in\n--active:--hostname:github.com) exit 0 ;;\n*) exit 1 ;;\nesac ;;\nrepo:view) printf '%s\\n' '{\"nameWithOwner\":\"octo/repo\",\"defaultBranchRef\":{\"name\":\"main\"}}' ;;\nesac",
        )?;

        let repository = fake.github().repository()?;

        assert_eq!(repository.name_with_owner, "octo/repo");
        assert!(
            fake.invocations()?
                .contains("auth\u{1f}status\u{1f}--active\u{1f}--hostname\u{1f}github.com\u{1f}\n")
        );
        Ok(())
    }

    #[test]
    fn setup_checks_do_not_expose_command_output() -> Result<(), Box<dyn Error>> {
        const MARKER: &str = "setup-check-output-must-not-be-exposed";
        const CHILD_ENV: &str = "BITBYGIT_GH_SETUP_OUTPUT_CHILD";

        if std::env::var_os(CHILD_ENV).is_some() {
            let fake = FakeGh::new(&format!(
                "case \"$1:$2\" in\n--version:*) printf '%s\\n' '{MARKER}'; printf '%s\\n' '{MARKER}' >&2; exit 0 ;;\nauth:status) printf '%s\\n' '{MARKER}'; printf '%s\\n' '{MARKER}' >&2; exit 0 ;;\nrepo:view) printf '%s\\n' '{{\"nameWithOwner\":\"octo/repo\",\"defaultBranchRef\":{{\"name\":\"main\"}}}}' ;;\nesac"
            ))?;

            fake.github().repository()?;
            return Ok(());
        }

        let output = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "tests::setup_checks_do_not_expose_command_output",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .output()?;

        assert!(output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains(MARKER));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(MARKER));
        Ok(())
    }

    #[test]
    fn decodes_rest_existing_pull_request_responses() -> Result<(), Box<dyn Error>> {
        let fake = FakeGh::new(
            "case \"$1:$2\" in\n--version:*) exit 0 ;;\nauth:status) exit 0 ;;\nrepo:view) [ \"$3\" = github.com/octo/repo ] && [ \"$4\" = --json ] && [ \"$5\" = nameWithOwner,defaultBranchRef ] || exit 1; printf '%s\\n' '{\"nameWithOwner\":\"octo/repo\",\"defaultBranchRef\":{\"name\":\"main\"}}' ;;\napi:--method) [ \"$3\" = GET ] && [ \"$4\" = --paginate ] && [ \"$5\" = --slurp ] && [ \"$6\" = repos/octo/repo/pulls ] && [ \"$7\" = -f ] && [ \"$8\" = state=open ] && [ \"$9\" = -f ] && [ \"${10}\" = head=octo:feature ] && [ \"${11}\" = -f ] && [ \"${12}\" = per_page=100 ] && [ \"${13}\" = --hostname ] && [ \"${14}\" = github.com ] || exit 1; printf '%s\\n' '[[{\"number\":42,\"html_url\":\"https://github.com/octo/repo/pull/42\",\"title\":\"Existing PR\",\"base\":{\"ref\":\"main\"},\"head\":{\"ref\":\"feature\",\"repo\":{\"full_name\":\"octo/repo\"}}}]]' ;;\n*) exit 1 ;;\nesac",
        )?;
        let github =
            GitHub::with_executable_and_repository(&fake.path, "/bin/sh", "github.com/octo/repo")
                .with_executable_args(vec![fake.executable.display().to_string()]);

        let repository = github.repository()?;
        let pull_requests = github.existing_pull_requests("feature")?;

        assert_eq!(repository.name_with_owner, "octo/repo");
        assert_eq!(repository.default_branch, "main");
        assert_eq!(pull_requests.len(), 1);
        assert_eq!(pull_requests[0].number, 42);
        assert_eq!(pull_requests[0].url, "https://github.com/octo/repo/pull/42");
        assert_eq!(pull_requests[0].base_ref_name, "main");
        assert_eq!(pull_requests[0].head_ref_name, "feature");
        assert_eq!(
            pull_requests[0]
                .head_repository
                .as_ref()
                .map(|repository| repository.name_with_owner.as_str()),
            Some("octo/repo")
        );
        assert!(fake.invocations()?.contains(
            "repo\u{1f}view\u{1f}github.com/octo/repo\u{1f}--json\u{1f}nameWithOwner,defaultBranchRef\u{1f}\n"
        ));
        assert!(fake.invocations()?.contains(
            "api\u{1f}--method\u{1f}GET\u{1f}--paginate\u{1f}--slurp\u{1f}repos/octo/repo/pulls\u{1f}-f\u{1f}state=open\u{1f}-f\u{1f}head=octo:feature\u{1f}-f\u{1f}per_page=100\u{1f}--hostname\u{1f}github.com\u{1f}\n"
        ));
        assert_eq!(fake.prompt_values()?, "1\n1\n1\n1\n1\n1\n1\n");
        Ok(())
    }

    #[test]
    fn checks_encoded_base_branch_reference() -> Result<(), Box<dyn Error>> {
        let fake = FakeGh::new(
            "case \"$1:$2\" in\n--version:*) exit 0 ;;\nauth:status) exit 0 ;;\nrepo:view) printf '%s\\n' '{\"nameWithOwner\":\"octo/repo\",\"defaultBranchRef\":{\"name\":\"main\"}}' ;;\napi:--method) [ \"$6\" = repos/octo/repo/git/matching-refs/heads/release%2Fnext ] || exit 1; printf '%s\\n' '[[{\"ref\":\"refs/heads/release/next\",\"object\":{\"sha\":\"abc123\"}}]]' ;;\n*) exit 1 ;;\nesac",
        )?;

        assert!(fake.github().branch_exists("release/next")?);
        assert_eq!(
            fake.github().branch_oid("release/next")?.as_deref(),
            Some("abc123")
        );
        Ok(())
    }

    #[test]
    fn rest_queries_pin_github_com_when_gh_host_conflicts() -> Result<(), Box<dyn Error>> {
        let fake = FakeGh::new(
            "GH_HOST=example.com\ncase \"$1:$2\" in\n--version:*) exit 0 ;;\nauth:status) exit 0 ;;\nrepo:view) printf '%s\\n' '{\"nameWithOwner\":\"octo/repo\",\"defaultBranchRef\":{\"name\":\"main\"}}' ;;\napi:--method) case \"$6\" in\n*/pulls) [ \"${13}\" = --hostname ] && [ \"${14}\" = github.com ] || exit 1; printf '%s\\n' '[[]]' ;;\n*/git/matching-refs/heads/*) [ \"$7\" = --hostname ] && [ \"$8\" = github.com ] || exit 1; printf '%s\\n' '[[]]' ;;\n*) exit 1 ;;\nesac ;;\n*) exit 1 ;;\nesac",
        )?;

        assert!(fake.github().existing_pull_requests("feature")?.is_empty());
        assert!(!fake.github().branch_exists("main")?);
        Ok(())
    }

    #[test]
    fn creates_pull_request_with_literal_shell_metacharacters_in_title()
    -> Result<(), Box<dyn Error>> {
        let fake = FakeGh::new(
            "case \"$1:$2\" in\n--version:*) exit 0 ;;\nauth:status) exit 0 ;;\npr:create) printf '%s\\n' 'https://github.com/octo/repo/pull/43' ;;\nesac",
        )?;
        let injected_file = fake.path.join("should-not-exist");
        let title = format!("release $(touch {}) ; | &", injected_file.display());
        let request = CreatePullRequest {
            title: title.clone(),
            body: "Ship it".to_owned(),
            base: "main".to_owned(),
            head: "feature".to_owned(),
        };

        let created = fake.github().create_pull_request(&request)?;

        assert_eq!(created.url, "https://github.com/octo/repo/pull/43");
        assert!(!injected_file.exists());
        assert!(fake.invocations()?.contains(&format!(
            "pr\u{1f}create\u{1f}--title\u{1f}{title}\u{1f}--body\u{1f}Ship it\u{1f}--base\u{1f}main\u{1f}--head\u{1f}feature\u{1f}\n"
        )));
        assert_eq!(fake.prompt_values()?, "1\n1\n1\n");
        Ok(())
    }

    #[test]
    fn fake_gh_executes_reliably_in_parallel() -> Result<(), Box<dyn Error>> {
        let threads = (0..32)
            .map(|_| {
                std::thread::spawn(|| -> Result<(), String> {
                    let fake = FakeGh::new(
                        "case \"$1:$2\" in\n--version:*) exit 0 ;;\nauth:status) exit 0 ;;\nesac\nexit 1",
                    )
                    .map_err(|error| error.to_string())?;
                    if fake.github().setup_status().map_err(|error| error.to_string())?
                        != GhSetupStatus::Ready
                    {
                        return Err("fake GitHub CLI was not ready".to_owned());
                    }
                    Ok(())
                })
            })
            .collect::<Vec<_>>();

        for thread in threads {
            thread
                .join()
                .map_err(|_| io::Error::other("fake GitHub CLI thread panicked"))?
                .map_err(io::Error::other)?;
        }
        Ok(())
    }

    struct FakeGh {
        path: PathBuf,
        executable: PathBuf,
        invocations: PathBuf,
        prompt_values: PathBuf,
    }

    impl FakeGh {
        fn new(body: &str) -> Result<Self, Box<dyn Error>> {
            let id = NEXT_FAKE_ID.fetch_add(1, Ordering::Relaxed);
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "bitbygit-fake-gh-{}-{id}-{nonce}",
                std::process::id()
            ));
            if path.exists() {
                fs::remove_dir_all(&path)?;
            }
            fs::create_dir_all(&path)?;
            let executable = path.join("gh");
            let invocations = path.join("invocations");
            let prompt_values = path.join("prompt-values");
            fs::write(
                &executable,
                format!(
                    "#!/bin/sh\nprintf '%s\\037' \"$@\" >> '{}'\nprintf '\\n' >> '{}'\nprintf '%s\\n' \"$GH_PROMPT_DISABLED\" >> '{}'\n{body}\n",
                    invocations.display(),
                    invocations.display(),
                    prompt_values.display(),
                ),
            )?;
            Ok(Self {
                path,
                executable,
                invocations,
                prompt_values,
            })
        }

        fn github(&self) -> GitHub {
            GitHub::with_executable(&self.path, "/bin/sh")
                .with_executable_args(vec![self.executable.display().to_string()])
        }

        fn invocations(&self) -> Result<String, io::Error> {
            fs::read_to_string(&self.invocations)
        }

        fn prompt_values(&self) -> Result<String, io::Error> {
            fs::read_to_string(&self.prompt_values)
        }
    }

    impl Drop for FakeGh {
        fn drop(&mut self) {
            let _result = fs::remove_dir_all(&self.path);
        }
    }
}
