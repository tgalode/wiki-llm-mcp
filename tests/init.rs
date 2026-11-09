use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::tempdir;
use wiki_llm_mcp::init::{init, INDEX_FILE, RULES_FILE};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).unwrap()
}

fn commit_count(root: &Path) -> usize {
    git(root, &["rev-list", "--count", "HEAD"])
        .trim()
        .parse()
        .unwrap()
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn sets_up_an_empty_vault() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("new-vault");
    let report = init(&root).unwrap();
    assert!(report.git_initialized);
    assert!(!report.snapshot_committed);
    assert_eq!(report.pages_indexed, 0);
    for path in [
        RULES_FILE,
        INDEX_FILE,
        ".gitignore",
        "raw/.gitkeep",
        "wiki/.gitkeep",
    ] {
        assert!(root.join(path).exists(), "missing {path}");
    }
    assert_eq!(commit_count(&root), 1);
    assert_eq!(git(&root, &["status", "--porcelain"]), "");
}

#[test]
fn keeps_existing_notes_in_place_and_snapshots_them_first() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "journal/day.md", "# My day\ntext");
    write(root, ".obsidian/workspace.json", "{}");
    write(root, ".obsidian/app.json", "{}");
    let report = init(root).unwrap();

    assert!(report.snapshot_committed);
    assert_eq!(report.pages_indexed, 1);
    assert_eq!(
        fs::read_to_string(root.join("journal/day.md")).unwrap(),
        "# My day\ntext"
    );
    assert_eq!(commit_count(root), 2);

    let first = git(root, &["ls-tree", "-r", "--name-only", "HEAD~1"]);
    assert!(first.contains("journal/day.md"));
    assert!(!first.contains(RULES_FILE));
    assert!(!git(root, &["ls-files"]).contains("workspace.json"));
    assert!(git(root, &["ls-files"]).contains(".obsidian/app.json"));
    assert_eq!(git(root, &["status", "--porcelain"]), "");
}

#[test]
fn indexes_notes_with_their_titles() {
    let dir = tempdir().unwrap();
    write(dir.path(), "wiki/topic.md", "# A | Title\ntext");
    write(dir.path(), "plain.md", "no heading");
    init(dir.path()).unwrap();
    let index = fs::read_to_string(dir.path().join(INDEX_FILE)).unwrap();
    assert!(index.contains("- [[wiki/topic|A   Title]]"), "{index}");
    assert!(index.contains("- [[plain|plain]]"), "{index}");
    assert!(!index.contains("WIKI"), "{index}");
}

#[test]
fn snapshots_uncommitted_work_in_an_existing_repository() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    write(root, "note.md", "draft");
    let report = init(root).unwrap();
    assert!(!report.git_initialized);
    assert!(report.snapshot_committed);
    assert_eq!(git(root, &["show", "HEAD~1:note.md"]), "draft");
}

#[test]
fn can_be_run_again_without_changing_anything() {
    let dir = tempdir().unwrap();
    write(dir.path(), "note.md", "text");
    init(dir.path()).unwrap();
    let before = commit_count(dir.path());
    let report = init(dir.path()).unwrap();
    assert!(!report.snapshot_committed);
    assert_eq!(commit_count(dir.path()), before);
}

#[test]
fn keeps_the_users_rules_and_gitignore() {
    let dir = tempdir().unwrap();
    write(dir.path(), RULES_FILE, "my rules");
    write(dir.path(), ".gitignore", "private/");
    init(dir.path()).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join(RULES_FILE)).unwrap(),
        "my rules"
    );
    let ignore = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(
        ignore.starts_with("private/\n") && ignore.contains(".trash/"),
        "{ignore}"
    );
}

#[test]
fn refuses_to_overwrite_a_foreign_index_and_changes_nothing() {
    let dir = tempdir().unwrap();
    write(dir.path(), INDEX_FILE, "my own file");
    assert!(init(dir.path()).is_err());
    assert_eq!(
        fs::read_to_string(dir.path().join(INDEX_FILE)).unwrap(),
        "my own file"
    );
    assert!(!dir.path().join(".git").exists());
}

#[test]
fn refuses_a_folder_inside_another_repository() {
    let dir = tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    let nested = dir.path().join("nested");
    fs::create_dir(&nested).unwrap();
    write(&nested, "note.md", "text");
    assert!(init(&nested).is_err());
    assert!(!nested.join("raw").exists());
    assert!(!dir.path().join(".gitignore").exists());
}
