//! What changed lately, read from the Git history. Every write is a commit,
//! the owner's as well as the LLM's, so the history tells them apart.

use anyhow::{bail, Result};
use serde::Serialize;

use crate::apply::AUTHOR_NAME;
use crate::git::git_stdout;
use crate::init::INDEX_FILE;
use crate::vault::Vault;

pub const MAX_HISTORY_LIMIT: usize = 100;
const MAX_SINCE_CHARS: usize = 32;
/// Commits that touch no visible page are skipped, so more are read than returned.
const MAX_COMMITS_READ: &str = "--max-count=1000";
const RECORD: char = '\u{1e}';
const FIELD: char = '\u{1f}';

#[derive(Debug, Serialize, PartialEq)]
pub struct HistoryEntry {
    pub commit: String,
    /// ISO 8601, with the author's time zone.
    pub date: String,
    /// "claude" for a change made through this tool, "owner" for the rest.
    pub by: &'static str,
    pub message: String,
    pub pages: Vec<PageChange>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct PageChange {
    pub path: String,
    /// "created", "changed", "moved" or "deleted".
    pub change: &'static str,
    /// Previous path of a moved page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// Same reach as the rest of the tool: Markdown pages outside hidden folders.
/// The generated index changes with every write and would only be noise.
fn is_visible(path: &str) -> bool {
    path != INDEX_FILE
        && path.to_ascii_lowercase().ends_with(".md")
        && !path.split('/').any(|part| part.starts_with('.'))
}

/// A date (`2026-11-09`), optionally followed by a time. Git would accept
/// almost anything and silently read it as something else.
fn check_since(since: &str) -> Result<()> {
    let bytes = since.as_bytes();
    let is_date = bytes.len() >= 10
        && bytes[..10].iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        });
    // Git misreads, without an error, the dates it has no room for.
    let number = |range: std::ops::Range<usize>| since[range].parse::<u32>().unwrap_or(0);
    let is_date = is_date
        && (1970..=2099).contains(&number(0..4))
        && (1..=12).contains(&number(5..7))
        && (1..=31).contains(&number(8..10));
    let rest_is_time = since
        .chars()
        .skip(10)
        .all(|c| c.is_ascii_digit() || "T:+-Z.".contains(c));
    if !is_date || !rest_is_time || since.chars().count() > MAX_SINCE_CHARS {
        bail!("invalid since: expected a date such as 2026-11-09 or 2026-11-09T14:30:00");
    }
    Ok(())
}

fn page_change(line: &str) -> Option<PageChange> {
    let mut fields = line.split('\t');
    let status = fields.next()?;
    let first = fields.next()?;
    let (path, change, from) = match status.chars().next()? {
        'A' => (first, "created", None),
        'D' => (first, "deleted", None),
        'R' => (fields.next()?, "moved", Some(first.to_string())),
        'C' => (fields.next()?, "created", None),
        _ => (first, "changed", None),
    };
    is_visible(path).then(|| PageChange {
        path: path.to_string(),
        change,
        from,
    })
}

/// The latest commits that touched pages, newest first.
pub fn recent_changes(
    vault: &Vault,
    since: Option<&str>,
    limit: usize,
) -> Result<Vec<HistoryEntry>> {
    let root = vault.root();
    // Without its own repository, Git would answer with a parent folder's history.
    if !root.join(".git").exists() {
        bail!("this vault has no Git repository, so no history");
    }
    let format = format!("--format={RECORD}%h{FIELD}%aI{FIELD}%an{FIELD}%s");
    let mut args = vec![
        // Otherwise accented names come out as escape sequences.
        "-c",
        "core.quotePath=false",
        "log",
        MAX_COMMITS_READ,
        "--find-renames",
        "--name-status",
        format.as_str(),
    ];
    let since_arg;
    if let Some(since) = since {
        check_since(since)?;
        since_arg = format!("--since={since}");
        args.push(&since_arg);
    }
    let log = git_stdout(root, &args)?;

    let mut entries = Vec::new();
    for record in log.split(RECORD).filter(|r| !r.trim().is_empty()) {
        let mut lines = record.lines();
        let mut header = lines.next().unwrap_or("").splitn(4, FIELD);
        let (Some(commit), Some(date), Some(author), Some(message)) =
            (header.next(), header.next(), header.next(), header.next())
        else {
            continue;
        };
        let pages: Vec<PageChange> = lines.filter_map(page_change).collect();
        if pages.is_empty() {
            continue;
        }
        entries.push(HistoryEntry {
            commit: commit.to_string(),
            date: date.to_string(),
            by: if author == AUTHOR_NAME {
                "claude"
            } else {
                "owner"
            },
            message: message.to_string(),
            pages,
        });
        if entries.len() >= limit.clamp(1, MAX_HISTORY_LIMIT) {
            break;
        }
    }
    Ok(entries)
}
