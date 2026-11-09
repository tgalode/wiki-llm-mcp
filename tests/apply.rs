use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::process::Command;
use std::thread;

use tempfile::{tempdir, TempDir};
use wiki_llm_mcp::apply::{apply_changes, Change, MAX_CHANGES};
use wiki_llm_mcp::init::{init, INDEX_FILE, RULES_FILE};
use wiki_llm_mcp::vault::{Vault, MAX_PAGE_BYTES};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
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

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap()
}

fn create(path: &str, content: &str) -> Change {
    Change::Create {
        path: path.into(),
        content: content.into(),
    }
}

fn update(path: &str, content: &str) -> Change {
    Change::Update {
        path: path.into(),
        content: content.into(),
    }
}

fn mv(from: &str, to: &str) -> Change {
    Change::Move {
        from: from.into(),
        to: to.into(),
    }
}

fn vault_with(pages: &[(&str, &str)]) -> (TempDir, Vault) {
    let dir = tempdir().unwrap();
    for (path, content) in pages {
        write(dir.path(), path, content);
    }
    init(dir.path()).unwrap();
    let vault = Vault::open(dir.path()).unwrap();
    (dir, vault)
}

#[test]
fn several_pages_make_a_single_commit_signed_by_the_llm() {
    let (dir, vault) = vault_with(&[("wiki/a.md", "old")]);
    let before = commits(dir.path());
    let report = apply_changes(
        &vault,
        &[
            create("wiki/b.md", "# B"),
            update("wiki/a.md", "new"),
            create("new/dir/c.md", "c"),
        ],
        "integrate an article",
    )
    .unwrap();

    assert_eq!(commits(dir.path()), before + 1);
    assert_eq!(read(dir.path(), "wiki/a.md"), "new");
    assert_eq!(read(dir.path(), "new/dir/c.md"), "c");
    assert_eq!(
        report.commit.unwrap(),
        git(dir.path(), &["rev-parse", "--short", "HEAD"])
    );
    assert_eq!(
        git(dir.path(), &["log", "-1", "--format=%an"]),
        "Claude (wiki-llm-mcp)"
    );
    assert_eq!(
        git(dir.path(), &["log", "-1", "--format=%s"]),
        "integrate an article"
    );
    assert!(git(dir.path(), &["log", "-1", "--format=%b"]).contains("wiki/b.md"));
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    assert!(read(dir.path(), INDEX_FILE).contains("[[wiki/b|B]]"));
}

#[test]
fn one_invalid_change_means_nothing_is_written() {
    let (dir, vault) = vault_with(&[("wiki/a.md", "old")]);
    let before = commits(dir.path());
    let error = apply_changes(
        &vault,
        &[
            update("wiki/a.md", "new"),
            create("wiki/b.md", "b"),
            update("missing.md", "x"),
        ],
        "r",
    )
    .unwrap_err();

    assert!(error.to_string().contains("missing.md"), "{error}");
    assert_eq!(read(dir.path(), "wiki/a.md"), "old");
    assert!(!dir.path().join("wiki/b.md").exists());
    assert_eq!(commits(dir.path()), before);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[test]
fn create_never_overwrites() {
    let (dir, vault) = vault_with(&[("wiki/a.md", "mine")]);
    for path in ["wiki/a.md", "wiki/A.md"] {
        assert!(
            apply_changes(&vault, &[create(path, "x")], "r").is_err(),
            "{path}"
        );
    }
    assert_eq!(read(dir.path(), "wiki/a.md"), "mine");
}

#[test]
fn a_reason_is_mandatory_and_inputs_are_bounded() {
    let (_dir, vault) = vault_with(&[]);
    assert!(apply_changes(&vault, &[create("a.md", "x")], " \n ").is_err());
    assert!(apply_changes(&vault, &[], "r").is_err());
    let many = vec![create("a.md", "x"); MAX_CHANGES + 1];
    assert!(apply_changes(&vault, &many, "r").is_err());
    let huge = "x".repeat(MAX_PAGE_BYTES as usize + 1);
    assert!(apply_changes(&vault, &[create("a.md", &huge)], "r").is_err());
}

#[test]
fn a_multiline_reason_stays_on_the_subject_line() {
    let (dir, vault) = vault_with(&[]);
    apply_changes(&vault, &[create("a.md", "x")], "first\nsecond").unwrap();
    assert_eq!(
        git(dir.path(), &["log", "-1", "--format=%s"]),
        "first second"
    );
}

#[test]
fn refuses_paths_outside_visible_markdown() {
    let (dir, vault) = vault_with(&[]);
    fs::create_dir(dir.path().join("elsewhere")).unwrap();
    let outside = tempdir().unwrap();
    symlink(outside.path(), dir.path().join("link")).unwrap();
    for path in [
        "../x.md",
        "/etc/x.md",
        ".git/x.md",
        ".obsidian/x.md",
        "a.txt",
        "link/x.md",
    ] {
        assert!(
            apply_changes(&vault, &[create(path, "x")], "r").is_err(),
            "{path}"
        );
    }
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn refuses_writing_through_a_symlinked_page() {
    let (dir, vault) = vault_with(&[("wiki/real.md", "real")]);
    symlink(
        dir.path().join("wiki/real.md"),
        dir.path().join("wiki/alias.md"),
    )
    .unwrap();
    assert!(apply_changes(&vault, &[update("wiki/alias.md", "x")], "r").is_err());
    assert!(apply_changes(&vault, &[create("wiki/alias.md", "x")], "r").is_err());
    assert_eq!(read(dir.path(), "wiki/real.md"), "real");
}

#[test]
fn rules_and_index_cannot_be_changed() {
    let (dir, vault) = vault_with(&[("a.md", "x")]);
    let rules = read(dir.path(), RULES_FILE);
    for file in [RULES_FILE, INDEX_FILE, "wiki.md"] {
        assert!(
            apply_changes(&vault, &[update(file, "x")], "r").is_err(),
            "{file}"
        );
    }
    assert!(apply_changes(&vault, &[mv(RULES_FILE, "b.md")], "r").is_err());
    assert_eq!(read(dir.path(), RULES_FILE), rules);
}

#[test]
fn raw_sources_can_be_added_but_not_altered() {
    let (dir, vault) = vault_with(&[]);
    apply_changes(&vault, &[create("raw/source.md", "original")], "add source").unwrap();
    assert!(apply_changes(&vault, &[update("raw/source.md", "x")], "r").is_err());
    assert!(apply_changes(&vault, &[mv("raw/source.md", "wiki/s.md")], "r").is_err());
    assert_eq!(read(dir.path(), "raw/source.md"), "original");
}

#[test]
fn the_owners_uncommitted_work_is_saved_separately_first() {
    let (dir, vault) = vault_with(&[("note.md", "committed")]);
    let before = commits(dir.path());
    write(dir.path(), "note.md", "edited in obsidian");
    write(dir.path(), "other.md", "new in obsidian");

    apply_changes(&vault, &[update("note.md", "from the llm")], "r").unwrap();

    assert_eq!(commits(dir.path()), before + 2);
    assert_eq!(
        git(dir.path(), &["show", "HEAD~1:note.md"]),
        "edited in obsidian"
    );
    assert_eq!(
        git(dir.path(), &["show", "HEAD~1:other.md"]),
        "new in obsidian"
    );
    assert_eq!(
        git(dir.path(), &["log", "-1", "--format=%an", "HEAD~1"]),
        "wiki-llm-mcp"
    );
    assert_eq!(read(dir.path(), "note.md"), "from the llm");
}

#[test]
fn identical_content_makes_no_commit() {
    let (dir, vault) = vault_with(&[("a.md", "same")]);
    let before = commits(dir.path());
    let report = apply_changes(&vault, &[update("a.md", "same")], "r").unwrap();
    assert_eq!(report.commit, None);
    assert_eq!(commits(dir.path()), before);
}

#[test]
fn moving_keeps_content_history_and_links_in_one_commit() {
    // Git recognizes a rename by similarity: the note needs more than one line.
    let body = "A paragraph of the note.\n".repeat(10);
    let note = format!("{body}[up](../top.md)\n");
    let (dir, vault) = vault_with(&[
        ("old/Note.md", note.as_str()),
        (
            "top.md",
            "see [[Note]] and [t](old/Note.md)\n```\n[[Note]]\n```",
        ),
        ("raw/src.md", "[[Note]]"),
        ("journal.md", "no link"),
    ]);
    let before = commits(dir.path());
    let report = apply_changes(
        &vault,
        &[mv("old/Note.md", "new/deep/Renamed.md")],
        "reorganize",
    )
    .unwrap();

    assert_eq!(commits(dir.path()), before + 1);
    assert!(!dir.path().join("old/Note.md").exists());
    assert_eq!(
        read(dir.path(), "new/deep/Renamed.md"),
        format!("{body}[up](../../top.md)\n")
    );
    assert_eq!(
        read(dir.path(), "top.md"),
        "see [[Renamed]] and [t](new/deep/Renamed.md)\n```\n[[Note]]\n```"
    );
    assert_eq!(read(dir.path(), "raw/src.md"), "[[Note]]");
    assert_eq!(report.links_updated, 1);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    let index = read(dir.path(), INDEX_FILE);
    assert!(
        index.contains("new/deep/Renamed") && !index.contains("old/Note"),
        "{index}"
    );
    assert!(
        git(
            dir.path(),
            &[
                "log",
                "--follow",
                "--format=%h",
                "--",
                "new/deep/Renamed.md"
            ]
        )
        .lines()
        .count()
            >= 2
    );
}

#[test]
fn moving_never_overwrites_a_page() {
    let (dir, vault) = vault_with(&[("a.md", "A"), ("b.md", "B")]);
    assert!(apply_changes(&vault, &[mv("a.md", "b.md")], "r").is_err());
    assert_eq!(
        (read(dir.path(), "a.md"), read(dir.path(), "b.md")),
        ("A".into(), "B".into())
    );
}

#[test]
fn chained_changes_see_each_other() {
    let (dir, vault) = vault_with(&[("a.md", "A"), ("link.md", "[[a]]")]);
    apply_changes(
        &vault,
        &[
            update("a.md", "A2"),
            mv("a.md", "b.md"),
            create("a.md", "fresh"),
            update("link.md", "[[b]] edited"),
        ],
        "r",
    )
    .unwrap();
    assert_eq!(read(dir.path(), "b.md"), "A2");
    assert_eq!(read(dir.path(), "a.md"), "fresh");
    assert_eq!(read(dir.path(), "link.md"), "[[b]] edited");
}

#[test]
fn concurrent_callers_are_serialized() {
    let (dir, vault) = vault_with(&[]);
    let before = commits(dir.path());
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let vault = vault.clone();
            thread::spawn(move || {
                apply_changes(&vault, &[create(&format!("p{i}.md"), "x")], "r").unwrap();
            })
        })
        .collect();
    handles.into_iter().for_each(|h| h.join().unwrap());
    assert_eq!(commits(dir.path()), before + 8);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[test]
fn a_failing_commit_restores_the_vault() {
    let (dir, vault) = vault_with(&[("a.md", "old"), ("b.md", "[[a]]")]);
    let hook = dir.path().join(".git/hooks/pre-commit");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let before = commits(dir.path());

    assert!(apply_changes(
        &vault,
        &[
            update("a.md", "new"),
            create("c.md", "c"),
            mv("b.md", "d.md")
        ],
        "r"
    )
    .is_err());

    assert_eq!(commits(dir.path()), before);
    assert_eq!(read(dir.path(), "a.md"), "old");
    assert_eq!(read(dir.path(), "b.md"), "[[a]]");
    assert!(!dir.path().join("c.md").exists() && !dir.path().join("d.md").exists());
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[test]
fn refuses_a_vault_without_git() {
    let dir = tempdir().unwrap();
    write(dir.path(), "a.md", "x");
    let vault = Vault::open(dir.path()).unwrap();
    let error = apply_changes(&vault, &[update("a.md", "y")], "r").unwrap_err();
    assert!(error.to_string().contains("init"), "{error}");
    assert_eq!(read(dir.path(), "a.md"), "x");
}

#[test]
fn refuses_to_write_during_a_git_operation() {
    let (dir, vault) = vault_with(&[("a.md", "x")]);
    fs::write(dir.path().join(".git/MERGE_HEAD"), "x").unwrap();
    assert!(apply_changes(&vault, &[update("a.md", "y")], "r").is_err());
    assert_eq!(read(dir.path(), "a.md"), "x");
}

/// A bare remote, the vault tracking it, and a second clone acting as another device.
fn with_remote(pages: &[(&str, &str)]) -> (TempDir, Vault, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let bare = dir.path().join("remote.git");
    let vault_path = dir.path().join("vault");
    let device = dir.path().join("device");
    fs::create_dir_all(&vault_path).unwrap();
    git(
        dir.path(),
        &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    for (path, content) in pages {
        write(&vault_path, path, content);
    }
    init(&vault_path).unwrap();
    git(
        &vault_path,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    git(&vault_path, &["push", "-q", "-u", "origin", "main"]);
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            device.to_str().unwrap(),
        ],
    );
    let vault = Vault::open(&vault_path).unwrap();
    (dir, vault, device)
}

fn commit_on_device(device: &Path, rel: &str, content: &str) {
    write(device, rel, content);
    git(device, &["add", "-A"]);
    git(device, &["commit", "-q", "-m", "from device"]);
    git(device, &["push", "-q"]);
}

#[test]
fn pushes_each_commit_to_the_remote() {
    let (_dir, vault, device) = with_remote(&[]);
    let report = apply_changes(&vault, &[create("a.md", "x")], "r").unwrap();
    assert!(report.pushed);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    git(&device, &["pull", "-q"]);
    assert_eq!(read(&device, "a.md"), "x");
}

#[test]
fn takes_in_remote_changes_before_writing() {
    let (_dir, vault, device) = with_remote(&[("a.md", "x")]);
    commit_on_device(&device, "from-device.md", "device note");
    apply_changes(&vault, &[update("from-device.md", "improved")], "r").unwrap();
    assert_eq!(read(vault.root(), "from-device.md"), "improved");
    git(&device, &["pull", "-q"]);
    assert_eq!(read(&device, "from-device.md"), "improved");
}

#[test]
fn a_conflict_with_the_remote_stops_the_write_and_forces_nothing() {
    let (_dir, vault, device) = with_remote(&[("a.md", "x")]);
    commit_on_device(&device, "a.md", "device version");
    write(vault.root(), "a.md", "local version");

    let error = apply_changes(&vault, &[create("b.md", "new")], "r").unwrap_err();

    assert!(error.to_string().contains("conflict"), "{error}");
    assert!(!vault.root().join("b.md").exists());
    assert_eq!(read(vault.root(), "a.md"), "local version");
    assert!(!vault.root().join(".git/rebase-merge").exists());
    git(&device, &["pull", "-q"]);
    assert_eq!(read(&device, "a.md"), "device version");
}

#[test]
fn an_unreachable_remote_only_delays_the_push() {
    let (dir, vault, _device) = with_remote(&[]);
    fs::rename(dir.path().join("remote.git"), dir.path().join("gone.git")).unwrap();

    let report = apply_changes(&vault, &[create("a.md", "x")], "offline").unwrap();
    assert!(report.commit.is_some());
    assert!(!report.pushed);
    assert!(!report.warnings.is_empty());

    fs::rename(dir.path().join("gone.git"), dir.path().join("remote.git")).unwrap();
    let report = apply_changes(&vault, &[create("b.md", "y")], "back online").unwrap();
    assert!(report.pushed, "{:?}", report.warnings);
    let bare = dir.path().join("remote.git");
    let tree = git(&bare, &["ls-tree", "--name-only", "main"]);
    assert!(tree.contains("a.md") && tree.contains("b.md"), "{tree}");
}
