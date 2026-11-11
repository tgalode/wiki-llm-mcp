//! Files that are not pages, and the two writes the owner has to allow:
//! deleting, and adding a file sent through an upload link.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

use tempfile::{tempdir, TempDir};
use wiki_llm_mcp::apply::{add_file, delete_paths, MAX_CHANGES};
use wiki_llm_mcp::init::{init, INDEX_FILE, RULES_FILE};
use wiki_llm_mcp::vault::{Vault, MAX_FILE_BYTES};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn commits(root: &Path) -> usize {
    git(root, &["rev-list", "--count", "HEAD"]).parse().unwrap()
}

fn vault_with(files: &[(&str, &[u8])]) -> (TempDir, Vault) {
    let dir = tempdir().unwrap();
    for (rel, content) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    init(dir.path()).unwrap();
    let vault = Vault::open(dir.path()).unwrap();
    (dir, vault)
}

fn paths(list: &[&str]) -> Vec<String> {
    list.iter().map(|p| p.to_string()).collect()
}

#[test]
fn lists_the_files_that_are_not_pages() {
    let (_dir, vault) = vault_with(&[
        ("wiki/a.md", b"# A"),
        ("raw/scan.pdf", b"%PDF-1.7"),
        ("raw/photos/one.jpg", b"jpeg"),
        (".obsidian/app.json", b"{}"),
        ("raw/.hidden.pdf", b"x"),
    ]);
    let all = vault.list_files(None, 0, 100);
    let names: Vec<&str> = all.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, ["raw/photos/one.jpg", "raw/scan.pdf"]);
    assert_eq!(all.files[1].bytes, 8);
    assert_eq!(all.total, 2);
    assert_eq!(all.next_offset, None);

    let first = vault.list_files(Some("raw/"), 0, 1);
    assert_eq!(first.files.len(), 1);
    assert_eq!(first.next_offset, Some(1));
    assert_eq!(vault.list_files(Some("wiki/"), 0, 100).total, 0);
}

#[test]
fn deletes_pages_and_files_in_one_commit_that_git_can_undo() {
    let (dir, vault) = vault_with(&[
        ("wiki/old.md", b"# Old"),
        ("wiki/keep.md", b"# Keep"),
        ("raw/scan.pdf", b"%PDF-1.7"),
    ]);
    let before = commits(dir.path());
    let report = delete_paths(
        &vault,
        &paths(&["wiki/old.md", "raw/scan.pdf"]),
        "the owner asked to drop the old draft",
    )
    .unwrap();

    assert!(!dir.path().join("wiki/old.md").exists());
    assert!(!dir.path().join("raw/scan.pdf").exists());
    assert!(dir.path().join("wiki/keep.md").exists());
    assert_eq!(commits(dir.path()), before + 1);
    assert_eq!(report.pages_changed, ["raw/scan.pdf", "wiki/old.md"]);
    assert_eq!(
        git(dir.path(), &["log", "-1", "--format=%an: %s"]),
        "Claude (wiki-llm-mcp): the owner asked to drop the old draft"
    );
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    assert!(!fs::read_to_string(dir.path().join(INDEX_FILE))
        .unwrap()
        .contains("wiki/old"));
    // Nothing is lost: the previous commit still holds both.
    assert_eq!(git(dir.path(), &["show", "HEAD~1:wiki/old.md"]), "# Old");
    assert_eq!(
        git(dir.path(), &["show", "HEAD~1:raw/scan.pdf"]),
        "%PDF-1.7"
    );
}

#[test]
fn one_path_that_cannot_be_deleted_means_nothing_is_deleted() {
    let (dir, vault) = vault_with(&[("wiki/a.md", b"# A"), ("raw/scan.pdf", b"pdf")]);
    let before = commits(dir.path());
    for bad in [
        "wiki/missing.md",
        "raw/missing.pdf",
        "raw",
        "../outside.md",
        ".git/config",
        ".obsidian/app.json",
        RULES_FILE,
        INDEX_FILE,
    ] {
        let result = delete_paths(&vault, &paths(&["wiki/a.md", bad]), "x");
        assert!(result.is_err(), "{bad} should be refused");
        assert!(dir.path().join("wiki/a.md").exists(), "{bad}");
    }
    // The same path twice: the second time, it is already gone.
    assert!(delete_paths(&vault, &paths(&["wiki/a.md", "wiki/a.md"]), "x").is_err());
    assert!(delete_paths(&vault, &[], "x").is_err());
    assert!(delete_paths(&vault, &paths(&["wiki/a.md"]), " ").is_err());
    let too_many = vec!["wiki/a.md".to_string(); MAX_CHANGES + 1];
    assert!(delete_paths(&vault, &too_many, "x").is_err());

    assert!(dir.path().join("wiki/a.md").exists());
    assert!(dir.path().join(RULES_FILE).exists());
    assert_eq!(commits(dir.path()), before);
}

#[cfg(unix)]
#[test]
fn a_delete_never_goes_through_a_symlink() {
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("target.pdf"), "precious").unwrap();
    let (dir, vault) = vault_with(&[("wiki/a.md", b"# A")]);
    symlink(outside.path(), dir.path().join("raw/linked")).unwrap();
    symlink(
        outside.path().join("target.pdf"),
        dir.path().join("raw/link.pdf"),
    )
    .unwrap();

    assert!(delete_paths(&vault, &paths(&["raw/linked/target.pdf"]), "x").is_err());
    assert!(delete_paths(&vault, &paths(&["raw/link.pdf"]), "x").is_err());
    assert!(outside.path().join("target.pdf").exists());
    assert!(fs::symlink_metadata(dir.path().join("raw/link.pdf")).is_ok());
}

#[test]
fn links_left_behind_by_a_delete_are_reported_not_rewritten() {
    let (dir, vault) = vault_with(&[
        ("wiki/gone.md", b"# Gone"),
        ("raw/scan.pdf", b"pdf"),
        ("wiki/by-name.md", b"see [[gone]] and ![[scan.pdf]]"),
        (
            "wiki/by-path.md",
            b"see [it](gone.md) and [scan](../raw/scan.pdf)",
        ),
        ("wiki/unrelated.md", b"see [[other]] and `[[gone]]`"),
    ]);
    let report =
        delete_paths(&vault, &paths(&["wiki/gone.md", "raw/scan.pdf"]), "cleanup").unwrap();

    assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
    for warning in &report.warnings {
        assert!(warning.contains("wiki/by-name.md"), "{warning}");
        assert!(warning.contains("wiki/by-path.md"), "{warning}");
        assert!(!warning.contains("unrelated"), "{warning}");
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("wiki/by-name.md")).unwrap(),
        "see [[gone]] and ![[scan.pdf]]"
    );
    assert_eq!(report.links_updated, 0);
}

#[test]
fn an_uploaded_file_is_committed_as_the_owners() {
    let (dir, vault) = vault_with(&[("wiki/a.md", b"# A")]);
    fs::write(dir.path().join("wiki/a.md"), "# A, edited in Obsidian").unwrap();
    let before = commits(dir.path());
    let bytes = vec![0u8, 159, 146, 150, 255];
    let report = add_file(&vault, "raw/2026/scan.pdf", bytes.clone()).unwrap();

    assert_eq!(
        fs::read(dir.path().join("raw/2026/scan.pdf")).unwrap(),
        bytes
    );
    assert_eq!(report.pages_changed, ["raw/2026/scan.pdf"]);
    assert!(report.commit.is_some());
    // The owner's pending edit got its own commit first, as for any write.
    assert_eq!(commits(dir.path()), before + 2);
    assert_eq!(
        git(dir.path(), &["log", "-1", "--format=%an: %s"]),
        "wiki-llm-mcp: Upload raw/2026/scan.pdf"
    );
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[test]
fn an_upload_never_replaces_or_escapes() {
    let (dir, vault) = vault_with(&[("wiki/a.md", b"# A"), ("raw/scan.pdf", b"original")]);
    let before = commits(dir.path());
    for bad in [
        "raw/scan.pdf",
        "wiki/a.md",
        "../outside.pdf",
        "/etc/outside.pdf",
        "raw/.hidden.pdf",
        ".git/hooks/pre-commit",
        ".obsidian/app.json",
        RULES_FILE,
        INDEX_FILE,
    ] {
        assert!(
            add_file(&vault, bad, b"new".to_vec()).is_err(),
            "{bad} should be refused"
        );
    }
    let too_large = vec![0u8; MAX_FILE_BYTES as usize + 1];
    assert!(add_file(&vault, "raw/huge.bin", too_large).is_err());

    assert_eq!(
        fs::read(dir.path().join("raw/scan.pdf")).unwrap(),
        b"original"
    );
    assert!(!dir.path().join("raw/huge.bin").exists());
    assert_eq!(commits(dir.path()), before);
}

#[cfg(unix)]
#[test]
fn an_upload_never_goes_through_a_symlink() {
    let outside = tempdir().unwrap();
    let (dir, vault) = vault_with(&[("wiki/a.md", b"# A")]);
    symlink(outside.path(), dir.path().join("raw/linked")).unwrap();
    assert!(add_file(&vault, "raw/linked/x.pdf", b"x".to_vec()).is_err());
    assert!(!outside.path().join("x.pdf").exists());
}
