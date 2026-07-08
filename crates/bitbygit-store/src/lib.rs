use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::ffi::OsString;
use std::fmt::{self, Display, Formatter};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorePaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub registry_file: PathBuf,
    pub state_file: PathBuf,
    pub audit_file: PathBuf,
}

impl StorePaths {
    pub fn from_roots(config_dir: impl Into<PathBuf>, data_dir: impl Into<PathBuf>) -> Self {
        let config_dir = config_dir.into();
        let data_dir = data_dir.into();
        Self {
            config_dir,
            registry_file: data_dir.join("registry.tsv"),
            state_file: data_dir.join("state.tsv"),
            audit_file: data_dir.join("audit.tsv"),
            data_dir,
        }
    }

    pub fn from_environment() -> Result<Self, StoreError> {
        let config_dir = env_path("BITBYGIT_CONFIG_DIR")
            .or_else(|| env_path("XDG_CONFIG_HOME").map(|path| path.join("bitbygit")))
            .or_else(|| env_path("APPDATA").map(|path| path.join("bitbygit")))
            .or_else(|| home_dir().map(|path| path.join(".config").join("bitbygit")))
            .ok_or(StoreError::HomeDirectoryUnavailable)?;

        let data_dir = env_path("BITBYGIT_DATA_DIR")
            .or_else(|| env_path("XDG_DATA_HOME").map(|path| path.join("bitbygit")))
            .or_else(|| env_path("APPDATA").map(|path| path.join("bitbygit").join("data")))
            .or_else(|| home_dir().map(|path| path.join(".local").join("share").join("bitbygit")))
            .ok_or(StoreError::HomeDirectoryUnavailable)?;

        Ok(Self::from_roots(config_dir, data_dir))
    }
}

#[derive(Debug, Clone)]
pub struct LocalStore {
    paths: StorePaths,
}

impl LocalStore {
    pub fn open(paths: StorePaths) -> Result<Self, StoreError> {
        fs::create_dir_all(&paths.config_dir).map_err(|source| StoreError::Io {
            path: paths.config_dir.clone(),
            source,
        })?;
        fs::create_dir_all(&paths.data_dir).map_err(|source| StoreError::Io {
            path: paths.data_dir.clone(),
            source,
        })?;
        Ok(Self { paths })
    }

    pub fn paths(&self) -> &StorePaths {
        &self.paths
    }

    pub fn add_repository(&self, path: impl AsRef<Path>) -> Result<RepositoryRecord, StoreError> {
        let root = bitbygit_git::Git::new(path.as_ref())
            .repo_root()
            .map_err(|source| StoreError::InvalidRepository {
                path: path.as_ref().to_path_buf(),
                message: source.to_string(),
            })?;
        let now = now_secs()?;
        let id = RepoId::from_path(&root);
        let mut records = self.load_registry_map()?;
        let mut state = self.load_state()?;
        let old_records = records.clone();
        let record = records
            .entry(id.clone())
            .and_modify(|record| {
                record.path = root.clone();
                record.last_seen_at = now;
            })
            .or_insert_with(|| RepositoryRecord {
                id,
                path: root,
                added_at: now,
                last_seen_at: now,
            })
            .clone();
        push_recent(&mut state.recent_repos, record.id.clone());

        self.save_registry(records.into_values())?;
        if let Err(error) = self.save_state(&state) {
            self.save_registry(old_records.into_values())?;
            return Err(error);
        }
        Ok(record)
    }

    pub fn remove_repository(&self, id: &RepoId) -> Result<Option<RepositoryRecord>, StoreError> {
        let mut records = self.load_registry_map()?;
        let mut state = self.load_state()?;
        let old_records = records.clone();
        let removed = records.remove(id);
        if state.active_repo.as_ref() == Some(id) {
            state.active_repo = None;
        }
        state.recent_repos.retain(|repo_id| repo_id != id);

        self.save_registry(records.into_values())?;
        if let Err(error) = self.save_state(&state) {
            self.save_registry(old_records.into_values())?;
            return Err(error);
        }

        Ok(removed)
    }

    pub fn list_repositories(&self) -> Result<Vec<RepositoryRecord>, StoreError> {
        Ok(self.load_registry_map()?.into_values().collect())
    }

    /// Checks every registered repository synchronously.
    ///
    /// TUI callers should prefer `repository_status_by_id` from a background
    /// task or a lazy viewport-specific refresh path when many repositories are
    /// registered.
    pub fn list_repository_statuses(&self) -> Result<Vec<RepositoryStatus>, StoreError> {
        self.list_repositories()?
            .into_iter()
            .map(|record| self.repository_status(record))
            .collect()
    }

    pub fn repository_status_by_id(&self, id: &RepoId) -> Result<RepositoryStatus, StoreError> {
        let records = self.load_registry_map()?;
        let record = records
            .get(id)
            .cloned()
            .ok_or_else(|| StoreError::UnknownRepository { id: id.clone() })?;
        self.repository_status(record)
    }

    pub fn repository_status(
        &self,
        record: RepositoryRecord,
    ) -> Result<RepositoryStatus, StoreError> {
        let status = match bitbygit_git::Git::new(&record.path).repo_root() {
            Ok(root) if root == record.path => RepositoryHealth::Valid,
            Ok(root) => RepositoryHealth::Invalid {
                message: format!("repository root moved to {}", root.display()),
            },
            Err(error) => RepositoryHealth::Invalid {
                message: error.to_string(),
            },
        };
        Ok(RepositoryStatus { record, status })
    }

    pub fn load_state(&self) -> Result<AppState, StoreError> {
        let contents = read_optional(&self.paths.state_file)?;
        let Some(contents) = contents else {
            return Ok(AppState::default());
        };
        parse_state(&contents).map_err(|error| attach_parse_path(error, &self.paths.state_file))
    }

    pub fn set_active_repository(&self, id: Option<RepoId>) -> Result<(), StoreError> {
        if let Some(id) = &id {
            self.require_registered(id)?;
        }

        let mut state = self.load_state()?;
        state.active_repo = id.clone();
        if let Some(id) = id {
            push_recent(&mut state.recent_repos, id);
        }
        self.save_state(&state)
    }

    pub fn append_audit(&self, entry: AuditEntry) -> Result<(), StoreError> {
        if let Some(parent) = self.paths.audit_file.parent() {
            fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let line = format_audit_entry(&entry);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.paths.audit_file)
            .map_err(|source| StoreError::Io {
                path: self.paths.audit_file.clone(),
                source,
            })?;
        file.write_all(line.as_bytes())
            .map_err(|source| StoreError::Io {
                path: self.paths.audit_file.clone(),
                source,
            })
    }

    pub fn list_audit_entries(&self) -> Result<Vec<AuditEntry>, StoreError> {
        let contents = read_optional(&self.paths.audit_file)?;
        let Some(contents) = contents else {
            return Ok(Vec::new());
        };
        parse_audit_entries(&contents)
            .map_err(|error| attach_parse_path(error, &self.paths.audit_file))
    }

    fn save_state(&self, state: &AppState) -> Result<(), StoreError> {
        write_atomic(&self.paths.state_file, &format_state(state))
    }

    fn require_registered(&self, id: &RepoId) -> Result<(), StoreError> {
        if self.load_registry_map()?.contains_key(id) {
            return Ok(());
        }

        Err(StoreError::UnknownRepository { id: id.clone() })
    }

    fn load_registry_map(&self) -> Result<BTreeMap<RepoId, RepositoryRecord>, StoreError> {
        let contents = read_optional(&self.paths.registry_file)?;
        let Some(contents) = contents else {
            return Ok(BTreeMap::new());
        };
        parse_registry(&contents)
            .map_err(|error| attach_parse_path(error, &self.paths.registry_file))
    }

    fn save_registry(
        &self,
        records: impl IntoIterator<Item = RepositoryRecord>,
    ) -> Result<(), StoreError> {
        write_atomic(&self.paths.registry_file, &format_registry(records))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoId(String);

impl RepoId {
    pub fn parse(value: impl Into<String>) -> Result<Self, StoreError> {
        let value = value.into();
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(parse_store_error(
                "repository id contains invalid characters".to_owned(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_path(path: &Path) -> Self {
        Self(format!("repo-{:016x}", fnv1a(path_bytes(path))))
    }
}

impl Display for RepoId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryRecord {
    pub id: RepoId,
    pub path: PathBuf,
    pub added_at: u64,
    pub last_seen_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryStatus {
    pub record: RepositoryRecord,
    pub status: RepositoryHealth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryHealth {
    Valid,
    Invalid { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppState {
    pub active_repo: Option<RepoId>,
    pub recent_repos: Vec<RepoId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub timestamp: u64,
    pub repo_id: Option<RepoId>,
    pub operation: String,
    pub result: String,
    pub message: String,
}

impl AuditEntry {
    pub fn new(
        repo_id: Option<RepoId>,
        operation: impl Into<String>,
        result: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            timestamp: now_secs()?,
            repo_id,
            operation: operation.into(),
            result: result.into(),
            message: message.into(),
        })
    }
}

#[derive(Debug)]
pub enum StoreError {
    HomeDirectoryUnavailable,
    ClockBeforeUnixEpoch,
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: Option<PathBuf>,
        message: String,
    },
    InvalidRepository {
        path: PathBuf,
        message: String,
    },
    UnknownRepository {
        id: RepoId,
    },
}

impl Display for StoreError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeDirectoryUnavailable => formatter.write_str("home directory is unavailable"),
            Self::ClockBeforeUnixEpoch => formatter.write_str("system clock is before Unix epoch"),
            Self::Io { path, source } => {
                write!(formatter, "failed to access {}: {source}", path.display())
            }
            Self::Parse { path, message } => {
                if let Some(path) = path {
                    write!(formatter, "failed to parse {}: {message}", path.display())
                } else {
                    write!(formatter, "failed to parse store data: {message}")
                }
            }
            Self::InvalidRepository { path, message } => {
                write!(
                    formatter,
                    "{} is not a usable Git repository: {message}",
                    path.display()
                )
            }
            Self::UnknownRepository { id } => write!(formatter, "unknown repository id {id}"),
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::HomeDirectoryUnavailable
            | Self::ClockBeforeUnixEpoch
            | Self::Parse { .. }
            | Self::InvalidRepository { .. }
            | Self::UnknownRepository { .. } => None,
        }
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn now_secs() -> Result<u64, StoreError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| StoreError::ClockBeforeUnixEpoch)
}

fn push_recent(recent: &mut Vec<RepoId>, id: RepoId) {
    let mut queue = VecDeque::from(std::mem::take(recent));
    queue.retain(|repo_id| repo_id != &id);
    queue.push_front(id);
    while queue.len() > 50 {
        let _removed = queue.pop_back();
    }
    *recent = queue.into();
}

fn read_optional(path: &Path) -> Result<Option<String>, StoreError> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(StoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn write_atomic(path: &Path, contents: &str) -> Result<(), StoreError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| StoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let mut last_error = None;
    for attempt in 0..100_u8 {
        let tmp_path = unique_tmp_path(path, attempt);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                last_error = Some(error);
                continue;
            }
            Err(source) => {
                return Err(StoreError::Io {
                    path: tmp_path,
                    source,
                });
            }
        };

        if let Err(source) = file.write_all(contents.as_bytes()) {
            let _cleanup = fs::remove_file(&tmp_path);
            return Err(StoreError::Io {
                path: tmp_path,
                source,
            });
        }

        if let Err(source) = file.sync_all() {
            let _cleanup = fs::remove_file(&tmp_path);
            return Err(StoreError::Io {
                path: tmp_path,
                source,
            });
        }

        if let Err(source) = fs::rename(&tmp_path, path) {
            let _cleanup = fs::remove_file(&tmp_path);
            return Err(StoreError::Io {
                path: path.to_path_buf(),
                source,
            });
        }

        sync_parent_dir(path)?;

        return Ok(());
    }

    Err(StoreError::Io {
        path: path.to_path_buf(),
        source: last_error.unwrap_or_else(|| {
            std::io::Error::new(
                ErrorKind::AlreadyExists,
                "temporary store file already exists",
            )
        }),
    })
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> Result<(), StoreError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let directory = fs::File::open(parent).map_err(|source| StoreError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    directory.sync_all().map_err(|source| StoreError::Io {
        path: parent.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

fn unique_tmp_path(path: &Path, attempt: u8) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "store".into());
    path.with_file_name(format!(".{file_name}.tmp-{}-{attempt}", std::process::id()))
}

fn parse_registry(contents: &str) -> Result<BTreeMap<RepoId, RepositoryRecord>, StoreError> {
    let mut records = BTreeMap::new();
    for (index, line) in contents.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 4 {
            return Err(parse_store_error(format!(
                "registry line {} has {} fields",
                index + 1,
                fields.len()
            )));
        }
        let id = RepoId::parse(fields[0])?;
        let path = decode_path(fields[1])?;
        let added_at = parse_u64(fields[2], "added_at")?;
        let last_seen_at = parse_u64(fields[3], "last_seen_at")?;
        let expected_id = RepoId::from_path(&path);
        if id != expected_id {
            return Err(parse_store_error(format!(
                "registry line {} id does not match path",
                index + 1
            )));
        }
        let record = RepositoryRecord {
            id: id.clone(),
            path,
            added_at,
            last_seen_at,
        };
        if records.insert(id, record).is_some() {
            return Err(parse_store_error(format!(
                "registry line {} duplicates repository id",
                index + 1
            )));
        }
    }
    Ok(records)
}

fn format_registry(records: impl IntoIterator<Item = RepositoryRecord>) -> String {
    let mut output = String::new();
    for record in records {
        output.push_str(record.id.as_str());
        output.push('\t');
        output.push_str(&encode_path(&record.path));
        output.push('\t');
        output.push_str(&record.added_at.to_string());
        output.push('\t');
        output.push_str(&record.last_seen_at.to_string());
        output.push('\n');
    }
    output
}

fn parse_state(contents: &str) -> Result<AppState, StoreError> {
    let mut state = AppState::default();
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("active\t") {
            if !value.is_empty() {
                state.active_repo = Some(RepoId::parse(value)?);
            }
            continue;
        }

        if let Some(value) = line.strip_prefix("recent\t") {
            state.recent_repos = value
                .split(',')
                .filter(|value| !value.is_empty())
                .map(RepoId::parse)
                .collect::<Result<Vec<_>, _>>()?;
            continue;
        }

        if !line.is_empty() {
            return Err(parse_store_error(format!("unknown state line: {line}")));
        }
    }
    Ok(state)
}

fn format_state(state: &AppState) -> String {
    let active = state.active_repo.as_ref().map(RepoId::as_str).unwrap_or("");
    let recent = state
        .recent_repos
        .iter()
        .map(RepoId::as_str)
        .collect::<Vec<_>>()
        .join(",");
    format!("active\t{active}\nrecent\t{recent}\n")
}

fn parse_audit_entries(contents: &str) -> Result<Vec<AuditEntry>, StoreError> {
    let mut entries = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 5 {
            return Err(parse_store_error(format!(
                "audit line {} has {} fields",
                index + 1,
                fields.len()
            )));
        }
        entries.push(AuditEntry {
            timestamp: parse_u64(fields[0], "timestamp")?,
            repo_id: if fields[1] == "-" {
                None
            } else {
                Some(RepoId::parse(fields[1])?)
            },
            operation: decode_string(fields[2])?,
            result: decode_string(fields[3])?,
            message: decode_string(fields[4])?,
        });
    }
    Ok(entries)
}

fn format_audit_entry(entry: &AuditEntry) -> String {
    let mut output = String::new();
    output.push_str(&entry.timestamp.to_string());
    output.push('\t');
    output.push_str(entry.repo_id.as_ref().map(RepoId::as_str).unwrap_or("-"));
    output.push('\t');
    output.push_str(&encode_string(&entry.operation));
    output.push('\t');
    output.push_str(&encode_string(&entry.result));
    output.push('\t');
    output.push_str(&encode_string(&entry.message));
    output.push('\n');
    output
}

fn parse_u64(value: &str, label: &str) -> Result<u64, StoreError> {
    value
        .parse::<u64>()
        .map_err(|_| parse_store_error(format!("{label} is not a number")))
}

fn parse_store_error(message: String) -> StoreError {
    StoreError::Parse {
        path: None,
        message,
    }
}

fn attach_parse_path(error: StoreError, file_path: &Path) -> StoreError {
    match error {
        StoreError::Parse {
            path: None,
            message,
        } => StoreError::Parse {
            path: Some(file_path.to_path_buf()),
            message,
        },
        other => other,
    }
}

fn encode_string(value: &str) -> String {
    encode_bytes(value.as_bytes())
}

fn decode_string(value: &str) -> Result<String, StoreError> {
    String::from_utf8(decode_bytes(value)?)
        .map_err(|_| parse_store_error("string is not UTF-8".to_owned()))
}

fn encode_path(path: &Path) -> String {
    encode_bytes(path_bytes(path))
}

fn decode_path(value: &str) -> Result<PathBuf, StoreError> {
    Ok(path_from_bytes(&decode_bytes(value)?))
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> &[u8] {
    path.to_string_lossy().as_bytes()
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

fn encode_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(hex_digit(byte >> 4));
        output.push(hex_digit(byte & 0x0f));
    }
    output
}

fn decode_bytes(value: &str) -> Result<Vec<u8>, StoreError> {
    if value.len() % 2 != 0 {
        return Err(parse_store_error("hex value has odd length".to_owned()));
    }

    let mut bytes = Vec::with_capacity(value.len() / 2);
    for chunk in value.as_bytes().chunks(2) {
        let high = parse_hex_digit(chunk[0])?;
        let low = parse_hex_digit(chunk[1])?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        10..=15 => char::from(b'a' + value - 10),
        _ => '0',
    }
}

fn parse_hex_digit(value: u8) -> Result<u8, StoreError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(parse_store_error("invalid hex digit".to_owned())),
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn persists_and_reloads_repository_registry() -> Result<(), Box<dyn Error>> {
        let fixture = Fixture::new()?;
        let repo = fixture.git_repo("repo")?;
        let store = fixture.store()?;

        let record = store.add_repository(repo.path())?;
        let reloaded = fixture.store()?.list_repositories()?;

        assert_eq!(reloaded, vec![record]);
        Ok(())
    }

    #[test]
    fn active_and_recent_state_survives_reload() -> Result<(), Box<dyn Error>> {
        let fixture = Fixture::new()?;
        let repo = fixture.git_repo("repo")?;
        let store = fixture.store()?;
        let record = store.add_repository(repo.path())?;

        store.set_active_repository(Some(record.id.clone()))?;
        let state = fixture.store()?.load_state()?;

        assert_eq!(state.active_repo, Some(record.id.clone()));
        assert_eq!(state.recent_repos, vec![record.id]);
        Ok(())
    }

    #[test]
    fn removing_repository_never_deletes_repo_from_disk() -> Result<(), Box<dyn Error>> {
        let fixture = Fixture::new()?;
        let repo = fixture.git_repo("repo")?;
        let store = fixture.store()?;
        let record = store.add_repository(repo.path())?;

        let removed = store.remove_repository(&record.id)?;

        assert_eq!(removed, Some(record));
        assert!(repo.path().exists());
        assert!(store.list_repositories()?.is_empty());
        Ok(())
    }

    #[test]
    fn invalid_add_returns_recoverable_error() -> Result<(), Box<dyn Error>> {
        let fixture = Fixture::new()?;
        let path = fixture.path.join("not-a-repo");
        fs::create_dir_all(&path)?;

        let result = fixture.store()?.add_repository(&path);

        assert!(matches!(result, Err(StoreError::InvalidRepository { .. })));
        Ok(())
    }

    #[test]
    fn moved_repository_is_reported_as_invalid_without_breaking_list() -> Result<(), Box<dyn Error>>
    {
        let fixture = Fixture::new()?;
        let repo = fixture.git_repo("repo")?;
        let store = fixture.store()?;
        let record = store.add_repository(repo.path())?;
        fs::rename(repo.path(), fixture.path.join("moved"))?;

        let statuses = store.list_repository_statuses()?;

        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].record.id, record.id);
        assert!(matches!(
            statuses[0].status,
            RepositoryHealth::Invalid { .. }
        ));
        Ok(())
    }

    #[test]
    fn repository_status_by_id_checks_one_registered_repo() -> Result<(), Box<dyn Error>> {
        let fixture = Fixture::new()?;
        let repo = fixture.git_repo("repo")?;
        let store = fixture.store()?;
        let record = store.add_repository(repo.path())?;

        let status = store.repository_status_by_id(&record.id)?;

        assert_eq!(status.record.id, record.id);
        assert_eq!(status.status, RepositoryHealth::Valid);
        Ok(())
    }

    #[test]
    fn audit_entries_are_persisted() -> Result<(), Box<dyn Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        let entry = AuditEntry::new(None, "refresh", "ok", "ready")?;

        store.append_audit(entry.clone())?;

        assert_eq!(fixture.store()?.list_audit_entries()?, vec![entry]);
        Ok(())
    }

    #[test]
    fn corrupt_state_parse_error_includes_file_path() -> Result<(), Box<dyn Error>> {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        fs::write(&store.paths().state_file, "bad-line\n")?;

        let result = store.load_state();

        assert!(matches!(
            result,
            Err(StoreError::Parse { path: Some(_), .. })
        ));
        Ok(())
    }

    #[test]
    fn corrupt_registry_id_is_rejected() -> Result<(), Box<dyn Error>> {
        let fixture = Fixture::new()?;
        let repo = fixture.git_repo("repo")?;
        let store = fixture.store()?;
        fs::write(
            &store.paths().registry_file,
            format!("repo-deadbeef\t{}\t1\t1\n", encode_path(repo.path())),
        )?;

        let result = store.list_repositories();

        assert!(matches!(result, Err(StoreError::Parse { .. })));
        Ok(())
    }

    #[test]
    fn duplicate_registry_id_is_rejected() -> Result<(), Box<dyn Error>> {
        let fixture = Fixture::new()?;
        let repo = fixture.git_repo("repo")?;
        let store = fixture.store()?;
        let id = RepoId::from_path(repo.path());
        let line = format!("{}\t{}\t1\t1\n", id.as_str(), encode_path(repo.path()));
        fs::write(&store.paths().registry_file, format!("{line}{line}"))?;

        let result = store.list_repositories();

        assert!(matches!(result, Err(StoreError::Parse { .. })));
        Ok(())
    }

    #[test]
    fn corrupt_state_prevents_add_without_registry_mutation() -> Result<(), Box<dyn Error>> {
        let fixture = Fixture::new()?;
        let repo = fixture.git_repo("repo")?;
        let store = fixture.store()?;
        fs::write(&store.paths().state_file, "bad-line\n")?;

        let result = store.add_repository(repo.path());

        assert!(matches!(result, Err(StoreError::Parse { .. })));
        assert!(store.list_repositories()?.is_empty());
        Ok(())
    }

    #[test]
    fn corrupt_state_prevents_remove_without_registry_mutation() -> Result<(), Box<dyn Error>> {
        let fixture = Fixture::new()?;
        let repo = fixture.git_repo("repo")?;
        let store = fixture.store()?;
        let record = store.add_repository(repo.path())?;
        fs::write(&store.paths().state_file, "bad-line\n")?;

        let result = store.remove_repository(&record.id);

        assert!(matches!(result, Err(StoreError::Parse { .. })));
        assert_eq!(store.list_repositories()?, vec![record]);
        Ok(())
    }

    #[test]
    fn invalid_repository_ids_are_rejected() {
        assert!(RepoId::parse("bad\tid").is_err());
        assert!(RepoId::parse("bad,id").is_err());
        assert!(RepoId::parse("").is_err());
    }

    #[test]
    fn store_paths_define_config_and_data_locations() -> Result<(), Box<dyn Error>> {
        let fixture = Fixture::new()?;
        let paths = fixture.paths();

        assert_eq!(paths.config_dir, fixture.path.join("config"));
        assert_eq!(paths.data_dir, fixture.path.join("data"));
        assert_eq!(
            paths.registry_file,
            fixture.path.join("data").join("registry.tsv")
        );
        assert_eq!(
            paths.state_file,
            fixture.path.join("data").join("state.tsv")
        );
        assert_eq!(
            paths.audit_file,
            fixture.path.join("data").join("audit.tsv")
        );
        Ok(())
    }

    struct Fixture {
        path: PathBuf,
    }

    impl Fixture {
        fn new() -> Result<Self, Box<dyn Error>> {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("bitbygit-store-test-{}-{id}", std::process::id()));
            if path.exists() {
                fs::remove_dir_all(&path)?;
            }
            fs::create_dir_all(&path)?;
            Ok(Self { path })
        }

        fn paths(&self) -> StorePaths {
            StorePaths::from_roots(self.path.join("config"), self.path.join("data"))
        }

        fn store(&self) -> Result<LocalStore, StoreError> {
            LocalStore::open(self.paths())
        }

        fn git_repo(&self, name: &str) -> Result<GitRepo, Box<dyn Error>> {
            let path = self.path.join(name);
            fs::create_dir_all(&path)?;
            run_git(&path, ["init", "-b", "main"])?;
            run_git(&path, ["config", "user.email", "bitbygit@example.invalid"])?;
            run_git(&path, ["config", "user.name", "bitbygit test"])?;
            fs::write(path.join("README.md"), "initial\n")?;
            run_git(&path, ["add", "README.md"])?;
            run_git(&path, ["commit", "-m", "initial"])?;
            Ok(GitRepo { path })
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _result = fs::remove_dir_all(&self.path);
        }
    }

    struct GitRepo {
        path: PathBuf,
    }

    impl GitRepo {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    fn run_git<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<(), Box<dyn Error>> {
        let output = Command::new("git").current_dir(cwd).args(args).output()?;
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
}
