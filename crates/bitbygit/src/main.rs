use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

use bitbygit_store::{AuditEntry, LocalStore, RepoId, StorePaths};

const RECENT_AUDIT_LIMIT: usize = 20;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("{} {}", bitbygit_core::APP_NAME, bitbygit_core::VERSION);
        return Ok(());
    }
    if is_debug_audit_command(&args) {
        print_recent_audit_entries()?;
        return Ok(());
    }

    bitbygit_tui::run()?;
    Ok(())
}

fn is_debug_audit_command(args: &[String]) -> bool {
    matches!(args, [flag] if flag == "--debug-audit")
        || matches!(args, [debug, audit] if debug == "debug" && audit == "audit")
}

fn print_recent_audit_entries() -> Result<(), Box<dyn Error>> {
    let store = LocalStore::open(StorePaths::from_environment()?)?;
    let entries = store.list_audit_entries()?;
    if entries.is_empty() {
        println!("No audit entries found.");
        return Ok(());
    }

    let repo_paths = repo_path_map(&store);
    let start = entries.len().saturating_sub(RECENT_AUDIT_LIMIT);
    for entry in entries[start..].iter().rev() {
        println!("{}", format_audit_entry(entry, &repo_paths));
    }
    Ok(())
}

fn repo_path_map(store: &LocalStore) -> BTreeMap<RepoId, PathBuf> {
    match store.list_repositories() {
        Ok(records) => records
            .into_iter()
            .map(|record| (record.id, record.path))
            .collect(),
        Err(_error) => BTreeMap::new(),
    }
}

fn format_audit_entry(entry: &AuditEntry, repo_paths: &BTreeMap<RepoId, PathBuf>) -> String {
    let repo = match &entry.repo_id {
        Some(repo_id) => match repo_paths.get(repo_id) {
            Some(path) => format!("{} ({})", repo_id, path.display()),
            None => repo_id.to_string(),
        },
        None => "-".to_owned(),
    };
    format!(
        "timestamp={} repo={} operation={} result={} message={}",
        entry.timestamp,
        repo,
        one_line(&entry.operation),
        one_line(&entry.result),
        one_line(&entry.message)
    )
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
