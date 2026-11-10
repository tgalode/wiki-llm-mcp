use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::{tempdir, TempDir};
use wiki_llm_mcp::apply::{apply_changes, Change};
use wiki_llm_mcp::history::{recent_changes, PageChange, MAX_HISTORY_LIMIT};
use wiki_llm_mcp::init::init;
use wiki_llm_mcp::vault::Vault;

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "user.name=Owner", "-c", "user.email=o@o"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn owner_commit(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", message]);
}

fn vault() -> (TempDir, Vault) {
    let dir = tempdir().unwrap();
    init(dir.path()).unwrap();
    let vault = Vault::open(dir.path()).unwrap();
    (dir, vault)
}

fn create(path: &str, content: &str) -> Change {
    Change::Create {
        path: path.into(),
        content: content.into(),
    }
}

#[test]
fn tells_the_owners_changes_from_the_llms_newest_first() {
    let (dir, vault) = vault();
    write(dir.path(), "notes/Été à Téllé.md", "from Obsidian");
    owner_commit(dir.path(), "note written in Obsidian");
    apply_changes(&vault, &[create("wiki/page.md", "# Page")], "add a page").unwrap();

    let history = recent_changes(&vault, None, 2).unwrap();

    assert_eq!(history.len(), 2);
    assert_eq!(history[0].by, "claude");
    assert_eq!(history[0].message, "add a page");
    assert_eq!(
        history[0].pages,
        [PageChange {
            path: "wiki/page.md".into(),
            change: "created",
            from: None
        }]
    );
    assert_eq!(history[1].by, "owner");
    assert_eq!(history[1].message, "note written in Obsidian");
    // Accented names come out as they are, not as escape sequences.
    assert_eq!(history[1].pages[0].path, "notes/Été à Téllé.md");
    assert!(history[0].date.starts_with("20"), "{}", history[0].date);
}

#[test]
fn reports_edits_moves_and_deletions() {
    let (dir, vault) = vault();
    let body = "A paragraph of the note.\n".repeat(10);
    write(dir.path(), "a.md", &body);
    write(dir.path(), "b.md", "b");
    write(dir.path(), "c.md", "c");
    owner_commit(dir.path(), "three notes");
    fs::create_dir_all(dir.path().join("moved")).unwrap();
    fs::rename(dir.path().join("a.md"), dir.path().join("moved/a.md")).unwrap();
    write(dir.path(), "b.md", "b, edited");
    fs::remove_file(dir.path().join("c.md")).unwrap();
    owner_commit(dir.path(), "reorganized in Obsidian");

    let history = recent_changes(&vault, None, 1).unwrap();

    let found = |path: &str| history[0].pages.iter().find(|p| p.path == path).unwrap();
    assert_eq!(found("moved/a.md").change, "moved");
    assert_eq!(found("moved/a.md").from.as_deref(), Some("a.md"));
    assert_eq!(found("b.md").change, "changed");
    assert_eq!(found("c.md").change, "deleted");
}

#[test]
fn shows_only_pages_within_reach() {
    let (dir, vault) = vault();
    write(dir.path(), ".obsidian/app.json", "{}");
    write(dir.path(), "raw/scan.pdf", "binary");
    owner_commit(dir.path(), "settings and an attachment");
    apply_changes(&vault, &[create("wiki/page.md", "# Page")], "add a page").unwrap();

    let history = recent_changes(&vault, None, MAX_HISTORY_LIMIT).unwrap();

    let paths: Vec<&str> = history
        .iter()
        .flat_map(|e| e.pages.iter().map(|p| p.path.as_str()))
        .collect();
    // Neither hidden files, nor attachments, nor the generated index; a
    // commit left with nothing to show is skipped.
    assert_eq!(paths, ["wiki/page.md", "WIKI.md"]);
}

#[test]
fn since_keeps_only_later_changes_and_must_be_a_date() {
    let (_dir, vault) = vault();
    apply_changes(&vault, &[create("wiki/page.md", "# Page")], "add a page").unwrap();

    assert_eq!(
        recent_changes(&vault, Some("2000-01-01"), 10)
            .unwrap()
            .len(),
        2
    );
    assert!(recent_changes(&vault, Some("2099-01-01T00:00:00"), 10)
        .unwrap()
        .is_empty());
    for bad in [
        "yesterday",
        "2026-11",
        "2026-11-09 --all",
        "--all",
        "2999-01-01",
        "2026-13-01",
    ] {
        assert!(recent_changes(&vault, Some(bad), 10).is_err(), "{bad}");
    }
}

#[test]
fn a_vault_without_its_own_repository_has_no_history() {
    let dir = tempdir().unwrap();
    write(dir.path(), "a.md", "x");
    let vault = Vault::open(dir.path()).unwrap();
    assert!(recent_changes(&vault, None, 10).is_err());
}
