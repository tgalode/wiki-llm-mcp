//! A minimal OAuth 2.1 authorization server for a single owner, who proves
//! their identity with a secret. Only what Claude's connectors need: discovery,
//! dynamic registration, authorization code with PKCE, rotating refresh tokens.
//!
//! Tokens are stored hashed, outside the vault's working tree.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use axum::extract::{Form, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;

pub const SECRET_ENV: &str = "WIKI_LLM_MCP_SECRET";
pub const MIN_SECRET_CHARS: usize = 20;
const CLAUDE_CALLBACK: &str = "https://claude.ai/api/mcp/auth_callback";
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];
const MAX_CLIENT_ID_CHARS: usize = 4096;
const MAX_REDIRECT_URIS: usize = 10;
const MAX_PENDING_CODES: usize = 256;
const MAX_SESSIONS: usize = 100;
const MAX_LOGIN_FAILURES: usize = 10;
const LOGIN_WINDOW_SECS: u64 = 15 * 60;
const SCOPE: &str = "wiki";

#[derive(Debug, Clone, Copy)]
pub struct Lifetimes {
    pub code_secs: u64,
    pub access_secs: u64,
    pub refresh_secs: u64,
}

impl Default for Lifetimes {
    fn default() -> Self {
        Self {
            code_secs: 60,
            access_secs: 60 * 60,
            refresh_secs: 90 * 24 * 60 * 60,
        }
    }
}

pub struct OAuthConfig {
    /// Origin the clients use, e.g. `https://wiki.example.com`.
    pub public_url: String,
    pub secret: String,
    pub state_dir: PathBuf,
    /// Redirect URIs accepted in addition to Claude's and loopback addresses.
    pub extra_redirects: Vec<String>,
    pub lifetimes: Lifetimes,
}

/// A public URL is a bare origin, in HTTPS except on the loopback interface.
pub fn normalize_public_url(raw: &str) -> Result<String> {
    let url = Url::parse(raw).context("--public-url is not a valid URL")?;
    let host = url.host_str().unwrap_or("");
    if url.scheme() != "https" && !(url.scheme() == "http" && LOOPBACK_HOSTS.contains(&host)) {
        bail!("--public-url must use https");
    }
    if host.is_empty() || url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        bail!("--public-url must be a bare origin, such as https://wiki.example.com");
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

pub fn check_secret(secret: &str) -> Result<()> {
    if secret.chars().count() < MIN_SECRET_CHARS {
        bail!("{SECRET_ENV} must be at least {MIN_SECRET_CHARS} characters long");
    }
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub(crate) fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no system randomness: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn sha256(value: &str) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}

fn hash_token(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(sha256(token))
}

fn same_secret(a: &str, b: &str) -> bool {
    sha256(a).ct_eq(&sha256(b)).into()
}

// --------------------------------------------------------------------- secret

const SECRET_FILE: &str = "secret";

/// The secret kept beside the tokens, for a server started without the
/// environment variable. `None` when there is none yet.
pub fn stored_secret(state_dir: &Path) -> Option<String> {
    fs::read_to_string(state_dir.join(SECRET_FILE))
        .ok()
        .map(|secret| secret.trim().to_string())
        .filter(|secret| !secret.is_empty())
}

/// Generates a secret and stores it, replacing the previous one. A running
/// server keeps the one it started with.
pub fn create_secret(state_dir: &Path) -> Result<String> {
    fs::create_dir_all(state_dir).context("could not create the state directory")?;
    restrict(state_dir, 0o700)?;
    let secret = random_token()?;
    let temporary = state_dir.join("secret.tmp");
    fs::write(&temporary, format!("{secret}\n")).context("could not store the secret")?;
    restrict(&temporary, 0o600)?;
    fs::rename(&temporary, state_dir.join(SECRET_FILE)).context("could not store the secret")?;
    Ok(secret)
}

// ---------------------------------------------------------------- token store

#[derive(Debug, Default, Serialize, Deserialize)]
struct Sessions {
    sessions: Vec<Session>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    access: String,
    access_expires: u64,
    refresh: String,
    refresh_expires: u64,
    client: String,
    created: u64,
}

/// Files are replaced atomically, so reads need no lock and a `revoke` run from
/// another process takes effect on the next request.
pub struct TokenStore {
    dir: PathBuf,
}

impl TokenStore {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).context("could not create the state directory")?;
        restrict(dir, 0o700)?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    fn path(&self) -> PathBuf {
        self.dir.join("tokens.json")
    }

    /// A damaged file means no valid token: it fails closed.
    fn load(&self) -> Sessions {
        fs::read(self.path())
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    fn update<T>(&self, change: impl FnOnce(&mut Sessions) -> T) -> Result<T> {
        let lock = File::create(self.dir.join("tokens.lock"))?;
        lock.lock()?;
        let mut sessions = self.load();
        let output = change(&mut sessions);
        let time = now();
        sessions.sessions.retain(|s| s.refresh_expires > time);
        let excess = sessions.sessions.len().saturating_sub(MAX_SESSIONS);
        sessions.sessions.drain(..excess);

        let temporary = self.dir.join("tokens.json.tmp");
        fs::write(&temporary, serde_json::to_vec(&sessions)?)?;
        restrict(&temporary, 0o600)?;
        fs::rename(&temporary, self.path())?;
        Ok(output)
    }

    pub fn is_valid_access(&self, token: &str) -> bool {
        let hash = hash_token(token);
        let time = now();
        self.load()
            .sessions
            .iter()
            .any(|s| s.access == hash && s.access_expires > time)
    }

    pub fn revoke_all(&self) -> Result<usize> {
        self.update(|sessions| {
            let count = sessions.sessions.len();
            sessions.sessions.clear();
            count
        })
    }
}

fn restrict(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

// --------------------------------------------------------------------- state

struct PendingCode {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    expires: u64,
}

pub struct OAuth {
    public_url: String,
    secret: String,
    extra_redirects: Vec<String>,
    lifetimes: Lifetimes,
    store: TokenStore,
    codes: Mutex<HashMap<String, PendingCode>>,
    login_failures: Mutex<VecDeque<u64>>,
}

pub type SharedOAuth = Arc<OAuth>;

/// The client's details live in its id: registration stores nothing, so it
/// cannot be used to fill the server, and every use is validated again.
#[derive(Debug, Serialize, Deserialize)]
struct Client {
    redirect_uris: Vec<String>,
    name: String,
}

fn is_loopback(url: &Url) -> bool {
    url.scheme() == "http" && url.host_str().is_some_and(|h| LOOPBACK_HOSTS.contains(&h))
}

impl OAuth {
    pub fn new(config: OAuthConfig) -> Result<SharedOAuth> {
        check_secret(&config.secret)?;
        Ok(Arc::new(Self {
            public_url: normalize_public_url(&config.public_url)?,
            secret: config.secret,
            extra_redirects: config.extra_redirects,
            lifetimes: config.lifetimes,
            store: TokenStore::open(&config.state_dir)?,
            codes: Mutex::new(HashMap::new()),
            login_failures: Mutex::new(VecDeque::new()),
        }))
    }

    pub fn host_names(&self) -> Vec<String> {
        let url = Url::parse(&self.public_url).expect("validated at startup");
        let host = url.host_str().unwrap_or("").to_string();
        match url.port() {
            Some(port) => vec![format!("{host}:{port}")],
            None => vec![host],
        }
    }

    fn resource(&self) -> String {
        format!("{}/mcp", self.public_url)
    }

    fn resource_metadata_url(&self) -> String {
        format!("{}/.well-known/oauth-protected-resource", self.public_url)
    }

    /// Only known callbacks: otherwise a link could send the owner's
    /// authorization to an address chosen by whoever registered the client.
    fn redirect_is_allowed(&self, uri: &str) -> bool {
        if uri == CLAUDE_CALLBACK || self.extra_redirects.iter().any(|e| e == uri) {
            return true;
        }
        Url::parse(uri).is_ok_and(|url| is_loopback(&url) && url.fragment().is_none())
    }

    fn decode_client(&self, client_id: &str) -> Option<Client> {
        if client_id.chars().count() > MAX_CLIENT_ID_CHARS {
            return None;
        }
        let bytes = URL_SAFE_NO_PAD.decode(client_id).ok()?;
        let client: Client = serde_json::from_slice(&bytes).ok()?;
        client
            .redirect_uris
            .iter()
            .all(|uri| self.redirect_is_allowed(uri))
            .then_some(client)
    }

    fn resource_matches(&self, resource: Option<&str>) -> bool {
        resource.is_none_or(|r| {
            let r = r.trim_end_matches('/');
            r == self.resource() || r == self.public_url
        })
    }

    fn validate_authorization(&self, params: &AuthorizeParams) -> Result<Client, &'static str> {
        if params.response_type.as_deref() != Some("code") {
            return Err("unsupported response_type");
        }
        if params.code_challenge_method.as_deref() != Some("S256") {
            return Err("PKCE with S256 is required");
        }
        let challenge = params.code_challenge.as_deref().unwrap_or("");
        if challenge.len() != 43 {
            return Err("invalid code_challenge");
        }
        let client = params
            .client_id
            .as_deref()
            .and_then(|id| self.decode_client(id))
            .ok_or("unknown client")?;
        let redirect = params.redirect_uri.as_deref().unwrap_or("");
        if !redirect_matches(&client, redirect) || !self.redirect_is_allowed(redirect) {
            return Err("redirect_uri not allowed");
        }
        if !self.resource_matches(params.resource.as_deref()) {
            return Err("invalid resource");
        }
        Ok(client)
    }

    fn login_is_blocked(&self) -> bool {
        let mut failures = self.login_failures.lock().expect("not poisoned");
        let oldest_counted = now().saturating_sub(LOGIN_WINDOW_SECS);
        while failures.front().is_some_and(|t| *t < oldest_counted) {
            failures.pop_front();
        }
        failures.len() >= MAX_LOGIN_FAILURES
    }

    fn record_login_failure(&self) {
        self.login_failures
            .lock()
            .expect("not poisoned")
            .push_back(now());
    }

    fn issue_code(&self, params: &AuthorizeParams) -> Result<String> {
        let code = random_token()?;
        let time = now();
        let mut codes = self.codes.lock().expect("not poisoned");
        codes.retain(|_, c| c.expires > time);
        if codes.len() >= MAX_PENDING_CODES {
            bail!("too many pending authorizations");
        }
        codes.insert(
            hash_token(&code),
            PendingCode {
                client_id: params.client_id.clone().unwrap_or_default(),
                redirect_uri: params.redirect_uri.clone().unwrap_or_default(),
                challenge: params.code_challenge.clone().unwrap_or_default(),
                expires: time + self.lifetimes.code_secs,
            },
        );
        Ok(code)
    }

    fn issue_session(&self, client: &str, sessions: &mut Sessions) -> Result<TokenResponse> {
        let (access, refresh) = (random_token()?, random_token()?);
        let time = now();
        sessions.sessions.push(Session {
            access: hash_token(&access),
            access_expires: time + self.lifetimes.access_secs,
            refresh: hash_token(&refresh),
            refresh_expires: time + self.lifetimes.refresh_secs,
            client: client.to_string(),
            created: time,
        });
        Ok(TokenResponse {
            access_token: access,
            token_type: "Bearer",
            expires_in: self.lifetimes.access_secs,
            refresh_token: refresh,
            scope: SCOPE,
        })
    }

    fn exchange_code(&self, form: &TokenForm) -> Result<TokenResponse, OAuthFailure> {
        // Single use: taken out before any check.
        let pending = self
            .codes
            .lock()
            .expect("not poisoned")
            .remove(&hash_token(form.code.as_deref().unwrap_or("")))
            .ok_or(OAuthFailure::InvalidGrant)?;
        let verifier = form.code_verifier.as_deref().unwrap_or("");
        let computed = URL_SAFE_NO_PAD.encode(sha256(verifier));
        let verified: bool = computed
            .as_bytes()
            .ct_eq(pending.challenge.as_bytes())
            .into();
        if pending.expires <= now()
            || !verified
            || !(43..=128).contains(&verifier.len())
            || form.client_id.as_deref() != Some(pending.client_id.as_str())
            || form.redirect_uri.as_deref() != Some(pending.redirect_uri.as_str())
            || !self.resource_matches(form.resource.as_deref())
        {
            return Err(OAuthFailure::InvalidGrant);
        }
        self.store
            .update(|sessions| self.issue_session(&pending.client_id, sessions))
            .map_err(|_| OAuthFailure::Server)?
            .map_err(|_| OAuthFailure::Server)
    }

    fn refresh(&self, form: &TokenForm) -> Result<TokenResponse, OAuthFailure> {
        let hash = hash_token(form.refresh_token.as_deref().unwrap_or(""));
        let client = form.client_id.clone().unwrap_or_default();
        let time = now();
        self.store
            .update(|sessions| {
                let position = sessions
                    .sessions
                    .iter()
                    .position(|s| s.refresh == hash && s.refresh_expires > time)
                    .ok_or(OAuthFailure::InvalidGrant)?;
                if sessions.sessions[position].client != client {
                    return Err(OAuthFailure::InvalidGrant);
                }
                // Rotation: the old refresh token dies with this response.
                sessions.sessions.remove(position);
                self.issue_session(&client, sessions)
                    .map_err(|_| OAuthFailure::Server)
            })
            .map_err(|_| OAuthFailure::Server)?
    }
}

fn redirect_matches(client: &Client, requested: &str) -> bool {
    let Ok(requested_url) = Url::parse(requested) else {
        return false;
    };
    client.redirect_uris.iter().any(|registered| {
        if registered == requested {
            return true;
        }
        // Native clients get a different local port on every run.
        Url::parse(registered).is_ok_and(|url| {
            is_loopback(&url)
                && is_loopback(&requested_url)
                && url.host_str() == requested_url.host_str()
                && url.path() == requested_url.path()
        })
    })
}

// ------------------------------------------------------------------ handlers

pub async fn protected_resource(State(oauth): State<SharedOAuth>) -> Json<serde_json::Value> {
    Json(json!({
        "resource": oauth.resource(),
        "authorization_servers": [oauth.public_url],
        "scopes_supported": [SCOPE],
        "bearer_methods_supported": ["header"],
    }))
}

pub async fn authorization_server(State(oauth): State<SharedOAuth>) -> Json<serde_json::Value> {
    let base = &oauth.public_url;
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "registration_endpoint": format!("{base}/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": [SCOPE, "offline_access"],
    }))
}

#[derive(Debug, Deserialize)]
pub struct RegistrationRequest {
    #[serde(default)]
    redirect_uris: Vec<String>,
    client_name: Option<String>,
}

fn oauth_error(status: StatusCode, code: &str, description: &str) -> Response {
    let body = Json(json!({"error": code, "error_description": description}));
    (status, no_store(), body).into_response()
}

fn no_store() -> [(header::HeaderName, HeaderValue); 1] {
    [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))]
}

pub async fn register(
    State(oauth): State<SharedOAuth>,
    Json(request): Json<RegistrationRequest>,
) -> Response {
    let invalid = |code, description| oauth_error(StatusCode::BAD_REQUEST, code, description);
    if request.redirect_uris.is_empty() || request.redirect_uris.len() > MAX_REDIRECT_URIS {
        return invalid("invalid_redirect_uri", "invalid number of redirect_uris");
    }
    if !request
        .redirect_uris
        .iter()
        .all(|uri| oauth.redirect_is_allowed(uri))
    {
        return invalid("invalid_redirect_uri", "redirect_uri not allowed");
    }
    let name: String = request
        .client_name
        .unwrap_or_else(|| "Unnamed client".to_string())
        .chars()
        .take(100)
        .collect();
    let client = Client {
        redirect_uris: request.redirect_uris,
        name,
    };
    let Ok(encoded) = serde_json::to_vec(&client) else {
        return invalid("invalid_client_metadata", "invalid client");
    };
    let client_id = URL_SAFE_NO_PAD.encode(encoded);
    if client_id.chars().count() > MAX_CLIENT_ID_CHARS {
        return invalid("invalid_client_metadata", "client metadata too large");
    }
    let body = Json(json!({
        "client_id": client_id,
        "client_id_issued_at": now(),
        "client_name": client.name,
        "redirect_uris": client.redirect_uris,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    }));
    (StatusCode::CREATED, no_store(), body).into_response()
}

#[derive(Debug, Deserialize)]
pub struct AuthorizeParams {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    resource: Option<String>,
    scope: Option<String>,
    /// Only in the form submitted by the owner.
    secret: Option<String>,
}

pub(crate) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn html(status: StatusCode, body: String) -> Response {
    let page = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>wiki-llm-mcp</title><style>body{{font:16px system-ui;max-width:26em;\
         margin:3em auto;padding:0 1em}}input[type=password],button{{font:inherit;\
         width:100%;padding:.6em;margin-top:.6em;box-sizing:border-box}}\
         .note{{color:#a33}}</style></head><body>{body}</body></html>"
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::REFERRER_POLICY, "no-referrer"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
            ),
        ],
        page,
    )
        .into_response()
}

fn error_page(status: StatusCode, message: &str) -> Response {
    html(
        status,
        format!("<h1>Authorization failed</h1><p>{}</p>", escape(message)),
    )
}

fn consent_page(params: &AuthorizeParams, client: &Client, note: Option<&str>) -> String {
    let redirect = params.redirect_uri.as_deref().unwrap_or("");
    let parsed = Url::parse(redirect).ok();
    let host = parsed
        .as_ref()
        .and_then(|u| u.host_str())
        .unwrap_or("unknown");
    let loopback_warning = parsed.as_ref().is_some_and(is_loopback).then_some(
        "<p class=\"note\">This client runs on the computer used to approve this request. \
         Continue only if you started it yourself.</p>",
    );
    let field = |name: &str, value: Option<&str>| {
        value.map_or(String::new(), |v| {
            format!(
                "<input type=\"hidden\" name=\"{name}\" value=\"{}\">",
                escape(v)
            )
        })
    };
    let fields = [
        field("response_type", params.response_type.as_deref()),
        field("client_id", params.client_id.as_deref()),
        field("redirect_uri", params.redirect_uri.as_deref()),
        field("state", params.state.as_deref()),
        field("code_challenge", params.code_challenge.as_deref()),
        field(
            "code_challenge_method",
            params.code_challenge_method.as_deref(),
        ),
        field("resource", params.resource.as_deref()),
        field("scope", params.scope.as_deref()),
    ]
    .concat();
    let note = note.map_or(String::new(), |n| format!("<p class=\"note\">{n}</p>"));
    format!(
        "<h1>Connect your wiki</h1>\
         <p><strong>{name}</strong> asks to read and modify this vault.</p>\
         <p>You will be sent back to <strong>{host}</strong>.</p>{loopback}{note}\
         <form method=\"post\" action=\"/authorize\">{fields}\
         <label>Server secret<input type=\"password\" name=\"secret\" required autofocus \
         autocomplete=\"current-password\"></label>\
         <button type=\"submit\">Authorize</button></form>",
        name = escape(&client.name),
        host = escape(host),
        loopback = loopback_warning.unwrap_or(""),
    )
}

pub async fn authorize_page(
    State(oauth): State<SharedOAuth>,
    Query(params): Query<AuthorizeParams>,
) -> Response {
    match oauth.validate_authorization(&params) {
        Ok(client) => html(StatusCode::OK, consent_page(&params, &client, None)),
        Err(message) => error_page(StatusCode::BAD_REQUEST, message),
    }
}

pub async fn authorize_submit(
    State(oauth): State<SharedOAuth>,
    Form(params): Form<AuthorizeParams>,
) -> Response {
    let client = match oauth.validate_authorization(&params) {
        Ok(client) => client,
        Err(message) => return error_page(StatusCode::BAD_REQUEST, message),
    };
    if oauth.login_is_blocked() {
        return error_page(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed attempts. Try again in 15 minutes.",
        );
    }
    if !same_secret(params.secret.as_deref().unwrap_or(""), &oauth.secret) {
        oauth.record_login_failure();
        let page = consent_page(&params, &client, Some("Wrong secret."));
        return html(StatusCode::UNAUTHORIZED, page);
    }
    let Ok(code) = oauth.issue_code(&params) else {
        return error_page(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many pending authorizations.",
        );
    };
    let Ok(mut target) = Url::parse(params.redirect_uri.as_deref().unwrap_or("")) else {
        return error_page(StatusCode::BAD_REQUEST, "invalid redirect_uri");
    };
    {
        let mut query = target.query_pairs_mut();
        query.append_pair("code", &code);
        if let Some(state) = &params.state {
            query.append_pair("state", state);
        }
        query.append_pair("iss", &oauth.public_url);
    }
    Redirect::to(target.as_str()).into_response()
}

#[derive(Debug, Deserialize)]
pub struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    resource: Option<String>,
}

#[derive(Debug, Serialize)]
struct TokenResponse {
    access_token: String,
    token_type: &'static str,
    expires_in: u64,
    refresh_token: String,
    scope: &'static str,
}

#[derive(Debug)]
enum OAuthFailure {
    InvalidGrant,
    Server,
}

pub async fn token(State(oauth): State<SharedOAuth>, Form(form): Form<TokenForm>) -> Response {
    let result = match form.grant_type.as_deref() {
        Some("authorization_code") => oauth.exchange_code(&form),
        Some("refresh_token") => oauth.refresh(&form),
        _ => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                "unsupported grant_type",
            )
        }
    };
    match result {
        Ok(tokens) => (no_store(), Json(tokens)).into_response(),
        Err(OAuthFailure::InvalidGrant) => oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "invalid or expired grant",
        ),
        Err(OAuthFailure::Server) => oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "could not issue tokens",
        ),
    }
}

/// Bearer token in the header only: a token in a URL would end up in logs.
pub async fn require_token(
    State(oauth): State<SharedOAuth>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if token.is_some_and(|t| oauth.store.is_valid_access(t)) {
        return next.run(request).await;
    }
    let mut challenge = format!(
        "Bearer resource_metadata=\"{}\", scope=\"{SCOPE}\"",
        oauth.resource_metadata_url()
    );
    if token.is_some() {
        challenge.push_str(", error=\"invalid_token\"");
    }
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    if let Ok(value) = HeaderValue::from_str(&challenge) {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, value);
    }
    response
}
