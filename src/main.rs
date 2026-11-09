use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rmcp::{transport::stdio, ServiceExt};
use wiki_llm_mcp::{init::init, server::WikiServer, vault::Vault};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Prepare a vault (empty or existing): folders, rules, Git and index.
    /// Existing notes are not moved.
    Init { path: PathBuf },
    /// Serve a vault to an MCP client over stdio.
    Serve { path: PathBuf },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Commands::Init { path } => {
            let report = init(&path)?;
            println!("Vault ready: {}", path.display());
            if report.git_initialized {
                println!("- Git repository created");
            }
            if report.snapshot_committed {
                println!("- Original state of your notes committed");
            }
            println!("- {} page(s) indexed", report.pages_indexed);
        }
        Commands::Serve { path } => {
            // stdout carries the MCP protocol: anything human goes to stderr.
            let server = WikiServer::new(Vault::open(&path)?);
            eprintln!("wiki-llm-mcp: serving {} over stdio", path.display());
            server.serve(stdio()).await?.waiting().await?;
        }
    }
    Ok(())
}
