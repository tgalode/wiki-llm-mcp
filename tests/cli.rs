//! The commands that make a first start need no preparation: `serve --init`
//! and the generated secret.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::tempdir;

const BIN: &str = env!("CARGO_BIN_EXE_wiki-llm-mcp");

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn serve_remote(vault: &Path, port: u16, secret: Option<&str>) -> Child {
    let address = format!("127.0.0.1:{port}");
    let mut command = Command::new(BIN);
    command
        .args(["serve", "--init", "--public-url"])
        .arg(format!("http://{address}"))
        .args(["--bind", &address])
        .arg(vault)
        .env_remove("WIKI_LLM_MCP_SECRET")
        .stderr(Stdio::null());
    if let Some(secret) = secret {
        command.env("WIKI_LLM_MCP_SECRET", secret);
    }
    let child = command.spawn().unwrap();
    let started = Instant::now();
    while TcpStream::connect(&address).is_err() {
        assert!(started.elapsed() < Duration::from_secs(20), "server not up");
        sleep(Duration::from_millis(50));
    }
    child
}

fn get(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn secret_command(vault: &Path, extra: &[&str]) -> (bool, String) {
    let output = Command::new(BIN)
        .arg("secret")
        .arg(vault)
        .args(extra)
        .env_remove("WIKI_LLM_MCP_SECRET")
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap().trim().to_string(),
    )
}

#[test]
fn serve_init_prepares_a_folder_of_notes_and_serves_it() {
    let dir = tempdir().unwrap();
    let vault = dir.path().join("notes");
    fs::create_dir_all(&vault).unwrap();
    fs::write(vault.join("idea.md"), "# Idea\nmine").unwrap();

    let mut child = Command::new(BIN)
        .args(["serve", "--init"])
        .arg(&vault)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"test","version":"0"}}})
    )
    .unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let info: Value = serde_json::from_str(&line).unwrap();
    assert!(info["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("# Wiki rules"));
    drop(stdin);
    assert!(child.wait().unwrap().success());

    assert!(vault.join(".git").is_dir() && vault.join("WIKI-INDEX.md").is_file());
    assert_eq!(
        fs::read_to_string(vault.join("idea.md")).unwrap(),
        "# Idea\nmine"
    );
}

#[test]
fn serve_without_init_leaves_an_unprepared_folder_alone() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("idea.md"), "mine").unwrap();
    let port = free_port();
    let address = format!("127.0.0.1:{port}");
    let status = Command::new(BIN)
        .args(["serve", "--public-url"])
        .arg(format!("http://{address}"))
        .args(["--bind", &address])
        .arg(dir.path())
        .env_remove("WIKI_LLM_MCP_SECRET")
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success());
    assert!(!dir.path().join(".git").exists());
}

#[test]
fn a_secret_is_generated_once_and_kept_across_restarts() {
    let dir = tempdir().unwrap();
    let vault = dir.path().join("vault");
    let port = free_port();

    let mut server = serve_remote(&vault, port, None);
    assert!(get(port, "/healthz").contains("200 OK"));
    let (found, secret) = secret_command(&vault, &[]);
    assert!(found);
    assert!(secret.len() >= 40, "{secret}");
    let file = vault.join(".git/wiki-llm-mcp/secret");
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Out of the pages and out of what Git tracks.
    let status = Command::new("git")
        .arg("-C")
        .arg(&vault)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(status.stdout.is_empty());
    server.kill().unwrap();
    server.wait().unwrap();

    let mut server = serve_remote(&vault, port, None);
    assert_eq!(secret_command(&vault, &[]).1, secret);
    server.kill().unwrap();
    server.wait().unwrap();

    let (created, renewed) = secret_command(&vault, &["--new"]);
    assert!(created);
    assert_ne!(renewed, secret);
    assert_eq!(secret_command(&vault, &[]).1, renewed);
}

#[test]
fn the_environment_variable_wins_and_nothing_is_stored() {
    let dir = tempdir().unwrap();
    let vault = dir.path().join("vault");
    let port = free_port();
    let mut server = serve_remote(&vault, port, Some("a secret chosen by the owner"));
    assert!(get(port, "/healthz").contains("200 OK"));
    server.kill().unwrap();
    server.wait().unwrap();

    assert!(!vault.join(".git/wiki-llm-mcp/secret").exists());
    assert!(!secret_command(&vault, &[]).0);
}
