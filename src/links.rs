//! Keeps links valid when a page moves: `[[wikilinks]]` and `[text](path.md)`.
//! Code blocks and inline code are left untouched.

use std::cell::Cell;
use std::collections::BTreeSet;

fn strip_md(path: &str) -> &str {
    path.strip_suffix(".md")
        .or_else(|| path.strip_suffix(".MD"))
        .unwrap_or(path)
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn dirname(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Joins a relative target onto a directory; None if it climbs out of the vault.
fn join(dir: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

fn relative(from_dir: &str, to: &str) -> String {
    let from: Vec<&str> = from_dir.split('/').filter(|p| !p.is_empty()).collect();
    let dest: Vec<&str> = to.split('/').collect();
    let shared = from.iter().zip(&dest).take_while(|(a, b)| a == b).count();
    let mut parts = vec![".."; from.len() - shared];
    parts.extend(&dest[shared..]);
    parts.join("/")
}

fn is_external(target: &str) -> bool {
    target.contains("://") || target.starts_with("mailto:")
}

/// Applies `rewrite` to the text outside code, line by line.
fn outside_code(content: &str, mut rewrite: impl FnMut(&str) -> String) -> String {
    let mut out = String::with_capacity(content.len());
    let mut fence: Option<&str> = None;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let marker = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m));
        match (fence, marker) {
            (Some(open), Some(m)) if open == m => {
                fence = None;
                out.push_str(line);
                continue;
            }
            (Some(_), _) => {
                out.push_str(line);
                continue;
            }
            (None, Some(m)) => {
                fence = Some(m);
                out.push_str(line);
                continue;
            }
            (None, None) => {}
        }
        for (i, segment) in line.split('`').enumerate() {
            if i > 0 {
                out.push('`');
            }
            if i % 2 == 0 {
                out.push_str(&rewrite(segment));
            } else {
                out.push_str(segment);
            }
        }
    }
    out
}

/// Calls `new_target` on the target of each wikilink; alias and heading are kept.
fn map_wikilinks(text: &str, new_target: &impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let inner_start = start + 2;
        let Some(len) = rest[inner_start..].find("]]") else {
            break;
        };
        let inner = &rest[inner_start..inner_start + len];
        out.push_str(&rest[..inner_start]);
        let split = inner.find(['#', '|']).unwrap_or(inner.len());
        match new_target(inner[..split].trim()) {
            Some(target) => {
                out.push_str(&target);
                out.push_str(&inner[split..]);
            }
            None => out.push_str(inner),
        }
        out.push_str("]]");
        rest = &rest[inner_start + len + 2..];
    }
    out.push_str(rest);
    out
}

/// Calls `new_target` on the decoded path of each inline link.
fn map_markdown_links(text: &str, new_target: &impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("](") {
        let dest_start = start + 2;
        let Some(len) = rest[dest_start..].find(')') else {
            break;
        };
        let inner = &rest[dest_start..dest_start + len];
        out.push_str(&rest[..dest_start]);
        let dest_end = inner.find(char::is_whitespace).unwrap_or(inner.len());
        let (dest, title) = inner.split_at(dest_end);
        let path_end = dest.find('#').unwrap_or(dest.len());
        let (path, fragment) = dest.split_at(path_end);
        let replacement = (!path.is_empty() && !path.starts_with('<') && !is_external(path))
            .then(|| new_target(&path.replace("%20", " ")))
            .flatten();
        match replacement {
            Some(target) => {
                out.push_str(&target.replace(' ', "%20"));
                out.push_str(fragment);
                out.push_str(title);
            }
            None => out.push_str(inner),
        }
        out.push(')');
        rest = &rest[dest_start + len + 1..];
    }
    out.push_str(rest);
    out
}

fn base_is_unique(pages: &BTreeSet<String>, base: &str, except: Option<&str>) -> bool {
    pages
        .iter()
        .filter(|p| Some(p.as_str()) != except)
        .filter(|p| strip_md(basename(p)).eq_ignore_ascii_case(base))
        .count()
        <= usize::from(except.is_none())
}

/// Whether the target of a wikilink leads to `path`. A bare name only
/// designates it if no other page has the same name.
fn wikilink_designates(target: &str, path: &str, pages: &BTreeSet<String>) -> bool {
    let linked = strip_md(target);
    let full = strip_md(path);
    let base = basename(full);
    linked.eq_ignore_ascii_case(full)
        || (!linked.contains('/')
            && linked.eq_ignore_ascii_case(base)
            && base_is_unique(pages, base, None))
}

fn new_wikilink_target(
    target: &str,
    old: &str,
    new: &str,
    pages: &BTreeSet<String>,
) -> Option<String> {
    if !wikilink_designates(target, old, pages) {
        return None;
    }
    let linked = strip_md(target);
    let is_bare = !linked.contains('/');
    let new_path = strip_md(new);
    let new_base = basename(new_path);
    if is_bare && base_is_unique(pages, new_base, Some(old)) {
        return (!linked.eq_ignore_ascii_case(new_base)).then(|| new_base.to_string());
    }
    Some(new_path.to_string())
}

/// Updates, in `content` (the text of `page`), the links that lead to `old`.
/// `pages` is the set of pages before the move.
pub fn rewrite_for_move(
    content: &str,
    page: &str,
    old: &str,
    new: &str,
    pages: &BTreeSet<String>,
) -> String {
    let dir = dirname(page);
    let wikilink = |target: &str| new_wikilink_target(target, old, new, pages);
    let markdown = |target: &str| {
        let lower = target.to_ascii_lowercase();
        if !lower.ends_with(".md") {
            return None;
        }
        if let Some(from_root) = target.strip_prefix('/') {
            return (from_root == old).then(|| format!("/{new}"));
        }
        // Obsidian writes both page-relative and vault-root links.
        let resolved = join(dir, target).filter(|p| pages.contains(p));
        match resolved {
            Some(path) => (path == old).then(|| relative(dir, new)),
            None => (join("", target).as_deref() == Some(old)).then(|| new.to_string()),
        }
    };
    outside_code(content, |text| {
        map_markdown_links(&map_wikilinks(text, &wikilink), &markdown)
    })
}

/// Whether `content` (the text of `page`) links to `target`, a page or a file
/// about to be deleted. Used to report the links a deletion leaves behind.
pub fn links_to(content: &str, page: &str, target: &str, pages: &BTreeSet<String>) -> bool {
    let dir = dirname(page);
    let found = Cell::new(false);
    let wikilink = |link: &str| {
        if wikilink_designates(link, target, pages) {
            found.set(true);
        }
        None
    };
    let markdown = |link: &str| {
        let leads_there = match link.strip_prefix('/') {
            Some(from_root) => from_root == target,
            // Obsidian writes both page-relative and vault-root links.
            None => [join(dir, link), join("", link)]
                .iter()
                .any(|resolved| resolved.as_deref() == Some(target)),
        };
        if leads_there {
            found.set(true);
        }
        None
    };
    outside_code(content, |text| {
        map_markdown_links(&map_wikilinks(text, &wikilink), &markdown)
    });
    found.get()
}

/// Page-relative links of a moved page point somewhere else from its new
/// folder: recomputes those whose target exists.
pub fn rebase_own_links(
    content: &str,
    old_page: &str,
    new_page: &str,
    exists: &impl Fn(&str) -> bool,
) -> String {
    let (old_dir, new_dir) = (dirname(old_page), dirname(new_page));
    if old_dir == new_dir {
        return content.to_string();
    }
    let markdown = |target: &str| {
        if target.starts_with('/') {
            return None;
        }
        let resolved = join(old_dir, target).filter(|p| exists(p))?;
        Some(relative(new_dir, &resolved))
    };
    outside_code(content, |text| map_markdown_links(text, &markdown))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn moved(content: &str, page: &str) -> String {
        let all = pages(&["old/Note.md", "a.md", "dir/b.md"]);
        rewrite_for_move(content, page, "old/Note.md", "new/Renamed.md", &all)
    }

    #[test]
    fn rewrites_every_wikilink_form() {
        assert_eq!(moved("[[Note]]", "a.md"), "[[Renamed]]");
        assert_eq!(moved("[[old/Note]]", "a.md"), "[[new/Renamed]]");
        assert_eq!(moved("[[old/Note.md]]", "a.md"), "[[new/Renamed]]");
        assert_eq!(moved("[[Note|shown]]", "a.md"), "[[Renamed|shown]]");
        assert_eq!(moved("[[Note#Part|x]]", "a.md"), "[[Renamed#Part|x]]");
        assert_eq!(
            moved("![[Note]] [[note]]", "a.md"),
            "![[Renamed]] [[Renamed]]"
        );
    }

    #[test]
    fn leaves_other_links_alone() {
        for text in [
            "[[Other]]",
            "[[Notebook]]",
            "[[x/Note]]",
            "[x](http://a/Note.md)",
        ] {
            assert_eq!(moved(text, "a.md"), text);
        }
    }

    #[test]
    fn does_not_touch_code() {
        let fenced = "```\n[[Note]]\n```\n[[Note]]\n";
        assert_eq!(moved(fenced, "a.md"), "```\n[[Note]]\n```\n[[Renamed]]\n");
        assert_eq!(
            moved("`[[Note]]` [[Note]]", "a.md"),
            "`[[Note]]` [[Renamed]]"
        );
    }

    #[test]
    fn a_bare_name_shared_by_two_pages_is_ambiguous() {
        let all = pages(&["x/Note.md", "y/Note.md"]);
        let out = rewrite_for_move(
            "[[Note]] [[x/Note]]",
            "a.md",
            "x/Note.md",
            "z/Note.md",
            &all,
        );
        assert_eq!(out, "[[Note]] [[z/Note]]");
    }

    #[test]
    fn uses_the_full_path_when_the_new_name_is_taken() {
        let all = pages(&["old/Note.md", "z/Taken.md"]);
        let out = rewrite_for_move("[[Note]]", "a.md", "old/Note.md", "new/Taken.md", &all);
        assert_eq!(out, "[[new/Taken]]");
    }

    #[test]
    fn keeps_a_bare_link_when_only_the_folder_changes() {
        let all = pages(&["old/Note.md"]);
        let out = rewrite_for_move("[[Note]]", "a.md", "old/Note.md", "new/Note.md", &all);
        assert_eq!(out, "[[Note]]");
    }

    #[test]
    fn rewrites_markdown_links() {
        assert_eq!(moved("[t](old/Note.md)", "a.md"), "[t](new/Renamed.md)");
        assert_eq!(
            moved("[t](../old/Note.md#h)", "dir/b.md"),
            "[t](../new/Renamed.md#h)"
        );
        assert_eq!(
            moved("[t](/old/Note.md)", "dir/b.md"),
            "[t](/new/Renamed.md)"
        );
        assert_eq!(moved("[t](old/Note.md)", "dir/b.md"), "[t](new/Renamed.md)");
        assert_eq!(
            moved("[t](old/Note.md \"T\")", "a.md"),
            "[t](new/Renamed.md \"T\")"
        );
    }

    #[test]
    fn encodes_spaces_in_markdown_links() {
        let all = pages(&["my note.md"]);
        let out = rewrite_for_move(
            "[t](my%20note.md)",
            "a.md",
            "my note.md",
            "a b/c d.md",
            &all,
        );
        assert_eq!(out, "[t](a%20b/c%20d.md)");
    }

    #[test]
    fn rebases_the_moved_pages_own_links() {
        let exists = |p: &str| p == "dir/b.md" || p == "dir/img.png";
        let out = rebase_own_links(
            "[b](b.md) ![i](img.png) [no](missing.md) [w](https://x/y.md)",
            "dir/p.md",
            "top/deep/p.md",
            &exists,
        );
        assert_eq!(
            out,
            "[b](../../dir/b.md) ![i](../../dir/img.png) [no](missing.md) [w](https://x/y.md)"
        );
    }

    #[test]
    fn preserves_text_without_links() {
        let text = "line\r\n\nno links ] ( [[ unclosed\n";
        assert_eq!(moved(text, "a.md"), text);
    }
}
