//! Thin wrapper around the `git` command.

use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

const TOOL_IDENTITY: [&str; 8] = [
    "-c",
    "user.name=wiki-llm-mcp",
    "-c",
    "user.email=wiki-llm-mcp@localhost",
    "-c",
    "commit.gpgsign=false",
    // Git for Windows converts line endings by default: pages would change
    // on their own between a write and the next read.
    "-c",
    "core.autocrlf=false",
];

fn command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(TOOL_IDENTITY)
        .args(args)
        // Must not be redirected to another repository by the caller's environment.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        // Nobody can answer a prompt, and stdin carries the MCP protocol.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    command
}

pub fn git(root: &Path, args: &[&str]) -> Result<Output> {
    command(root, args)
        .output()
        .context("could not run git: is it installed?")
}

pub fn git_ok(root: &Path, args: &[&str]) -> Result<()> {
    let output = git(root, args)?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub fn git_stdout(root: &Path, args: &[&str]) -> Result<String> {
    let output = git(root, args)?;
    if !output.status.success() {
        bail!("git {} failed", args.first().unwrap_or(&""));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// For commands that use the network: an unreachable remote must not hold the
/// write lock forever. Returns the exit status and stderr.
pub fn git_network(root: &Path, args: &[&str], limit: Duration) -> Result<(ExitStatus, String)> {
    let mut child = command(root, args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run git: is it installed?")?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out after {}s", limit.as_secs());
        }
        sleep(Duration::from_millis(50));
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    Ok((status, stderr.trim().to_string()))
}

/// Commits what is staged; returns false when there was nothing to commit.
pub fn commit_staged(root: &Path, message: &str) -> Result<bool> {
    if !has_staged_changes(root)? {
        return Ok(false);
    }
    git_ok(root, &["commit", "-q", "-m", message])?;
    Ok(true)
}

/// Also true for the first commit of a repository, as long as files are staged.
pub fn has_staged_changes(root: &Path) -> Result<bool> {
    let is_unborn = !git(root, &["rev-parse", "--verify", "-q", "HEAD"])?
        .status
        .success();
    if is_unborn {
        return Ok(!git(root, &["ls-files"])?.stdout.is_empty());
    }
    Ok(!git(root, &["diff", "--cached", "--quiet"])?
        .status
        .success())
}
