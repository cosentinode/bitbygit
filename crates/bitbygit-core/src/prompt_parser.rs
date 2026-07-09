use std::error::Error;
use std::fmt;

use crate::OperationRequest;

const PROMPT_EXAMPLES: &str = "branches, checkout <branch>, branch <name>, branch <name> from <base>, merge <branch>, rebase <base>, open pr, commit -m \"message\", fetch, push, pull, or pull --rebase";
pub const UNSUPPORTED_PROMPT_MESSAGE: &str = "Unsupported prompt. Try: branches, checkout <branch>, branch <name>, branch <name> from <base>, merge <branch>, rebase <base>, open pr, commit -m \"message\", fetch, push, pull, or pull --rebase";
const SHELL_SYNTAX_MESSAGE: &str =
    "Shell-style prompt syntax is not supported. Use one guarded prompt at a time.";
const RAW_GIT_MESSAGE: &str =
    "Raw Git commands are not supported. Try a guarded prompt such as `push` or `pull`.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedPrompt {
    Single(OperationRequest),
    Sequence(Vec<OperationRequest>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptParseError {
    message: String,
}

impl PromptParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for PromptParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for PromptParseError {}

pub fn parse_prompt(input: &str) -> Result<ParsedPrompt, PromptParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(parse_error(format!(
            "Prompt required. Try: {PROMPT_EXAMPLES}"
        )));
    }

    let segments = split_prompt_sequence(trimmed)?;
    let mut requests = Vec::with_capacity(segments.len());
    for segment in segments {
        requests.push(parse_single_prompt(segment)?);
    }

    if requests.len() == 1 {
        Ok(ParsedPrompt::Single(requests.swap_remove(0)))
    } else {
        Ok(ParsedPrompt::Sequence(requests))
    }
}

fn split_prompt_sequence(input: &str) -> Result<Vec<&str>, PromptParseError> {
    let mut segments = Vec::new();
    let mut segment_start = 0;
    let mut in_quote = false;
    let mut index = 0;

    while index < input.len() {
        let Some(character) = input[index..].chars().next() else {
            break;
        };
        let character_len = character.len_utf8();

        if character == '"' {
            in_quote = !in_quote;
            index += character_len;
            continue;
        }

        if !in_quote {
            if let Some(connector_len) = connector_len_at(input, index) {
                let segment = input[segment_start..index].trim();
                if segment.is_empty() {
                    return Err(parse_error("Expected a prompt before the connector."));
                }
                segments.push(segment);
                index += connector_len;
                segment_start = index;
                continue;
            }
            if is_shell_metacharacter(character) {
                return Err(parse_error(SHELL_SYNTAX_MESSAGE));
            }
        }

        index += character_len;
    }

    if in_quote {
        return Err(parse_error(
            "Unclosed quote. Close the quoted string before submitting.",
        ));
    }

    let segment = input[segment_start..].trim();
    if segment.is_empty() {
        return Err(parse_error("Expected a prompt after the connector."));
    }
    segments.push(segment);
    Ok(segments)
}

fn connector_len_at(input: &str, index: usize) -> Option<usize> {
    if input[index..].starts_with("&&") {
        return Some(2);
    }
    ["and", "then"].into_iter().find_map(|connector| {
        if ascii_word_at(input, index, connector) {
            Some(connector.len())
        } else {
            None
        }
    })
}

fn ascii_word_at(input: &str, index: usize, word: &str) -> bool {
    let end = index + word.len();
    let Some(candidate) = input.get(index..end) else {
        return false;
    };
    candidate.eq_ignore_ascii_case(word)
        && has_whitespace_boundary_before(input, index)
        && has_whitespace_boundary_after(input, end)
}

fn has_whitespace_boundary_before(input: &str, index: usize) -> bool {
    index == 0
        || input[..index]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
}

fn has_whitespace_boundary_after(input: &str, index: usize) -> bool {
    index == input.len()
        || input[index..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
}

fn is_shell_metacharacter(character: char) -> bool {
    matches!(character, ';' | '|' | '&' | '<' | '>' | '`' | '$' | '\\')
}

fn parse_single_prompt(input: &str) -> Result<OperationRequest, PromptParseError> {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();

    if lower == "git" || lower.starts_with("git ") {
        return Err(parse_error(RAW_GIT_MESSAGE));
    }

    match lower.as_str() {
        "branches" => Ok(OperationRequest::Branches),
        "fetch" => Ok(OperationRequest::Fetch),
        "push" => Ok(OperationRequest::Push),
        "open pr" | "open pull request" => Ok(OperationRequest::OpenPullRequest { base: None }),
        "pull" => Ok(OperationRequest::Pull { rebase: false }),
        "pull --rebase" | "pull rebase" => Ok(OperationRequest::Pull { rebase: true }),
        _ if lower.starts_with("checkout ") => parse_one_arg_prompt(trimmed, "checkout")
            .map(|branch| OperationRequest::Checkout { branch }),
        _ if lower.starts_with("merge ") => {
            parse_one_arg_prompt(trimmed, "merge").map(|branch| OperationRequest::Merge { branch })
        }
        _ if lower.starts_with("rebase ") => {
            parse_one_arg_prompt(trimmed, "rebase").map(|base| OperationRequest::Rebase { base })
        }
        _ if lower.starts_with("open pr to ") => parse_open_pull_request_prompt(trimmed, "open pr"),
        _ if lower.starts_with("open pull request to ") => {
            parse_open_pull_request_prompt(trimmed, "open pull request")
        }
        _ if lower.starts_with("branch ") => parse_branch_prompt(trimmed),
        _ if lower == "commit" || lower.starts_with("commit ") => {
            parse_commit_prompt(trimmed).map(|message| OperationRequest::Commit { message })
        }
        _ => Err(parse_error(UNSUPPORTED_PROMPT_MESSAGE)),
    }
}

fn parse_open_pull_request_prompt(
    input: &str,
    command: &str,
) -> Result<OperationRequest, PromptParseError> {
    let Some(rest) = input.get(command.len()..) else {
        return Err(parse_error("Expected: open pr or open pr to <base>"));
    };
    let rest = rest.trim_start();
    let Some(base) = rest
        .get(..2)
        .filter(|prefix| prefix.eq_ignore_ascii_case("to"))
        .and_then(|_prefix| rest.get(2..))
        .filter(|base| base.starts_with(char::is_whitespace))
    else {
        return Err(parse_error("Expected: open pr or open pr to <base>"));
    };
    Ok(OperationRequest::OpenPullRequest {
        base: Some(parse_branch_arg(
            base.trim(),
            "Expected: open pr or open pr to <base>",
        )?),
    })
}

fn parse_one_arg_prompt(input: &str, command: &str) -> Result<String, PromptParseError> {
    let Some(rest) = input.get(command.len()..) else {
        return Err(parse_error(format!("Expected: {command} <branch>")));
    };
    parse_branch_arg(rest.trim(), &format!("Expected: {command} <branch>"))
}

fn parse_branch_prompt(input: &str) -> Result<OperationRequest, PromptParseError> {
    let Some(rest) = input.get("branch".len()..) else {
        return Err(parse_error(
            "Expected: branch <name> or branch <name> from <base>",
        ));
    };
    let parts = rest.split_whitespace().collect::<Vec<_>>();
    match parts.as_slice() {
        [branch] if branch.eq_ignore_ascii_case("list") => {
            Err(parse_error("Use `branches` to list branches."))
        }
        [branch] => Ok(OperationRequest::CreateBranch {
            branch: parse_branch_arg(branch, "Expected: branch <name>")?,
            base: None,
        }),
        [branch, keyword, base] if keyword.eq_ignore_ascii_case("from") => {
            Ok(OperationRequest::CreateBranch {
                branch: parse_branch_arg(branch, "Expected: branch <name> from <base>")?,
                base: Some(parse_branch_arg(
                    base,
                    "Expected: branch <name> from <base>",
                )?),
            })
        }
        _ => Err(parse_error(
            "Expected: branch <name> or branch <name> from <base>",
        )),
    }
}

fn parse_branch_arg(value: &str, usage: &str) -> Result<String, PromptParseError> {
    if value.is_empty()
        || value.starts_with('-')
        || value.contains(char::is_whitespace)
        || value.contains('\0')
    {
        return Err(parse_error(usage));
    }
    Ok(value.to_owned())
}

fn parse_commit_prompt(input: &str) -> Result<String, PromptParseError> {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower != "commit" && !lower.starts_with("commit ") {
        return Err(parse_error(
            "Unsupported prompt. Try: commit -m \"message\"",
        ));
    }
    let Some(rest) = trimmed.get("commit".len()..) else {
        return Err(parse_error(
            "Unsupported prompt. Try: commit -m \"message\"",
        ));
    };
    let mut message = rest.trim_start();
    if message.is_empty() {
        return Err(parse_error(
            "Commit message required. Try: commit -m \"message\"",
        ));
    }
    if message == "-m" {
        message = "";
    } else if let Some(after_flag) = message.strip_prefix("-m ") {
        message = after_flag.trim_start();
    } else if message.starts_with('-') {
        return Err(parse_error(
            "Unsupported prompt. Try: commit -m \"message\"",
        ));
    }
    parse_commit_message(message)
}

fn parse_commit_message(input: &str) -> Result<String, PromptParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(parse_error(
            "Commit message required. Try: commit -m \"message\"",
        ));
    }
    if let Some(rest) = trimmed.strip_prefix('"') {
        let Some(end) = rest.find('"') else {
            return Err(parse_error(
                "Unclosed quote. Close the quoted string before submitting.",
            ));
        };
        if !rest[end + 1..].trim().is_empty() {
            return Err(parse_error("Unexpected text after commit message."));
        }
        let message = &rest[..end];
        if message.trim().is_empty() {
            return Err(parse_error("Commit message cannot be empty."));
        }
        return Ok(message.to_owned());
    }
    Ok(trimmed.to_owned())
}

fn parse_error(message: impl Into<String>) -> PromptParseError {
    PromptParseError::new(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_single_step_golden_prompts() {
        let cases = [
            ("branches", OperationRequest::Branches),
            ("fetch", OperationRequest::Fetch),
            ("push", OperationRequest::Push),
            ("open pr", OperationRequest::OpenPullRequest { base: None }),
            (
                "open pull request",
                OperationRequest::OpenPullRequest { base: None },
            ),
            (
                "open pull request to develop",
                OperationRequest::OpenPullRequest {
                    base: Some("develop".to_owned()),
                },
            ),
            (
                "OPEN PR TO release",
                OperationRequest::OpenPullRequest {
                    base: Some("release".to_owned()),
                },
            ),
            ("pull", OperationRequest::Pull { rebase: false }),
            ("pull --rebase", OperationRequest::Pull { rebase: true }),
            ("pull rebase", OperationRequest::Pull { rebase: true }),
            (
                "checkout feature/auth",
                OperationRequest::Checkout {
                    branch: "feature/auth".to_owned(),
                },
            ),
            (
                "branch feature/auth",
                OperationRequest::CreateBranch {
                    branch: "feature/auth".to_owned(),
                    base: None,
                },
            ),
            (
                "branch feature/auth from origin/main",
                OperationRequest::CreateBranch {
                    branch: "feature/auth".to_owned(),
                    base: Some("origin/main".to_owned()),
                },
            ),
            (
                "merge feature/auth",
                OperationRequest::Merge {
                    branch: "feature/auth".to_owned(),
                },
            ),
            (
                "rebase origin/main",
                OperationRequest::Rebase {
                    base: "origin/main".to_owned(),
                },
            ),
            (
                "commit -m \"sync docs\"",
                OperationRequest::Commit {
                    message: "sync docs".to_owned(),
                },
            ),
            (
                "commit ship staged work",
                OperationRequest::Commit {
                    message: "ship staged work".to_owned(),
                },
            ),
        ];

        for (input, request) in cases {
            assert_eq!(parse_prompt(input), Ok(ParsedPrompt::Single(request)));
        }
    }

    #[test]
    fn parses_request_sequences_without_shell_text() {
        assert_eq!(
            parse_prompt("commit -m \"fix auth and routing\" and push"),
            Ok(ParsedPrompt::Sequence(vec![
                OperationRequest::Commit {
                    message: "fix auth and routing".to_owned(),
                },
                OperationRequest::Push,
            ]))
        );
        assert_eq!(
            parse_prompt("fetch then pull --rebase"),
            Ok(ParsedPrompt::Sequence(vec![
                OperationRequest::Fetch,
                OperationRequest::Pull { rebase: true },
            ]))
        );
        assert_eq!(
            parse_prompt("fetch && push"),
            Ok(ParsedPrompt::Sequence(vec![
                OperationRequest::Fetch,
                OperationRequest::Push,
            ]))
        );
    }

    #[test]
    fn preserves_connectors_inside_quotes() {
        assert_eq!(
            parse_prompt("commit -m \"fix auth and routing then cleanup\""),
            Ok(ParsedPrompt::Single(OperationRequest::Commit {
                message: "fix auth and routing then cleanup".to_owned(),
            }))
        );
        assert_eq!(
            parse_prompt("commit -m \"fix auth && routing\""),
            Ok(ParsedPrompt::Single(OperationRequest::Commit {
                message: "fix auth && routing".to_owned(),
            }))
        );
    }

    #[test]
    fn rejects_unsupported_golden_prompts() {
        let cases = [
            ("", format!("Prompt required. Try: {PROMPT_EXAMPLES}")),
            ("branch list", "Use `branches` to list branches.".to_owned()),
            (
                "checkout --detach",
                "Expected: checkout <branch>".to_owned(),
            ),
            (
                "branch feature from",
                "Expected: branch <name> or branch <name> from <base>".to_owned(),
            ),
            ("push --force", UNSUPPORTED_PROMPT_MESSAGE.to_owned()),
            ("pull --ff-only", UNSUPPORTED_PROMPT_MESSAGE.to_owned()),
            ("git push", RAW_GIT_MESSAGE.to_owned()),
            ("git commit -m \"message\"", RAW_GIT_MESSAGE.to_owned()),
            (
                "commit",
                "Commit message required. Try: commit -m \"message\"".to_owned(),
            ),
            (
                "commit -m",
                "Commit message required. Try: commit -m \"message\"".to_owned(),
            ),
            (
                "commit --amend",
                "Unsupported prompt. Try: commit -m \"message\"".to_owned(),
            ),
            (
                "commit --no-verify",
                "Unsupported prompt. Try: commit -m \"message\"".to_owned(),
            ),
            (
                "commit -S -m \"signed\"",
                "Unsupported prompt. Try: commit -m \"message\"".to_owned(),
            ),
            (
                "commit -m \"message\" trailing",
                "Unexpected text after commit message.".to_owned(),
            ),
            (
                "commitment -m \"message\"",
                UNSUPPORTED_PROMPT_MESSAGE.to_owned(),
            ),
        ];

        for (input, message) in cases {
            assert_eq!(
                parse_prompt(input).map_err(|error| error.message().to_owned()),
                Err(message)
            );
        }
    }

    #[test]
    fn unclosed_quotes_fail_closed() {
        assert_eq!(
            parse_prompt("commit -m \"fix and push").map_err(|error| error.message().to_owned()),
            Err("Unclosed quote. Close the quoted string before submitting.".to_owned())
        );
    }

    #[test]
    fn shell_like_prompts_reject_the_whole_prompt() {
        for input in [
            "fetch & push",
            "fetch; push",
            "push | cat",
            "pull > out.txt",
            "fetch and git push",
        ] {
            assert!(parse_prompt(input).is_err());
        }
    }
}
