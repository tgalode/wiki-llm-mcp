//! Upload links. The bytes of a file cannot travel through an MCP call, so
//! the LLM asks for a short-lived link instead and the owner sends the files
//! from a browser. A link only adds files, to one folder, and never replaces
//! anything. Remote mode only, and only when the owner allowed it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::apply::add_file;
use crate::oauth::{escape, random_token};
use crate::vault::{Vault, MAX_FILE_BYTES};

pub const DEFAULT_FOLDER: &str = "raw";
pub const LINK_LIFETIME: Duration = Duration::from_secs(30 * 60);
const MAX_OPEN_LINKS: usize = 32;
const MAX_FILES_PER_LINK: usize = 50;
const MAX_NAME_BYTES: usize = 255;

struct Link {
    folder: String,
    expires: Instant,
    files_left: usize,
}

struct Inner {
    vault: Vault,
    public_url: String,
    lifetime: Duration,
    /// Keyed by the hash of the token. Kept in memory only: a restart closes
    /// every link.
    links: Mutex<HashMap<[u8; 32], Link>>,
}

#[derive(Clone)]
pub struct Uploads(Arc<Inner>);

#[derive(Debug, Serialize)]
pub struct UploadLink {
    /// Page where the owner picks the files to send.
    pub url: String,
    /// Folder the files are added to.
    pub folder: String,
    pub expires_in_minutes: u64,
    pub max_file_bytes: u64,
}

/// Never prints the links: they are credentials.
impl std::fmt::Debug for Uploads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Uploads")
    }
}

fn key(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl Uploads {
    pub fn new(vault: Vault, public_url: &str) -> Self {
        Self::with_lifetime(vault, public_url, LINK_LIFETIME)
    }

    pub fn with_lifetime(vault: Vault, public_url: &str, lifetime: Duration) -> Self {
        Self(Arc::new(Inner {
            vault,
            public_url: public_url.trim_end_matches('/').to_string(),
            lifetime,
            links: Mutex::new(HashMap::new()),
        }))
    }

    fn links(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], Link>> {
        self.0.links.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Opens a link to add files to `folder`, `raw` when none is given.
    pub fn create_link(&self, folder: Option<&str>) -> Result<UploadLink> {
        let folder = Vault::normalize_any(folder.unwrap_or(DEFAULT_FOLDER))?;
        self.0.vault.check_writable(&folder)?;
        if self.0.vault.root().join(&folder).is_file() {
            bail!("{folder} is a file, not a folder");
        }
        let token = random_token()?;
        let now = Instant::now();
        let mut links = self.links();
        links.retain(|_, link| link.expires > now);
        if links.len() >= MAX_OPEN_LINKS {
            bail!("too many upload links are open: wait for one to expire");
        }
        links.insert(
            key(&token),
            Link {
                folder: folder.clone(),
                expires: now + self.0.lifetime,
                files_left: MAX_FILES_PER_LINK,
            },
        );
        Ok(UploadLink {
            url: format!("{}/upload/{token}", self.0.public_url),
            folder,
            expires_in_minutes: self.0.lifetime.as_secs() / 60,
            max_file_bytes: MAX_FILE_BYTES,
        })
    }

    /// The folder of a link that is still open.
    fn folder(&self, token: &str) -> Option<String> {
        let links = self.links();
        let link = links.get(&key(token))?;
        (link.expires > Instant::now()).then(|| link.folder.clone())
    }

    /// Same, counting one more file against the link.
    fn take_slot(&self, token: &str) -> Result<String, Refusal> {
        let mut links = self.links();
        let link = links
            .get_mut(&key(token))
            .filter(|link| link.expires > Instant::now())
            .ok_or(Refusal::NoSuchLink)?;
        if link.files_left == 0 {
            return Err(Refusal::Invalid(
                "this link has received its share of files: ask for a new one".into(),
            ));
        }
        link.files_left -= 1;
        Ok(link.folder.clone())
    }
}

enum Refusal {
    NoSuchLink,
    Invalid(String),
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Refusal::NoSuchLink => (
                StatusCode::NOT_FOUND,
                "this upload link is not valid any more".to_string(),
            ),
            Refusal::Invalid(message) => (StatusCode::BAD_REQUEST, message),
        };
        (status, no_store(), Json(json!({ "error": message }))).into_response()
    }
}

fn no_store() -> [(header::HeaderName, &'static str); 1] {
    [(header::CACHE_CONTROL, "no-store")]
}

/// A file name as the owner typed it: one visible path component.
fn check_name(name: &str) -> Result<&str, Refusal> {
    let name = name.trim();
    let is_plain = !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control);
    if !is_plain {
        return Err(Refusal::Invalid("invalid file name".into()));
    }
    Ok(name)
}

/// `PUT /upload/{token}/{name}`: the body is the file itself.
pub async fn receive(
    State(uploads): State<Uploads>,
    Path((token, name)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let name = match check_name(&name) {
        Ok(name) => name,
        Err(refusal) => return refusal.into_response(),
    };
    let folder = match uploads.take_slot(&token) {
        Ok(folder) => folder,
        Err(refusal) => return refusal.into_response(),
    };
    let path = format!("{folder}/{name}");
    let vault = uploads.0.vault.clone();
    let bytes = body.to_vec();
    // Git and the filesystem run off the async runtime.
    let written = tokio::task::spawn_blocking(move || add_file(&vault, &path, bytes)).await;
    match written {
        Ok(Ok(report)) => (StatusCode::CREATED, no_store(), Json(report)).into_response(),
        Ok(Err(error)) => Refusal::Invalid(error.to_string()).into_response(),
        Err(error) => Refusal::Invalid(error.to_string()).into_response(),
    }
}

/// `GET /upload/{token}`: the page the owner sends files from.
pub async fn page(State(uploads): State<Uploads>, Path(token): Path<String>) -> Response {
    let Some(folder) = uploads.folder(&token) else {
        return html(
            StatusCode::NOT_FOUND,
            "",
            "<h1>Link not valid any more</h1><p>Upload links are short-lived: ask Claude for \
             a new one.</p>"
                .to_string(),
        );
    };
    let Ok(nonce) = random_token() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let body = format!(
        "<h1>Add files to the vault</h1>\
         <p>They go to <code>{folder}/</code>, {megabytes} MB at most each. A file that \
         already exists is never replaced: rename yours before sending it.</p>\
         <input type=\"file\" id=\"pick\" multiple>\
         <ul id=\"list\"></ul>\
         <button id=\"send\" disabled>Upload</button>\
         <script nonce=\"{nonce}\">const MAX_BYTES={max_bytes};{SCRIPT}</script>",
        folder = escape(&folder),
        megabytes = MAX_FILE_BYTES / (1024 * 1024),
        max_bytes = MAX_FILE_BYTES,
    );
    html(StatusCode::OK, &nonce, body)
}

/// One row per chosen file, with a name the owner can change before sending;
/// files go one at a time, since each is a commit.
const SCRIPT: &str = r#"
const pick = document.getElementById('pick');
const list = document.getElementById('list');
const send = document.getElementById('send');
let rows = [];
pick.addEventListener('change', () => {
  list.textContent = '';
  rows = [...pick.files].map((file) => {
    const item = document.createElement('li');
    const name = document.createElement('input');
    name.type = 'text';
    name.value = file.name;
    const status = document.createElement('span');
    item.append(name, status);
    list.append(item);
    return { file, name, status, done: false };
  });
  send.disabled = rows.length === 0;
});
send.addEventListener('click', async () => {
  send.disabled = true;
  pick.disabled = true;
  for (const row of rows) {
    if (row.done) continue;
    if (row.file.size > MAX_BYTES) {
      row.status.textContent = 'Too large';
      continue;
    }
    row.status.textContent = 'Sending...';
    try {
      const url = location.pathname + '/' + encodeURIComponent(row.name.value.trim());
      const response = await fetch(url, { method: 'PUT', body: row.file });
      const answer = await response.json().catch(() => ({}));
      if (response.ok) {
        row.done = true;
        row.name.disabled = true;
        row.status.textContent = 'Added';
      } else {
        row.status.textContent = answer.error || ('Refused (' + response.status + ')');
      }
    } catch (error) {
      row.status.textContent = 'Network error';
    }
  }
  pick.disabled = false;
  send.disabled = rows.every((row) => row.done);
});
"#;

fn html(status: StatusCode, nonce: &str, body: String) -> Response {
    let page = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>wiki-llm-mcp</title><style>body{{font:16px system-ui;max-width:30em;\
         margin:3em auto;padding:0 1em}}input,button{{font:inherit;width:100%;padding:.6em;\
         margin-top:.6em;box-sizing:border-box}}ul{{list-style:none;padding:0}}\
         li span{{display:block;color:#666;font-size:.9em}}</style></head>\
         <body>{body}</body></html>"
    );
    let policy = format!(
        "default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-{nonce}'; \
         connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::X_FRAME_OPTIONS, "DENY".to_string()),
            (header::REFERRER_POLICY, "no-referrer".to_string()),
            (header::CONTENT_SECURITY_POLICY, policy),
        ],
        page,
    )
        .into_response()
}
