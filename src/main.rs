use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmcp::{transport::stdio, ServiceExt};
use wiki_llm_mcp::{
    init::{init, is_initialized},
    oauth::{
        check_secret, create_secret, normalize_public_url, stored_secret, Lifetimes, OAuthConfig,
        SECRET_ENV,
    },
    remote,
    server::{Options, WikiServer},
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
        /// Prepare the vault first, as `init` does, if it is not ready yet.
        #[arg(long)]
        init: bool,
        /// HTTPS origin clients reach this server at. Enables remote mode. The
        /// secret comes from the WIKI_LLM_MCP_SECRET environment variable, or
        /// is generated and stored on the first start: see `secret`.
        #[arg(long)]
        public_url: Option<String>,
        /// Address to listen on in remote mode.
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: SocketAddr,
        /// OAuth redirect URI to accept in addition to Claude's (repeatable).
        #[arg(long = "allow-redirect-uri")]
        allow_redirect_uri: Vec<String>,
        /// Offer a tool that deletes pages and files. Git keeps what they
        /// contained. Without this flag, nothing can be deleted.
        #[arg(long)]
        allow_delete: bool,
        /// Offer a tool that gives the owner a short-lived link to send files
        /// (PDF, images) from a browser. Remote mode only.
        #[arg(long, requires = "public_url")]
        allow_upload: bool,
    },
    /// Disconnect every remote client of a vault.
    Revoke { path: PathBuf },
    /// Print the secret that was generated for a vault served without
    /// WIKI_LLM_MCP_SECRET.
    Secret {
        path: PathBuf,
        /// Generate a new secret. Restart the server to apply it.
        #[arg(long)]
        new: bool,
    },
}

/// The variable counts only when it holds something: an empty value in an
/// environment file means it is not used.
fn secret_from_env() -> Option<String> {
    std::env::var(SECRET_ENV).ok().filter(|s| !s.is_empty())
}

fn open_vault(path: &Path, prepare: bool) -> Result<Vault> {
    if prepare && !is_initialized(path) {
        let report = init(path)?;
        eprintln!(
            "wiki-llm-mcp: vault prepared, {} page(s) indexed",
            report.pages_indexed
        );
    }
    Vault::open(path)
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
            init,
            public_url: None,
            allow_delete,
            ..
        } => {
            // stdout carries the MCP protocol: anything human goes to stderr.
            let options = Options {
                allow_delete,
                uploads: None,
            };
            let server = WikiServer::with_options(open_vault(&path, init)?, options);
            eprintln!("wiki-llm-mcp: serving {} over stdio", path.display());
            server.serve(stdio()).await?.waiting().await?;
        }
        Commands::Serve {
            path,
            init,
            public_url: Some(public_url),
            bind,
            allow_redirect_uri,
            allow_delete,
            allow_upload,
        } => {
            let public_url = normalize_public_url(&public_url)?;
            let vault = open_vault(&path, init)?;
            let state_dir = remote::state_dir(&vault)?;
            // Never a command-line argument: those are visible to every process.
            let secret = match secret_from_env() {
                Some(secret) => secret,
                None => match stored_secret(&state_dir) {
                    Some(secret) => secret,
                    None => {
                        let secret = create_secret(&state_dir)?;
                        eprintln!(
                            "wiki-llm-mcp: no {SECRET_ENV}, so a secret was generated. \
                             Display it with: wiki-llm-mcp secret {}",
                            path.display()
                        );
                        secret
                    }
                },
            };
            check_secret(&secret)?;
            let config = OAuthConfig {
                public_url,
                secret,
                state_dir,
                extra_redirects: allow_redirect_uri,
                lifetimes: Lifetimes::default(),
            };
            let allowed = remote::Allowed {
                delete: allow_delete,
                upload: allow_upload,
            };
            remote::serve(vault, config, allowed, bind).await?;
        }
        Commands::Revoke { path } => {
            let count = remote::revoke_all(&Vault::open(&path)?)?;
            println!("{count} session(s) revoked");
        }
        Commands::Secret { path, new } => {
            let state_dir = remote::state_dir(&Vault::open(&path)?)?;
            if secret_from_env().is_some() {
                eprintln!(
                    "wiki-llm-mcp: {SECRET_ENV} is set and takes precedence over the stored secret"
                );
            }
            let secret = if new {
                let secret = create_secret(&state_dir)?;
                eprintln!(
                    "wiki-llm-mcp: new secret stored. Restart the server to apply it; \
                     `revoke` disconnects the clients already connected"
                );
                secret
            } else {
                stored_secret(&state_dir).context(
                    "no stored secret: the server was never started in remote mode without \
                     the environment variable. `--new` generates one",
                )?
            };
            println!("{secret}");
        }
    }
    Ok(())
}
