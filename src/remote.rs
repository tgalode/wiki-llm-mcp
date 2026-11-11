//! Remote mode: MCP over HTTP, behind the built-in OAuth server. TLS is left to
//! the reverse proxy or tunnel in front of it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::DefaultBodyLimit;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post, put};
use axum::Router;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use crate::git::git_stdout;
use crate::oauth::{
    authorization_server, authorize_page, authorize_submit, protected_resource, register,
    require_token, token, OAuth, OAuthConfig, TokenStore,
};
use crate::server::{Options, WikiServer};
use crate::upload::{self, Uploads};
use crate::vault::{canonical, Vault, MAX_FILE_BYTES};

const MAX_MCP_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_OAUTH_BODY_BYTES: usize = 64 * 1024;

/// Inside `.git`: never committed, and out of reach of the page tools.
pub fn state_dir(vault: &Vault) -> Result<PathBuf> {
    state_dir_of(vault.root())
}

fn state_dir_of(root: &Path) -> Result<PathBuf> {
    let git_dir = git_stdout(root, &["rev-parse", "--absolute-git-dir"])
        .context("this vault has no Git repository: run `wiki-llm-mcp init` first")?;
    let git_dir = PathBuf::from(git_dir);
    Ok(canonical(&git_dir).unwrap_or(git_dir).join("wiki-llm-mcp"))
}

/// What the owner allowed on the command line. Nothing by default.
#[derive(Debug, Clone, Copy, Default)]
pub struct Allowed {
    pub delete: bool,
    pub upload: bool,
}

pub fn build_app(vault: Vault, config: OAuthConfig, allowed: Allowed) -> Result<Router> {
    let uploads = allowed
        .upload
        .then(|| Uploads::new(vault.clone(), &config.public_url));
    build_app_with(vault, config, allowed.delete, uploads)
}

pub fn build_app_with(
    vault: Vault,
    config: OAuthConfig,
    allow_delete: bool,
    uploads: Option<Uploads>,
) -> Result<Router> {
    let oauth = OAuth::new(config)?;
    let server = WikiServer::with_options(
        vault,
        Options {
            allow_delete,
            uploads: uploads.clone(),
        },
    );
    let mcp = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_sse_keep_alive(None)
            .with_max_request_body_bytes(MAX_MCP_BODY_BYTES)
            .with_allowed_hosts(oauth.host_names()),
    );
    let protected = Router::new()
        .nest_service("/mcp", mcp)
        .layer(DefaultBodyLimit::max(MAX_MCP_BODY_BYTES))
        .layer(from_fn_with_state(oauth.clone(), require_token));
    let open = Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .route("/register", post(register))
        .route("/authorize", get(authorize_page).post(authorize_submit))
        .route("/token", post(token))
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(MAX_OAUTH_BODY_BYTES))
        .with_state(oauth);
    let app = open.merge(protected);
    // An upload link is its own credential: whoever holds it may add files
    // for as long as it lives, without a token.
    Ok(match uploads {
        Some(uploads) => app.merge(
            Router::new()
                .route("/upload/{token}", get(upload::page))
                .route("/upload/{token}/{name}", put(upload::receive))
                .layer(DefaultBodyLimit::max(MAX_FILE_BYTES as usize))
                .with_state(uploads),
        ),
        None => app,
    })
}

pub async fn serve(
    vault: Vault,
    config: OAuthConfig,
    allowed: Allowed,
    bind: SocketAddr,
) -> Result<()> {
    let public_url = config.public_url.clone();
    let app = build_app(vault, config, allowed)?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("could not listen on {bind}"))?;
    eprintln!("wiki-llm-mcp: listening on {bind}, connector URL: {public_url}/mcp");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Disconnects every client. A running server applies it on the next request.
pub fn revoke_all(vault: &Vault) -> Result<usize> {
    TokenStore::open(&state_dir(vault)?)?.revoke_all()
}
