use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;

use tempfile::{tempdir, TempDir};
use wiki_llm_mcp::vault::{Vault, MAX_LIST_LIMIT, MAX_PAGE_BYTES, MAX_QUERY_CHARS};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// A vault next to a private file, so escapes have something real to reach.
fn fixture() -> (TempDir, Vault) {
    let dir = tempdir().unwrap();
    let root = dir.path().join("vault");
    fs::create_dir(&root).unwrap();
    write(dir.path(), "secret.md", "TOP SECRET");
    write(&root, "a.md", "# Alpha\nRust and Obsidian");
    write(&root, "wiki/b.md", "rust rust RUST");
    write(&root, "wiki/deep/c.md", "Obsidian only");
    write(&root, "notes.txt", "rust");
    write(&root, ".obsidian/hidden.md", "rust");
    write(&root, ".git/config.md", "rust");
    let vault = Vault::open(&root).unwrap();
    (dir, vault)
}

#[test]
fn lists_only_visible_markdown_pages() {
    let (_dir, vault) = fixture();
    assert_eq!(vault.all_pages(), ["a.md", "wiki/b.md", "wiki/deep/c.md"]);
}

#[test]
fn paginates_and_filters_by_prefix() {
    let (_dir, vault) = fixture();
    let first = vault.list_pages(None, 0, 2);
    assert_eq!(first.pages, ["a.md", "wiki/b.md"]);
    assert_eq!((first.total, first.next_offset), (3, Some(2)));
    let last = vault.list_pages(None, 2, 2);
    assert_eq!(last.pages, ["wiki/deep/c.md"]);
    assert_eq!(last.next_offset, None);
    assert_eq!(vault.list_pages(Some("wiki/"), 0, 10).total, 2);
    assert!(vault.list_pages(None, usize::MAX, 10).pages.is_empty());
}

#[test]
fn caps_the_list_limit() {
    let dir = tempdir().unwrap();
    for i in 0..(MAX_LIST_LIMIT + 5) {
        write(dir.path(), &format!("{i}.md"), "x");
    }
    let vault = Vault::open(dir.path()).unwrap();
    assert_eq!(
        vault.list_pages(None, 0, 10_000).pages.len(),
        MAX_LIST_LIMIT
    );
}

#[test]
fn reads_a_page() {
    let (_dir, vault) = fixture();
    assert_eq!(vault.read_page("wiki/b.md").unwrap(), "rust rust RUST");
}

#[test]
fn refuses_paths_that_leave_or_probe_the_vault() {
    let (_dir, vault) = fixture();
    for path in [
        "../secret.md",
        "wiki/../../secret.md",
        "/etc/passwd.md",
        ".obsidian/hidden.md",
        ".git/config.md",
        "wiki/./b.md/../../../secret.md",
        "notes.txt",
        "a.md\0",
        "wiki\\b.md",
        "",
        "missing.md",
    ] {
        assert!(vault.read_page(path).is_err(), "should refuse {path:?}");
    }
}

#[test]
fn errors_do_not_reveal_the_vault_location() {
    let (_dir, vault) = fixture();
    let location = vault.root().to_string_lossy().into_owned();
    for path in ["missing.md", "../secret.md", "notes.txt"] {
        let error = vault.read_page(path).unwrap_err().to_string();
        assert!(!error.contains(&location), "leak in {error:?}");
    }
}

#[cfg(unix)]
#[test]
fn refuses_symlinks_escaping_or_reaching_hidden_files() {
    let (dir, vault) = fixture();
    let root = vault.root().to_path_buf();
    symlink(dir.path().join("secret.md"), root.join("out.md")).unwrap();
    symlink(root.join(".git/config.md"), root.join("to-git.md")).unwrap();
    symlink(root.join("notes.txt"), root.join("to-txt.md")).unwrap();
    symlink(dir.path(), root.join("outdir")).unwrap();
    for path in ["out.md", "to-git.md", "to-txt.md", "outdir/secret.md"] {
        assert!(vault.read_page(path).is_err(), "should refuse {path}");
    }
    let listed = vault.all_pages().join(" ");
    assert!(!listed.contains("out") && !listed.contains("to-"));
    assert!(vault.search("SECRET", 50).unwrap().is_empty());
}

#[test]
fn refuses_oversized_pages_and_skips_them_in_search() {
    let dir = tempdir().unwrap();
    write(
        dir.path(),
        "big.md",
        &"needle ".repeat(MAX_PAGE_BYTES as usize / 6),
    );
    let vault = Vault::open(dir.path()).unwrap();
    assert!(vault.read_page("big.md").is_err());
    assert!(vault.search("needle", 10).unwrap().is_empty());
}

#[test]
fn searches_case_insensitively_requiring_every_word() {
    let (_dir, vault) = fixture();
    let both = vault.search("RUST obsidian", 10).unwrap();
    assert_eq!(both.len(), 1);
    assert_eq!(both[0].path, "a.md");
    assert_eq!(both[0].snippet, "Rust and Obsidian");
    let ranked = vault.search("rust", 10).unwrap();
    assert_eq!(ranked[0].path, "wiki/b.md");
    assert_eq!(ranked[0].matches, 3);
    assert_eq!(vault.search("rust", 1).unwrap().len(), 1);
}

#[test]
fn searches_page_paths_too() {
    let (_dir, vault) = fixture();
    let hits = vault.search("deep", 10).unwrap();
    assert_eq!(hits[0].path, "wiki/deep/c.md");
}

#[test]
fn refuses_empty_or_oversized_queries() {
    let (_dir, vault) = fixture();
    assert!(vault.search("   ", 10).is_err());
    assert!(vault.search(&"x".repeat(MAX_QUERY_CHARS + 1), 10).is_err());
}

#[test]
fn snippets_do_not_split_multibyte_characters() {
    let dir = tempdir().unwrap();
    write(dir.path(), "u.md", &format!("clé {}", "é".repeat(500)));
    let vault = Vault::open(dir.path()).unwrap();
    assert_eq!(
        vault.search("clé", 1).unwrap()[0].snippet.chars().count(),
        200
    );
}

#[test]
fn recognizes_the_names_windows_resolves_to_another_file() {
    use wiki_llm_mcp::vault::is_windows_alias;
    for name in ["GIT~1", "note.md:stream", "note.md.", "note.md ", "a:b.md"] {
        assert!(is_windows_alias(name), "{name}");
    }
    for name in ["note.md", "my note.md", "v1.2 notes.md", "été.md"] {
        assert!(!is_windows_alias(name), "{name}");
    }
}

#[cfg(windows)]
#[test]
fn refuses_windows_aliases_of_protected_files() {
    let (_dir, vault) = fixture();
    for path in ["GIT~1/config.md", "note.md:stream.md", "wiki./a.md"] {
        assert!(vault.read_page(path).is_err(), "should refuse {path}");
        assert!(Vault::normalize(path).is_err(), "should refuse {path}");
    }
}
