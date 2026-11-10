//! The whole OAuth flow and authenticated MCP calls, through the real router.

use std::fs;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::{tempdir, TempDir};
use tower::ServiceExt;
use wiki_llm_mcp::init::init;
use wiki_llm_mcp::oauth::{normalize_public_url, Lifetimes, OAuthConfig};
use wiki_llm_mcp::remote::{build_app, revoke_all, state_dir};
use wiki_llm_mcp::vault::{canonical, Vault};

const BASE: &str = "https://wiki.example.com";
const HOST: &str = "wiki.example.com";
const SECRET: &str = "correct horse battery staple";
const CALLBACK: &str = "https://claude.ai/api/mcp/auth_callback";
const VERIFIER: &str = "a-long-enough-verifier-for-pkce-0123456789-abcdef";

struct Harness {
    dir: TempDir,
    app: Router,
}

fn harness_with(lifetimes: Lifetimes) -> Harness {
    let dir = tempdir().unwrap();
    init(dir.path()).unwrap();
    fs::write(dir.path().join("wiki/page.md"), "# Page\nremote content").unwrap();
    let vault = Vault::open(dir.path()).unwrap();
    let config = OAuthConfig {
        public_url: BASE.to_string(),
        secret: SECRET.to_string(),
        state_dir: state_dir(&vault).unwrap(),
        extra_redirects: vec![],
        lifetimes,
    };
    let app = build_app(vault, config).unwrap();
    Harness { dir, app }
}

fn harness() -> Harness {
    harness_with(Lifetimes::default())
}

fn challenge() -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()))
}

fn encode(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

async fn body_text(response: Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

impl Harness {
    async fn send(&self, request: Request<Body>) -> Response {
        self.app.clone().oneshot(request).await.unwrap()
    }

    async fn get(&self, path: &str) -> Response {
        let request = Request::get(path)
            .header(header::HOST, HOST)
            .body(Body::empty())
            .unwrap();
        self.send(request).await
    }

    async fn post_form(&self, path: &str, pairs: &[(&str, &str)]) -> Response {
        let request = Request::post(path)
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(encode(pairs)))
            .unwrap();
        self.send(request).await
    }

    async fn register_redirect(&self, redirect: &str) -> Response {
        let body = json!({"redirect_uris":[redirect],"client_name":"Claude <b>"});
        let request = Request::post("/register")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        self.send(request).await
    }

    async fn client(&self) -> String {
        let response = self.register_redirect(CALLBACK).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: Value = serde_json::from_str(&body_text(response).await).unwrap();
        body["client_id"].as_str().unwrap().to_string()
    }

    async fn submit_secret(&self, client: &str, secret: &str) -> Response {
        let challenge = challenge();
        self.post_form(
            "/authorize",
            &[
                ("response_type", "code"),
                ("client_id", client),
                ("redirect_uri", CALLBACK),
                ("state", "xyz"),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("secret", secret),
            ],
        )
        .await
    }

    async fn code(&self, client: &str) -> String {
        let response = self.submit_secret(client, SECRET).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response.headers()[header::LOCATION].to_str().unwrap();
        let url = url::Url::parse(location).unwrap();
        assert!(location.starts_with(CALLBACK), "{location}");
        let get = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        assert_eq!(get("state").as_deref(), Some("xyz"));
        assert_eq!(get("iss").as_deref(), Some(BASE));
        get("code").unwrap()
    }

    async fn exchange(&self, client: &str, code: &str, verifier: &str) -> Response {
        self.post_form(
            "/token",
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", CALLBACK),
                ("client_id", client),
                ("code_verifier", verifier),
            ],
        )
        .await
    }

    async fn tokens(&self, client: &str) -> Value {
        let code = self.code(client).await;
        let response = self.exchange(client, &code, VERIFIER).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        serde_json::from_str(&body_text(response).await).unwrap()
    }

    async fn refresh(&self, client: &str, refresh: &str) -> Response {
        self.post_form(
            "/token",
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
                ("client_id", client),
            ],
        )
        .await
    }

    async fn mcp(&self, token: Option<&str>, message: Value) -> Response {
        let mut request = Request::post("/mcp")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        self.send(request.body(Body::from(message.to_string())).unwrap())
            .await
    }
}

fn tools_list() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})
}

#[tokio::test]
async fn an_anonymous_call_points_to_the_discovery_document() {
    let h = harness();
    let response = h.mcp(None, tools_list()).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let challenge = response.headers()[header::WWW_AUTHENTICATE]
        .to_str()
        .unwrap();
    assert!(challenge.contains(&format!(
        "resource_metadata=\"{BASE}/.well-known/oauth-protected-resource\""
    )));
}

#[tokio::test]
async fn discovery_documents_describe_the_server() {
    let h = harness();
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let body: Value = serde_json::from_str(&body_text(h.get(path).await).await).unwrap();
        assert_eq!(body["resource"], format!("{BASE}/mcp"));
        assert_eq!(body["authorization_servers"][0], BASE);
    }
    let response = h.get("/.well-known/oauth-authorization-server").await;
    let body: Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(body["issuer"], BASE);
    assert_eq!(body["registration_endpoint"], format!("{BASE}/register"));
    assert_eq!(body["code_challenge_methods_supported"][0], "S256");
    assert_eq!(body["token_endpoint_auth_methods_supported"][0], "none");
    assert!(body["scopes_supported"]
        .to_string()
        .contains("offline_access"));
}

#[tokio::test]
async fn full_flow_gives_access_to_the_vault_tools() {
    let h = harness();
    let client = h.client().await;
    let tokens = h.tokens(&client).await;
    assert_eq!(tokens["token_type"], "Bearer");
    let access = tokens["access_token"].as_str().unwrap();

    let response = h.mcp(Some(access), tools_list()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_text(response).await;
    assert!(
        text.contains("apply_changes") && !text.contains("delete_"),
        "{text}"
    );

    let read = json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"read_page","arguments":{"path":"wiki/page.md"}}});
    let response = h.mcp(Some(access), read).await;
    assert!(body_text(response).await.contains("remote content"));
}

#[tokio::test]
async fn the_consent_page_shows_who_asks_and_escapes_it() {
    let h = harness();
    let client = h.client().await;
    let path = format!(
        "/authorize?{}",
        encode(&[
            ("response_type", "code"),
            ("client_id", &client),
            ("redirect_uri", CALLBACK),
            ("state", "\"><script>x</script>"),
            ("code_challenge", &challenge()),
            ("code_challenge_method", "S256"),
        ])
    );
    let response = h.get(&path).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap()
        .contains("frame-ancestors 'none'"));
    let page = body_text(response).await;
    assert!(page.contains("claude.ai") && page.contains("Claude &lt;b&gt;"));
    assert!(!page.contains("<script>") && !page.contains("Claude <b>"));
}

#[tokio::test]
async fn registration_refuses_unknown_callbacks() {
    let h = harness();
    for uri in [
        "https://evil.example/cb",
        "http://evil.example/cb",
        "not a url",
    ] {
        let response = h.register_redirect(uri).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
    }
    let response = h.register_redirect("http://localhost:3118/callback").await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn a_forged_client_cannot_use_another_callback() {
    let h = harness();
    let forged = URL_SAFE_NO_PAD
        .encode(json!({"redirect_uris":["https://evil.example/cb"],"name":"x"}).to_string());
    let challenge = challenge();
    let response = h
        .post_form(
            "/authorize",
            &[
                ("response_type", "code"),
                ("client_id", &forged),
                ("redirect_uri", "https://evil.example/cb"),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("secret", SECRET),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response.headers().get(header::LOCATION).is_none());
}

#[tokio::test]
async fn loopback_clients_may_use_any_port_but_not_any_path() {
    let h = harness();
    let response = h.register_redirect("http://localhost/callback").await;
    let body: Value = serde_json::from_str(&body_text(response).await).unwrap();
    let client = body["client_id"].as_str().unwrap().to_string();
    let attempt = |redirect: &'static str| {
        let path = format!(
            "/authorize?{}",
            encode(&[
                ("response_type", "code"),
                ("client_id", &client),
                ("redirect_uri", redirect),
                ("code_challenge", &challenge()),
                ("code_challenge_method", "S256"),
            ])
        );
        path
    };
    assert_eq!(
        h.get(&attempt("http://localhost:3118/callback"))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        h.get(&attempt("http://localhost:3118/other"))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn pkce_is_mandatory() {
    let h = harness();
    let client = h.client().await;
    for (method, value) in [("plain", "x".repeat(43)), ("S256", "short".to_string())] {
        let path = format!(
            "/authorize?{}",
            encode(&[
                ("response_type", "code"),
                ("client_id", &client),
                ("redirect_uri", CALLBACK),
                ("code_challenge", &value),
                ("code_challenge_method", method),
            ])
        );
        assert_eq!(h.get(&path).await.status(), StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn a_wrong_secret_issues_nothing() {
    let h = harness();
    let client = h.client().await;
    let response = h.submit_secret(&client, "wrong").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().get(header::LOCATION).is_none());
}

#[tokio::test]
async fn repeated_failures_block_login_even_with_the_right_secret() {
    let h = harness();
    let client = h.client().await;
    for _ in 0..10 {
        h.submit_secret(&client, "wrong").await;
    }
    let response = h.submit_secret(&client, SECRET).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn a_wrong_verifier_fails_and_burns_the_code() {
    let h = harness();
    let client = h.client().await;
    let code = h.code(&client).await;
    let wrong = "b".repeat(43);
    assert_eq!(
        h.exchange(&client, &code, &wrong).await.status(),
        StatusCode::BAD_REQUEST
    );
    let response = h.exchange(&client, &code, VERIFIER).await;
    assert!(body_text(response).await.contains("invalid_grant"));
}

#[tokio::test]
async fn a_code_works_only_once() {
    let h = harness();
    let client = h.client().await;
    let code = h.code(&client).await;
    assert_eq!(
        h.exchange(&client, &code, VERIFIER).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        h.exchange(&client, &code, VERIFIER).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn a_code_is_bound_to_its_client() {
    let h = harness();
    let client = h.client().await;
    let other = {
        let response = h.register_redirect("http://localhost/cb").await;
        let body: Value = serde_json::from_str(&body_text(response).await).unwrap();
        body["client_id"].as_str().unwrap().to_string()
    };
    let code = h.code(&client).await;
    assert_eq!(
        h.exchange(&other, &code, VERIFIER).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn an_expired_code_is_refused() {
    let h = harness_with(Lifetimes {
        code_secs: 0,
        ..Lifetimes::default()
    });
    let client = h.client().await;
    let code = h.code(&client).await;
    assert_eq!(
        h.exchange(&client, &code, VERIFIER).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn an_expired_access_token_is_refused_but_can_be_refreshed() {
    let h = harness_with(Lifetimes {
        access_secs: 0,
        ..Lifetimes::default()
    });
    let client = h.client().await;
    let tokens = h.tokens(&client).await;
    let response = h
        .mcp(Some(tokens["access_token"].as_str().unwrap()), tools_list())
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers()[header::WWW_AUTHENTICATE]
        .to_str()
        .unwrap()
        .contains("invalid_token"));
    let refreshed = h
        .refresh(&client, tokens["refresh_token"].as_str().unwrap())
        .await;
    assert_eq!(refreshed.status(), StatusCode::OK);
}

#[tokio::test]
async fn refresh_tokens_rotate_and_replay_is_refused() {
    let h = harness();
    let client = h.client().await;
    let first = h.tokens(&client).await;
    let old = first["refresh_token"].as_str().unwrap();

    let response = h.refresh(&client, old).await;
    assert_eq!(response.status(), StatusCode::OK);
    let second: Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_ne!(second["refresh_token"], first["refresh_token"]);
    let new_access = second["access_token"].as_str().unwrap();
    assert_eq!(
        h.mcp(Some(new_access), tools_list()).await.status(),
        StatusCode::OK
    );

    let replay = h.refresh(&client, old).await;
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(replay).await.contains("invalid_grant"));
}

#[tokio::test]
async fn a_refresh_token_is_bound_to_its_client() {
    let h = harness();
    let client = h.client().await;
    let tokens = h.tokens(&client).await;
    let refresh = tokens["refresh_token"].as_str().unwrap();
    assert_eq!(
        h.refresh("someone-else", refresh).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(h.refresh(&client, refresh).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn revoking_disconnects_clients_at_once() {
    let h = harness();
    let client = h.client().await;
    let tokens = h.tokens(&client).await;
    let access = tokens["access_token"].as_str().unwrap();
    assert_eq!(
        h.mcp(Some(access), tools_list()).await.status(),
        StatusCode::OK
    );

    let vault = Vault::open(h.dir.path()).unwrap();
    assert_eq!(revoke_all(&vault).unwrap(), 1);

    assert_eq!(
        h.mcp(Some(access), tools_list()).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        h.refresh(&client, tokens["refresh_token"].as_str().unwrap())
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn tokens_are_stored_hashed_private_and_out_of_git() {
    let h = harness();
    let client = h.client().await;
    let tokens = h.tokens(&client).await;

    let vault = Vault::open(h.dir.path()).unwrap();
    let dir = state_dir(&vault).unwrap();
    let file = dir.join("tokens.json");
    let stored = fs::read_to_string(&file).unwrap();
    for field in ["access_token", "refresh_token"] {
        assert!(!stored.contains(tokens[field].as_str().unwrap()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert!(dir.starts_with(canonical(h.dir.path()).unwrap().join(".git")));
}

#[tokio::test]
async fn a_token_in_the_url_is_ignored() {
    let h = harness();
    let client = h.client().await;
    let tokens = h.tokens(&client).await;
    let request = Request::post(format!(
        "/mcp?access_token={}",
        tokens["access_token"].as_str().unwrap()
    ))
    .header(header::HOST, HOST)
    .header(header::CONTENT_TYPE, "application/json")
    .header(header::ACCEPT, "application/json, text/event-stream")
    .body(Body::from(tools_list().to_string()))
    .unwrap();
    assert_eq!(h.send(request).await.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_unexpected_host_is_refused() {
    let h = harness();
    let client = h.client().await;
    let tokens = h.tokens(&client).await;
    let request = Request::post("/mcp")
        .header(header::HOST, "attacker.example")
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", tokens["access_token"].as_str().unwrap()),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(tools_list().to_string()))
        .unwrap();
    assert_eq!(h.send(request).await.status(), StatusCode::FORBIDDEN);
}

#[test]
fn startup_refuses_weak_or_unsafe_settings() {
    let dir = tempdir().unwrap();
    init(dir.path()).unwrap();
    let vault = Vault::open(dir.path()).unwrap();
    let config = |url: &str, secret: &str| OAuthConfig {
        public_url: url.to_string(),
        secret: secret.to_string(),
        state_dir: state_dir(&vault).unwrap(),
        extra_redirects: vec![],
        lifetimes: Lifetimes::default(),
    };
    assert!(build_app(vault.clone(), config(BASE, "too short")).is_err());
    for url in [
        "http://wiki.example.com",
        "https://x.example/path",
        "nonsense",
    ] {
        assert!(
            build_app(vault.clone(), config(url, SECRET)).is_err(),
            "{url}"
        );
    }
    assert!(normalize_public_url("http://127.0.0.1:8787").is_ok());
    assert_eq!(
        normalize_public_url("https://a.example/").unwrap(),
        "https://a.example"
    );
}
