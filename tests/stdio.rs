//! Drives the real binary over stdio, the way an MCP client does.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};
use tempfile::tempdir;

#[test]
fn serves_the_vault_over_the_mcp_protocol() {
    let dir = tempdir().unwrap();
    let vault = dir.path().join("vault");
    let bin = env!("CARGO_BIN_EXE_wiki-llm-mcp");
    assert!(Command::new(bin)
        .arg("init")
        .arg(&vault)
        .status()
        .unwrap()
        .success());
    fs::write(
        vault.join("wiki/topic.md"),
        "# Topic\nabout borrow checkers",
    )
    .unwrap();

    let mut child = Command::new(bin)
        .arg("serve")
        .arg(&vault)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut send = |message: Value| writeln!(stdin, "{message}").unwrap();
    let mut receive = || {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    };

    send(
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},
        "clientInfo":{"name":"test","version":"0"}}}),
    );
    let info = receive();
    assert_eq!(info["result"]["serverInfo"]["name"], "wiki-llm-mcp");
    assert!(info["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("# Wiki rules"));
    send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));

    send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let tools = receive();
    let tools = tools["result"]["tools"].as_array().unwrap();
    let mut names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort();
    // Without --allow-delete there is no delete tool, and upload links only
    // exist in remote mode: this list is on purpose.
    assert_eq!(
        names,
        [
            "apply_changes",
            "list_files",
            "list_pages",
            "read_page",
            "recent_changes",
            "search"
        ]
    );
    let read_only = tools
        .iter()
        .filter(|t| t["annotations"]["readOnlyHint"] == true);
    assert_eq!(read_only.count(), 5);

    let mut call = |id: u32, name: &str, arguments: Value| {
        send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}}));
        receive()["result"].clone()
    };
    let text = |result: &Value| result["content"][0]["text"].as_str().unwrap().to_string();

    let read = call(3, "read_page", json!({"path":"wiki/topic.md"}));
    let page: Value = serde_json::from_str(&text(&read)).unwrap();
    assert_eq!(page["content"], "# Topic\nabout borrow checkers");
    assert_eq!(page["path"], "wiki/topic.md");
    let revision = page["revision"].as_str().unwrap().to_string();
    assert_eq!(revision.len(), 12);

    let found = call(4, "search", json!({"query":"BORROW"}));
    assert!(text(&found).contains("wiki/topic.md"));

    let listed = call(5, "list_pages", json!({"prefix":"wiki/"}));
    assert_eq!(
        serde_json::from_str::<Value>(&text(&listed)).unwrap()["total"],
        1
    );

    // The index file on disk predates topic.md, written without this tool.
    assert!(!fs::read_to_string(vault.join("WIKI-INDEX.md"))
        .unwrap()
        .contains("wiki/topic"));
    let index = call(11, "read_page", json!({"path":"WIKI-INDEX.md"}));
    assert!(text(&index).contains("[[wiki/topic|Topic]]"), "{index}");

    let refused = call(6, "read_page", json!({"path":"../../etc/passwd.md"}));
    assert_eq!(refused["isError"], true);

    let applied = call(
        7,
        "apply_changes",
        json!({"reason":"add a page","changes":[
            {"op":"create","path":"wiki/new.md","content":"# New"},
            {"op":"update","path":"wiki/topic.md","content":"# Topic\nabout lifetimes\n",
             "revision":revision},
            {"op":"edit","path":"wiki/topic.md","old_text":"lifetimes","new_text":"traits"},
            {"op":"append","path":"wiki/topic.md","content":"and macros\n"},
            {"op":"move","from":"wiki/topic.md","to":"wiki/renamed.md"}]}),
    );
    let report: Value = serde_json::from_str(&text(&applied)).unwrap();
    assert!(report["commit"].is_string(), "{report}");
    assert!(
        report["revisions"]["wiki/renamed.md"].is_string(),
        "{report}"
    );
    assert_eq!(
        std::fs::read_to_string(vault.join("wiki/renamed.md")).unwrap(),
        "# Topic\nabout traits\nand macros\n"
    );

    let stale = call(
        9,
        "apply_changes",
        json!({"reason":"x","changes":[
            {"op":"update","path":"wiki/renamed.md","content":"lost","revision":revision}]}),
    );
    assert_eq!(stale["isError"], true, "{stale}");
    let no_revision = call(
        10,
        "apply_changes",
        json!({"reason":"x","changes":[
            {"op":"update","path":"wiki/renamed.md","content":"lost"}]}),
    );
    assert!(
        no_revision.is_null() || no_revision["isError"] == true,
        "{no_revision}"
    );
    assert!(std::fs::read_to_string(vault.join("wiki/renamed.md"))
        .unwrap()
        .contains("traits"));

    let no_delete = call(
        8,
        "apply_changes",
        json!({"reason":"x","changes":[{"op":"delete","path":"wiki/new.md"}]}),
    );
    assert!(
        no_delete.is_null() || no_delete["isError"] == true,
        "{no_delete}"
    );
    let no_delete_tool = call(14, "delete", json!({"reason":"x","paths":["wiki/new.md"]}));
    assert!(
        no_delete_tool.is_null() || no_delete_tool["isError"] == true,
        "{no_delete_tool}"
    );
    assert!(vault.join("wiki/new.md").exists());

    let history = call(12, "recent_changes", json!({}));
    let history: Value = serde_json::from_str(&text(&history)).unwrap();
    assert_eq!(history[0]["by"], "claude", "{history}");
    assert_eq!(history[0]["message"], "add a page");
    assert!(history[0]["pages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["path"] == "wiki/new.md" && p["change"] == "created"));
    let bad_since = call(13, "recent_changes", json!({"since":"yesterday"}));
    assert_eq!(bad_since["isError"], true, "{bad_since}");

    drop(stdin);
    assert!(child.wait().unwrap().success());
}
