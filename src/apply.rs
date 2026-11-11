//! The only way to change a vault. A call validates everything, then writes
//! everything in one commit, or writes nothing. Deleting and adding files are
//! separate entry points, which the owner has to allow when starting the server.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

use crate::git::{commit_staged, git, git_network, git_ok, git_stdout, has_staged_changes};
use crate::init::{refresh_index, INDEX_FILE, RULES_FILE};
use crate::links::{links_to, rebase_own_links, rewrite_for_move};
use crate::vault::{canonical, is_page, revision, Vault, MAX_FILE_BYTES, MAX_PAGE_BYTES};

pub const MAX_CHANGES: usize = 200;
const MAX_REASON_CHARS: usize = 500;
const MAX_PATHS_IN_MESSAGE: usize = 20;
const MAX_LINKERS_IN_WARNING: usize = 10;
const NETWORK_LIMIT: Duration = Duration::from_secs(60);
/// How long a read trusts the last synchronization with the remote.
pub const READ_SYNC_EVERY: Duration = Duration::from_secs(60);
/// A read waits less for the network than a write does.
const READ_SYNC_LIMIT: Duration = Duration::from_secs(15);
const SYNC_MARKER: &str = "wiki-llm-mcp.synced";
pub const AUTHOR_NAME: &str = "Claude (wiki-llm-mcp)";
const AUTHOR: &str = "Claude (wiki-llm-mcp) <wiki-llm-mcp@localhost>";

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "self::schemars")]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    /// Create a new page. Fails if the page already exists.
    Create { path: String, content: String },
    /// Replace the whole content of an existing page. Prefer `edit` or `append`
    /// for a local change.
    Update {
        path: String,
        content: String,
        /// The `revision` that `read_page` returned for this page. The update
        /// is refused if the page changed since, so read the page first.
        revision: String,
    },
    /// Replace one passage of an existing page, leaving the rest untouched.
    Edit {
        path: String,
        /// Text to replace, copied exactly from the page. It must appear
        /// exactly once: include enough surrounding text to make it unique.
        old_text: String,
        /// Text that takes its place. Empty to remove the passage.
        new_text: String,
    },
    /// Add text at the end of an existing page.
    Append { path: String, content: String },
    /// Move or rename a page, updating the links that point to it.
    Move { from: String, to: String },
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ApplyReport {
    /// Short hash of the commit; absent when the changes altered nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Pages and files written, moved or deleted.
    pub pages_changed: Vec<String>,
    /// New revision of each page written, for a later `update` without
    /// reading the page again.
    pub revisions: BTreeMap<String, String>,
    pub links_updated: usize,
    pub pushed: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

fn is_protected(page: &str) -> bool {
    page.eq_ignore_ascii_case(RULES_FILE) || page.eq_ignore_ascii_case(INDEX_FILE)
}

fn is_raw(page: &str) -> bool {
    page.split('/')
        .next()
        .is_some_and(|first| first.eq_ignore_ascii_case("raw"))
}

fn ensure_not_protected(page: &str) -> Result<()> {
    if is_protected(page) {
        bail!("{page} is managed by the vault owner or generated: it cannot be changed");
    }
    Ok(())
}

fn ensure_not_raw(page: &str) -> Result<()> {
    if is_raw(page) {
        bail!("{page}: sources in raw/ can be added but not changed or moved");
    }
    Ok(())
}

/// The vault as it will be once the changes are applied. Nothing touches the
/// disk until every change has been validated against this.
struct Plan<'a> {
    vault: &'a Vault,
    pages: BTreeSet<String>,
    contents: HashMap<String, String>,
    /// Files that are not pages, to be added as they are.
    files: BTreeMap<String, Vec<u8>>,
    written: BTreeSet<String>,
    removed: BTreeSet<String>,
    links_updated: usize,
    warnings: Vec<String>,
}

impl<'a> Plan<'a> {
    fn new(vault: &'a Vault) -> Self {
        Self {
            vault,
            pages: vault.all_pages().into_iter().collect(),
            contents: HashMap::new(),
            files: BTreeMap::new(),
            written: BTreeSet::new(),
            removed: BTreeSet::new(),
            links_updated: 0,
            warnings: Vec::new(),
        }
    }

    fn read(&mut self, page: &str) -> Result<String> {
        if let Some(content) = self.contents.get(page) {
            return Ok(content.clone());
        }
        let content = self.vault.read_page(page)?;
        self.contents.insert(page.to_string(), content.clone());
        Ok(content)
    }

    fn set(&mut self, page: &str, content: String) {
        self.pages.insert(page.to_string());
        self.removed.remove(page);
        self.written.insert(page.to_string());
        self.contents.insert(page.to_string(), content);
    }

    /// Refuses a destination already in use, ignoring case because macOS and
    /// Windows would otherwise overwrite the existing page.
    fn ensure_free(&self, page: &str) -> Result<()> {
        let on_disk = fs::symlink_metadata(self.vault.root().join(page)).is_ok()
            && !self.removed.contains(page);
        let planned = self.pages.iter().chain(self.files.keys());
        if on_disk || planned.into_iter().any(|p| p.eq_ignore_ascii_case(page)) {
            bail!("{page} already exists");
        }
        Ok(())
    }

    /// Adds a file given as bytes. Like `create`, it never replaces anything.
    fn add_file(&mut self, path: &str, bytes: Vec<u8>) -> Result<()> {
        let path = Vault::normalize_any(path)?;
        ensure_not_protected(&path)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            bail!("{path}: file too large (limit: {MAX_FILE_BYTES} bytes)");
        }
        self.vault.check_writable(&path)?;
        self.ensure_free(&path)?;
        self.files.insert(path, bytes);
        Ok(())
    }

    /// Deletes a page or a file. Links that pointed to it are left as they
    /// are and reported: removing them is an editorial choice.
    fn delete(&mut self, path: &str) -> Result<()> {
        let target = Vault::normalize_any(path)?;
        ensure_not_protected(&target)?;
        self.vault.check_writable(&target)?;
        let exists = if is_page(&target) {
            self.pages.contains(&target)
        } else {
            !self.removed.contains(&target)
                && fs::symlink_metadata(self.vault.root().join(&target))
                    .is_ok_and(|meta| meta.is_file())
        };
        if !exists {
            bail!("not found: {target}");
        }

        let pages = self.pages.clone();
        let mut linkers = Vec::new();
        for page in pages.iter().filter(|page| **page != target) {
            if is_protected(page) {
                continue;
            }
            let Ok(content) = self.read(page) else {
                continue;
            };
            if links_to(&content, page, &target, &pages) {
                linkers.push(page.as_str());
            }
        }
        if !linkers.is_empty() {
            let more = linkers.len().saturating_sub(MAX_LINKERS_IN_WARNING);
            linkers.truncate(MAX_LINKERS_IN_WARNING);
            let mut list = linkers.join(", ");
            if more > 0 {
                list.push_str(&format!(" and {more} more"));
            }
            self.warnings.push(format!(
                "{target} was deleted but is still linked from: {list}"
            ));
        }

        self.pages.remove(&target);
        self.written.remove(&target);
        self.contents.remove(&target);
        self.removed.insert(target);
        Ok(())
    }

    /// Checks that a page can be changed in place, and returns its normalized path.
    fn existing_page(&self, path: &str) -> Result<String> {
        let page = Vault::normalize(path)?;
        ensure_not_protected(&page)?;
        ensure_not_raw(&page)?;
        self.vault.check_writable(&page)?;
        if !self.pages.contains(&page) {
            bail!("page not found: {page}");
        }
        Ok(page)
    }

    fn apply(&mut self, change: &Change) -> Result<()> {
        match change {
            Change::Create { path, content } => {
                let page = Vault::normalize(path)?;
                ensure_not_protected(&page)?;
                ensure_size(&page, content)?;
                self.vault.check_writable(&page)?;
                self.ensure_free(&page)?;
                self.set(&page, content.clone());
            }
            Change::Update {
                path,
                content,
                revision: read_revision,
            } => {
                let page = self.existing_page(path)?;
                ensure_size(&page, content)?;
                // A page already changed by this call has no revision the
                // caller could know: the check is for what was on disk.
                if !self.written.contains(&page) && revision(&self.read(&page)?) != *read_revision {
                    bail!(
                        "{page} changed since you read it, or the revision is not the one \
                         read_page returned: read the page again, then retry"
                    );
                }
                self.set(&page, content.clone());
            }
            Change::Edit {
                path,
                old_text,
                new_text,
            } => {
                let page = self.existing_page(path)?;
                if old_text.is_empty() {
                    bail!("{page}: old_text is empty");
                }
                let current = self.read(&page)?;
                match current.matches(old_text.as_str()).count() {
                    0 => bail!(
                        "{page}: old_text was not found. It must match the page exactly: \
                         read the page again"
                    ),
                    1 => {}
                    count => bail!(
                        "{page}: old_text appears {count} times. Include more surrounding \
                         text so that it is unique"
                    ),
                }
                let updated = current.replacen(old_text.as_str(), new_text, 1);
                ensure_size(&page, &updated)?;
                self.set(&page, updated);
            }
            Change::Append { path, content } => {
                let page = self.existing_page(path)?;
                if content.is_empty() {
                    bail!("{page}: nothing to append");
                }
                let mut updated = self.read(&page)?;
                if !updated.is_empty() && !updated.ends_with('\n') {
                    updated.push('\n');
                }
                updated.push_str(content);
                ensure_size(&page, &updated)?;
                self.set(&page, updated);
            }
            Change::Move { from, to } => self.apply_move(from, to)?,
        }
        Ok(())
    }

    fn apply_move(&mut self, from: &str, to: &str) -> Result<()> {
        let old = Vault::normalize(from)?;
        let new = Vault::normalize(to)?;
        for page in [&old, &new] {
            ensure_not_protected(page)?;
            ensure_not_raw(page)?;
            self.vault.check_writable(page)?;
        }
        if !self.pages.contains(&old) {
            bail!("page not found: {old}");
        }
        self.ensure_free(&new)?;
        // Checked now so that the move fails if the page cannot be carried over.
        self.read(&old)?;

        let before = self.pages.clone();
        for page in &before {
            // Unreadable pages cannot hold rewritable links.
            if is_protected(page) {
                continue;
            }
            let Ok(content) = self.read(page) else {
                continue;
            };
            let rewritten = rewrite_for_move(&content, page, &old, &new, &before);
            if rewritten == content {
                continue;
            }
            // Sources stay as they were: the broken link is reported instead.
            if is_raw(page) {
                self.warnings.push(format!(
                    "{page} links to {old}, which is now {new}: sources in raw/ are not \
                     rewritten, so this link still points to the old location"
                ));
                continue;
            }
            self.links_updated += 1;
            self.set(page, rewritten);
        }

        let root = self.vault.root().to_path_buf();
        let exists = |p: &str| p == new || before.contains(p) || root.join(p).exists();
        let content = rebase_own_links(&self.read(&old)?, &old, &new, &exists);
        self.pages.remove(&old);
        self.written.remove(&old);
        self.removed.insert(old.clone());
        self.contents.remove(&old);
        self.set(&new, content);
        Ok(())
    }
}

fn ensure_size(page: &str, content: &str) -> Result<()> {
    if content.len() as u64 > MAX_PAGE_BYTES {
        bail!("{page}: content too large (limit: {MAX_PAGE_BYTES} bytes)");
    }
    Ok(())
}

fn check_reason(reason: &str) -> Result<String> {
    let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if reason.is_empty() {
        bail!("a reason is required: it becomes the commit message");
    }
    Ok(reason.chars().take(MAX_REASON_CHARS).collect())
}

fn check_repository(root: &Path) -> Result<PathBuf> {
    let top = git(root, &["rev-parse", "--show-toplevel"])?;
    let is_own_repo = top.status.success()
        && canonical(Path::new(String::from_utf8_lossy(&top.stdout).trim()))
            .is_ok_and(|top| top == root);
    if !is_own_repo {
        bail!("this vault has no Git repository: run `wiki-llm-mcp init` before writing");
    }
    if !git(root, &["symbolic-ref", "-q", "HEAD"])?.status.success() {
        bail!("the vault's Git repository is not on a branch");
    }
    let git_dir = PathBuf::from(git_stdout(root, &["rev-parse", "--absolute-git-dir"])?);
    let git_dir = canonical(&git_dir).unwrap_or(git_dir);
    for marker in [
        "MERGE_HEAD",
        "rebase-merge",
        "rebase-apply",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
    ] {
        if git_dir.join(marker).exists() {
            bail!("a Git operation is in progress in the vault: finish it before writing");
        }
    }
    Ok(git_dir)
}

/// The owner's uncommitted work (Obsidian edits) gets its own commit first, so
/// that it can never be lost or mistaken for a change made by the LLM.
fn snapshot_owner_work(root: &Path) -> Result<()> {
    git_ok(root, &["add", "-A"])?;
    commit_staged(root, "wiki-llm-mcp: snapshot of uncommitted changes")?;
    Ok(())
}

fn has_upstream(root: &Path) -> Result<bool> {
    Ok(git(root, &["rev-parse", "--abbrev-ref", "@{upstream}"])?
        .status
        .success())
}

/// Being offline only delays synchronization; a real conflict stops the write.
fn pull_remote(root: &Path, warnings: &mut Vec<String>) -> Result<()> {
    match git_network(root, &["fetch", "--quiet"], NETWORK_LIMIT) {
        Ok((status, _)) if status.success() => {}
        Ok((_, stderr)) => {
            warnings.push(format!("could not reach the remote: {stderr}"));
            return Ok(());
        }
        Err(error) => {
            warnings.push(format!("could not reach the remote: {error}"));
            return Ok(());
        }
    }
    if !git(root, &["rebase", "--quiet", "@{upstream}"])?
        .status
        .success()
    {
        let _ = git(root, &["rebase", "--abort"]);
        bail!(
            "the vault conflicts with changes on its Git remote. Nothing was written. \
             Resolve the conflict with Git, then try again"
        );
    }
    Ok(())
}

fn push_remote(root: &Path, warnings: &mut Vec<String>) -> bool {
    match git_network(root, &["push", "--quiet"], NETWORK_LIMIT) {
        Ok((status, _)) if status.success() => true,
        Ok((_, stderr)) => {
            warnings.push(format!("committed locally but not pushed: {stderr}"));
            false
        }
        Err(error) => {
            warnings.push(format!("committed locally but not pushed: {error}"));
            false
        }
    }
}

fn mark_synced(git_dir: &Path) {
    let _ = fs::write(git_dir.join(SYNC_MARKER), "");
}

/// Before a read, takes in what other devices pushed, at most once per
/// `max_age`: without this, only a write would bring the vault up to date.
/// Only a fast-forward is done, so nothing is rewritten and nothing can
/// conflict; uncommitted or unpushed work is left for the next write to
/// reconcile. Returns whether the pages changed.
pub fn sync_for_read(vault: &Vault, max_age: Duration) -> Result<bool> {
    let root = vault.root();
    let marker = root.join(".git").join(SYNC_MARKER);
    let is_fresh = |marker: &Path| {
        fs::metadata(marker)
            .and_then(|meta| meta.modified())
            .is_ok_and(|at| at.elapsed().is_ok_and(|age| age < max_age))
    };
    // The usual case costs one file lookup, and a vault without Git nothing more.
    if !root.join(".git").is_dir() || is_fresh(&marker) {
        return Ok(false);
    }
    let git_dir = check_repository(root)?;
    let lock = File::create(git_dir.join("wiki-llm-mcp.lock")).context("could not create lock")?;
    lock.lock().context("could not take the write lock")?;
    if is_fresh(&marker) {
        return Ok(false);
    }
    // Marked first: an unreachable remote is not tried again by every read.
    mark_synced(&git_dir);
    if !has_upstream(root)? {
        return Ok(false);
    }
    let fetched = git_network(root, &["fetch", "--quiet"], READ_SYNC_LIMIT);
    if !fetched.is_ok_and(|(status, _)| status.success()) {
        return Ok(false);
    }
    let before = git_stdout(root, &["rev-parse", "HEAD"])?;
    // Git itself refuses when the histories diverged or when a file changed
    // on both sides, and leaves everything as it was.
    let _ = git(root, &["merge", "--ff-only", "--quiet", "@{upstream}"])?;
    Ok(git_stdout(root, &["rev-parse", "HEAD"])? != before)
}

/// Restores files to their previous state after a failed write.
fn undo(previous: &[(PathBuf, Option<Vec<u8>>)]) {
    for (path, content) in previous.iter().rev() {
        let _ = match content {
            Some(content) => fs::write(path, content),
            None => fs::remove_file(path),
        };
    }
}

fn remember(previous: &mut Vec<(PathBuf, Option<Vec<u8>>)>, path: &Path) {
    previous.push((path.to_path_buf(), fs::read(path).ok()));
}

fn write_and_commit(
    vault: &Vault,
    plan: &Plan,
    reason: &str,
    author: Option<&str>,
) -> Result<Option<String>> {
    let root = vault.root();
    let mut previous = Vec::new();

    let staged: Result<Vec<String>> = (|| {
        let pages = plan
            .written
            .iter()
            .map(|page| (page, plan.contents[page].as_bytes()));
        let files = plan.files.iter().map(|(file, bytes)| (file, &bytes[..]));
        for (name, bytes) in pages.chain(files) {
            let path = root.join(name);
            remember(&mut previous, &path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).context("could not create folder")?;
            }
            fs::write(&path, bytes).with_context(|| format!("could not write {name}"))?;
        }
        for page in &plan.removed {
            let path = root.join(page);
            remember(&mut previous, &path);
            fs::remove_file(&path).with_context(|| format!("could not remove {page}"))?;
        }
        let mut paths: Vec<String> = plan
            .written
            .iter()
            .chain(plan.files.keys())
            .chain(&plan.removed)
            .cloned()
            .collect();
        let index_path = root.join(INDEX_FILE);
        remember(&mut previous, &index_path);
        if refresh_index(vault)? {
            paths.push(INDEX_FILE.to_string());
        }
        Ok(paths)
    })();

    let result = staged.and_then(|paths| {
        let mut add = vec!["--literal-pathspecs", "add", "-A", "--"];
        add.extend(paths.iter().map(String::as_str));
        let committed = git_ok(root, &add).and_then(|()| {
            if !has_staged_changes(root)? {
                return Ok(None);
            }
            let mut body: Vec<&str> = paths
                .iter()
                .map(String::as_str)
                .take(MAX_PATHS_IN_MESSAGE)
                .collect();
            let more = paths.len().saturating_sub(MAX_PATHS_IN_MESSAGE);
            let more = format!("... and {more} more");
            if paths.len() > MAX_PATHS_IN_MESSAGE {
                body.push(&more);
            }
            let body = body.join("\n");
            let author = author.map(|author| format!("--author={author}"));
            let mut commit = vec!["commit", "-q"];
            commit.extend(author.as_deref());
            commit.extend(["-m", reason, "-m", &body]);
            git_ok(root, &commit)?;
            Ok(Some(git_stdout(root, &["rev-parse", "--short", "HEAD"])?))
        });
        if committed.is_err() {
            let mut reset = vec!["--literal-pathspecs", "reset", "-q", "--"];
            reset.extend(paths.iter().map(String::as_str));
            let _ = git(root, &reset);
        }
        committed
    });

    if result.is_err() {
        undo(&previous);
    }
    result
}

pub fn apply_changes(vault: &Vault, changes: &[Change], reason: &str) -> Result<ApplyReport> {
    let reason = check_reason(reason)?;
    if changes.is_empty() {
        bail!("no changes given");
    }
    if changes.len() > MAX_CHANGES {
        bail!("too many changes in one call (limit: {MAX_CHANGES})");
    }
    transact(vault, &reason, Some(AUTHOR), |plan| {
        changes.iter().try_for_each(|change| plan.apply(change))
    })
}

/// Deletes pages and files, all of them or none, in one commit. Git keeps what
/// they contained. The server only offers this when the owner allowed it.
pub fn delete_paths(vault: &Vault, paths: &[String], reason: &str) -> Result<ApplyReport> {
    let reason = check_reason(reason)?;
    if paths.is_empty() {
        bail!("no paths given");
    }
    if paths.len() > MAX_CHANGES {
        bail!("too many paths in one call (limit: {MAX_CHANGES})");
    }
    transact(vault, &reason, Some(AUTHOR), |plan| {
        paths.iter().try_for_each(|path| plan.delete(path))
    })
}

/// Adds one file sent by the owner through an upload link. The commit is not
/// signed by the LLM: the file comes from the owner.
pub fn add_file(vault: &Vault, path: &str, bytes: Vec<u8>) -> Result<ApplyReport> {
    let reason = check_reason(&format!("Upload {path}"))?;
    transact(vault, &reason, None, |plan| plan.add_file(path, bytes))
}

/// The steps every write goes through: lock, save the owner's work, take in
/// the remote, validate the whole plan, then write, commit and push.
fn transact(
    vault: &Vault,
    reason: &str,
    author: Option<&str>,
    build: impl FnOnce(&mut Plan) -> Result<()>,
) -> Result<ApplyReport> {
    let root = vault.root();
    let git_dir = check_repository(root)?;

    // One writer at a time, including across several server processes.
    let lock = File::create(git_dir.join("wiki-llm-mcp.lock")).context("could not create lock")?;
    lock.lock().context("could not take the write lock")?;

    let mut warnings = Vec::new();
    snapshot_owner_work(root)?;
    let upstream = has_upstream(root)?;
    if upstream {
        pull_remote(root, &mut warnings)?;
        mark_synced(&git_dir);
    }

    // Validated against the synchronized vault, before anything is written.
    let mut plan = Plan::new(vault);
    build(&mut plan)?;

    let commit = write_and_commit(vault, &plan, reason, author)?;
    // Also sends commits that an earlier offline call could not push.
    let pushed = upstream && push_remote(root, &mut warnings);
    warnings.extend(plan.warnings.iter().cloned());

    let revisions = plan
        .written
        .iter()
        .map(|page| (page.clone(), revision(&plan.contents[page])))
        .collect();
    let mut pages_changed: Vec<String> = plan.written.iter().cloned().collect();
    pages_changed.extend(plan.files.keys().cloned());
    pages_changed.extend(plan.removed.iter().cloned());
    Ok(ApplyReport {
        commit,
        pages_changed,
        revisions,
        links_updated: plan.links_updated,
        pushed,
        warnings,
    })
}
