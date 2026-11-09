use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmcp::{transport::stdio, ServiceExt};
use wiki_llm_mcp::{
    init::init,
    oauth::{check_secret, normalize_public_url, Lifetimes, OAuthConfig, SECRET_ENV},
    remote,
    server::WikiServer,
    vault::Vault,
};

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
    /// Serve a vault: over stdio by default, over HTTP with --public-url.
    Serve {
        path: PathBuf,
        /// HTTPS origin clients reach this server at. Enables remote mode and
        /// requires the WIKI_LLM_MCP_SECRET environment variable.
        #[arg(long)]
        public_url: Option<String>,
        /// Address to listen on in remote mode.
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: SocketAddr,
        /// OAuth redirect URI to accept in addition to Claude's (repeatable).
        #[arg(long = "allow-redirect-uri")]
        allow_redirect_uri: Vec<String>,
    },
    /// Disconnect every remote client of a vault.
    Revoke { path: PathBuf },
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
        Commands::Serve {
            path,
            public_url: None,
            ..
        } => {
            // stdout carries the MCP protocol: anything human goes to stderr.
            let server = WikiServer::new(Vault::open(&path)?);
            eprintln!("wiki-llm-mcp: serving {} over stdio", path.display());
            server.serve(stdio()).await?.waiting().await?;
        }
        Commands::Serve {
            path,
            public_url: Some(public_url),
            bind,
            allow_redirect_uri,
        } => {
            // In the environment: a command-line argument is visible to every process.
            let secret = std::env::var(SECRET_ENV)
                .with_context(|| format!("remote mode requires the {SECRET_ENV} variable"))?;
            check_secret(&secret)?;
            let public_url = normalize_public_url(&public_url)?;
            let vault = Vault::open(&path)?;
            let config = OAuthConfig {
                public_url,
                secret,
                state_dir: remote::state_dir(&vault)?,
                extra_redirects: allow_redirect_uri,
                lifetimes: Lifetimes::default(),
            };
            remote::serve(vault, config, bind).await?;
        }
        Commands::Revoke { path } => {
            let count = remote::revoke_all(&Vault::open(&path)?)?;
            println!("{count} session(s) revoked");
        }
    }
    Ok(())
}
