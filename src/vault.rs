//! Read access to a vault. Every path coming from a client goes through
//! [`Vault::resolve`], which is the only way to reach a file.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

pub const MAX_PAGE_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_PATH_CHARS: usize = 1024;
pub const MAX_QUERY_CHARS: usize = 256;
pub const MAX_LIST_LIMIT: usize = 200;
pub const MAX_SEARCH_LIMIT: usize = 50;
const SNIPPET_CHARS: usize = 200;

#[derive(Debug, Clone)]
pub struct Vault {
    root: PathBuf,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct PageList {
    pub pages: Vec<String>,
    pub total: usize,
    /// Pass as `offset` to get the next page of results; absent on the last one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct SearchHit {
    pub path: String,
    pub matches: usize,
    pub snippet: String,
}

/// Short fingerprint of a page's content. A writer sends back the one it read,
/// so that a page changed in the meantime is not overwritten.
pub fn revision(content: &str) -> String {
    Sha256::digest(content.as_bytes())[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// The real location of a path. On Windows the standard library answers
/// `\\?\C:\notes`, a form that Git refuses and that never compares equal to the
/// plain `C:\notes`: the prefix is dropped.
pub fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    let real = path.canonicalize()?;
    #[cfg(windows)]
    if let Some(plain) = real.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        if !plain.starts_with(r"UNC\") {
            return Ok(PathBuf::from(plain));
        }
    }
    Ok(real)
}

/// Names that Windows resolves to another file than the one written: an
/// alternate data stream (`a.md:x`), a short alias (`GIT~1` is `.git`), or a
/// name whose trailing dots and spaces are dropped.
pub fn is_windows_alias(name: &str) -> bool {
    name.contains(':') || name.contains('~') || name.ends_with('.') || name.ends_with(' ')
}

impl Vault {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let root =
            canonical(path).with_context(|| format!("vault not found: {}", path.display()))?;
        if !root.is_dir() {
            bail!("vault is not a directory: {}", path.display());
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Checks the form of a client-supplied path, without touching the disk,
    /// and returns it in canonical `a/b.md` form.
    pub fn normalize(rel: &str) -> Result<String> {
        if rel.is_empty() || rel.chars().count() > MAX_PATH_CHARS {
            bail!("invalid path");
        }
        if rel.contains('\0') || rel.contains('\\') {
            bail!("invalid path: forbidden character");
        }
        let path = Path::new(rel);
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(part)
                    if cfg!(windows) && is_windows_alias(&part.to_string_lossy()) =>
                {
                    bail!("invalid path: this name is not safe on Windows")
                }
                Component::Normal(part) if !is_hidden(&part.to_string_lossy()) => {
                    parts.push(part.to_string_lossy().into_owned());
                }
                Component::Normal(_) => bail!("invalid path: hidden files are not accessible"),
                _ => bail!("invalid path: must be relative, inside the vault"),
            }
        }
        if !has_md_extension(path) {
            bail!("invalid path: only .md pages are accessible");
        }
        Ok(parts.join("/"))
    }

    /// Refuses a write location that goes through a symlink or lands on one.
    pub fn check_writable(&self, rel: &str) -> Result<()> {
        let mut current = self.root.clone();
        for part in rel.split('/') {
            current.push(part);
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    bail!("invalid path: symlinks cannot be written through: {rel}")
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Turns a client-supplied relative path into a real file inside the vault.
    /// Error messages never contain the vault's absolute location.
    fn resolve(&self, rel: &str) -> Result<PathBuf> {
        Self::normalize(rel)?;
        let path = Path::new(rel);
        let real = canonical(&self.root.join(path))
            .map_err(|_| anyhow::anyhow!("page not found: {rel}"))?;
        // A symlink may lead outside the vault, or to a hidden or non-Markdown file.
        let inside = real
            .strip_prefix(&self.root)
            .map_err(|_| anyhow::anyhow!("invalid path: outside the vault"))?;
        if inside
            .components()
            .any(|c| is_hidden(&c.as_os_str().to_string_lossy()))
            || !has_md_extension(&real)
        {
            bail!("invalid path: not an accessible page");
        }
        if !real.is_file() {
            bail!("page not found: {rel}");
        }
        Ok(real)
    }

    pub fn read_page(&self, rel: &str) -> Result<String> {
        let real = self.resolve(rel)?;
        if fs::metadata(&real)?.len() > MAX_PAGE_BYTES {
            bail!("page too large (limit: {MAX_PAGE_BYTES} bytes): {rel}");
        }
        fs::read_to_string(&real).map_err(|_| anyhow::anyhow!("page is not valid UTF-8: {rel}"))
    }

    /// All Markdown pages, as sorted `/`-separated relative paths.
    /// Hidden entries are pruned and symlinks are not followed.
    pub fn all_pages(&self) -> Vec<String> {
        let mut pages: Vec<String> = WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !is_hidden(&e.file_name().to_string_lossy()))
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file() && has_md_extension(e.path()))
            .filter_map(|e| {
                let rel = e.path().strip_prefix(&self.root).ok()?;
                let parts: Vec<_> = rel.iter().map(|p| p.to_string_lossy()).collect();
                Some(parts.join("/"))
            })
            .collect();
        pages.sort();
        pages
    }

    pub fn list_pages(&self, prefix: Option<&str>, offset: usize, limit: usize) -> PageList {
        let limit = limit.clamp(1, MAX_LIST_LIMIT);
        let matching: Vec<String> = self
            .all_pages()
            .into_iter()
            .filter(|p| prefix.is_none_or(|prefix| p.starts_with(prefix)))
            .collect();
        let total = matching.len();
        let pages: Vec<String> = matching.into_iter().skip(offset).take(limit).collect();
        let end = offset.saturating_add(pages.len());
        PageList {
            next_offset: (end < total).then_some(end),
            pages,
            total,
        }
    }

    /// Case-insensitive search. A page matches when it contains every word of
    /// the query, in its path or its content; the most matches come first.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        if query.chars().count() > MAX_QUERY_CHARS {
            bail!("query too long (limit: {MAX_QUERY_CHARS} characters)");
        }
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if terms.is_empty() {
            bail!("query is empty");
        }

        let mut hits = Vec::new();
        for path in self.all_pages() {
            // Oversized or non-UTF-8 pages are simply not searchable.
            let Ok(content) = self.read_page(&path) else {
                continue;
            };
            let lower_content = content.to_lowercase();
            let lower_path = path.to_lowercase();
            if !terms
                .iter()
                .all(|t| lower_content.contains(t) || lower_path.contains(t))
            {
                continue;
            }
            let matches = terms
                .iter()
                .map(|t| lower_content.matches(t.as_str()).count())
                .sum();
            let snippet = content
                .lines()
                .find(|line| {
                    let line = line.to_lowercase();
                    terms.iter().any(|t| line.contains(t))
                })
                .unwrap_or_else(|| content.lines().next().unwrap_or(""))
                .trim()
                .chars()
                .take(SNIPPET_CHARS)
                .collect();
            hits.push(SearchHit {
                path,
                matches,
                snippet,
            });
        }
        hits.sort_by(|a, b| b.matches.cmp(&a.matches).then(a.path.cmp(&b.path)));
        hits.truncate(limit.clamp(1, MAX_SEARCH_LIMIT));
        Ok(hits)
    }
}

fn has_md_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}
