//! Auth service tests: dev mode, and the WorkOS flows (headless paste-code exchange,
//! loopback callback, refresh rotation + revocation, org onboarding) against a stub
//! edge HTTP server on a plain tokio TcpListener.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use jolt_api::methods;
use jolt_engine::{
    Auth, AuthConfig, AuthState, Engine, EngineConfig, EngineSupervisor, HarnessId, InstanceLock,
};
use jolt_rpc::connect_ws;

// ---------------------------------------------------------------------------
// Fake JWTs
// ---------------------------------------------------------------------------

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

/// An unsigned JWT with the claims the engine reads (`exp`/`iat` for TTL, `org_id`).
fn fake_jwt(ttl_secs: i64, org_id: Option<&str>) -> String {
    let mut claims = serde_json::json!({ "sub": "user_1", "iat": 1_000, "exp": 1_000 + ttl_secs });
    if let Some(org) = org_id {
        claims["org_id"] = serde_json::json!(org);
    }
    format!("e30.{}.sig", base64url(claims.to_string().as_bytes()))
}

// ---------------------------------------------------------------------------
// Stub edge server
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StubState {
    exchanges: AtomicUsize,
    refreshes: AtomicUsize,
    /// Refresh tokens seen by /auth/refresh, in order.
    refresh_tokens: Mutex<Vec<String>>,
    /// Organization scopes requested by /auth/refresh, in order.
    refresh_orgs: Mutex<Vec<Option<String>>>,
    /// TTL (seconds) for minted access tokens.
    token_ttl: AtomicUsize,
    /// org_id claim for exchange-minted tokens ("" = none).
    exchange_org: Mutex<String>,
    /// Org WorkOS chooses when a refresh omits `organizationId`.
    default_refresh_org: Mutex<Option<String>>,
    /// Test-only simulation of WorkOS returning a scope other than requested.
    refresh_org_override: Mutex<Option<String>>,
    /// Active organization memberships returned by /auth/orgs.
    orgs: Mutex<Vec<serde_json::Value>>,
    /// Names requested through organization creation.
    created_org_names: Mutex<Vec<String>>,
}

struct StubEdge {
    port: u16,
    state: Arc<StubState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for StubEdge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl StubEdge {
    async fn start() -> StubEdge {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
        let port = listener.local_addr().expect("addr").port();
        let state = Arc::new(StubState::default());
        state.token_ttl.store(3600, Ordering::SeqCst);
        state.orgs.lock().expect("lock").push(serde_json::json!({
            "id": "om_1", "organizationId": "org_1", "name": "Personal"
        }));
        let handler_state = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(handle(stream, handler_state.clone()));
            }
        });
        StubEdge { port, state, task }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<(String, String, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next()?.to_string();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    Some((method, target, String::from_utf8_lossy(&body).into_owned()))
}

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn handle(mut stream: tokio::net::TcpStream, state: Arc<StubState>) {
    let Some((method, target, body)) = read_request(&mut stream).await else {
        return;
    };
    let path = target.split('?').next().unwrap_or("");
    let ttl = state.token_ttl.load(Ordering::SeqCst) as i64;
    match (method.as_str(), path) {
        ("GET", "/health") => {
            respond(&mut stream, "200 OK", r#"{"ok":true,"auth":"workos"}"#).await;
        }
        ("POST", "/auth/exchange") => {
            let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            if parsed.get("code").and_then(|v| v.as_str()).is_none() {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    r#"{"error":"missing code"}"#,
                )
                .await;
                return;
            }
            let n = state.exchanges.fetch_add(1, Ordering::SeqCst) + 1;
            let org = state.exchange_org.lock().expect("lock").clone();
            let token = fake_jwt(ttl, (!org.is_empty()).then_some(org.as_str()));
            let response = serde_json::json!({
                "user": { "id": "user_1", "email": "w@example.com",
                          "firstName": "Wing", "lastName": "Test" },
                "accessToken": token,
                "refreshToken": format!("refresh-{n}"),
            });
            respond(&mut stream, "200 OK", &response.to_string()).await;
        }
        ("POST", "/auth/refresh") => {
            let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let refresh_token = parsed
                .get("refreshToken")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            state
                .refresh_tokens
                .lock()
                .expect("lock")
                .push(refresh_token.to_string());
            let requested_org = parsed
                .get("organizationId")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            state
                .refresh_orgs
                .lock()
                .expect("lock")
                .push(requested_org.clone());
            if refresh_token == "dead" {
                respond(&mut stream, "401 Unauthorized", r#"{"error":"revoked"}"#).await;
                return;
            }
            let n = state.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
            let default_org = state.default_refresh_org.lock().expect("lock").clone();
            let override_org = state.refresh_org_override.lock().expect("lock").clone();
            let org = override_org.or(requested_org).or(default_org);
            let response = serde_json::json!({
                "accessToken": fake_jwt(ttl, org.as_deref()),
                "refreshToken": format!("rotated-{n}"),
            });
            respond(&mut stream, "200 OK", &response.to_string()).await;
        }
        ("GET", "/auth/orgs") => {
            let orgs = state.orgs.lock().expect("lock").clone();
            respond(
                &mut stream,
                "200 OK",
                &serde_json::json!({ "orgs": orgs }).to_string(),
            )
            .await;
        }
        ("POST", "/auth/orgs") => {
            let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            if let Some(name) = parsed.get("name").and_then(|value| value.as_str()) {
                state
                    .created_org_names
                    .lock()
                    .expect("lock")
                    .push(name.to_string());
            }
            respond(&mut stream, "200 OK", r#"{"organizationId":"org_new"}"#).await;
        }
        _ => respond(&mut stream, "404 Not Found", r#"{"error":"not_found"}"#).await,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn workos_config(edge_url: &str, data_dir: &std::path::Path) -> AuthConfig {
    let mut config = AuthConfig::new(edge_url, data_dir);
    config.workos_client_id = Some("client_test".into());
    config.workos_api_base = "https://authkit.example".into();
    config
}

fn query_param(url: &str, key: &str) -> Option<String> {
    url.split_once('?')?
        .1
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
}

async fn wait_for<T: Clone + PartialEq>(
    rx: &mut tokio::sync::watch::Receiver<T>,
    check: impl Fn(&T) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if check(&rx.borrow()) {
                return;
            }
            rx.changed().await.expect("state channel open");
        }
    })
    .await
    .expect("state reached in time");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dev_mode_is_signed_in_with_configured_bearer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = AuthConfig::new("http://127.0.0.1:1", dir.path());
    config.dev_user_id = "wing-dev".into();
    let auth = Auth::new(config);
    assert!(!auth.workos_enabled());
    assert!(matches!(auth.state(), AuthState::SignedIn { user, .. } if user.id == "wing-dev"));
    assert_eq!(auth.access_token().await.as_deref(), Some("wing-dev"));
    // Dev sign-in mirrors the TS service: a no-op URL, CompleteSignIn accepted.
    assert_eq!(auth.start_sign_in().await.expect("dev sign-in"), "");
    auth.complete_sign_in("whatever")
        .await
        .expect("dev complete is a no-op");
}

#[tokio::test]
async fn headless_flow_exchanges_pasted_code_and_gates_on_org() {
    let edge = StubEdge::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Auth::new(workos_config(&edge.url(), dir.path()));
    assert!(auth.workos_enabled());
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert_eq!(auth.access_token().await, None, "signed out: no token");

    let url = auth.start_headless_sign_in();
    assert!(url.starts_with("https://authkit.example/user_management/authorize?"));
    assert_eq!(
        query_param(&url, "client_id").as_deref(),
        Some("client_test")
    );
    let redirect = query_param(&url, "redirect_uri").expect("redirect");
    assert!(
        redirect.contains("auth%2Fcli%2Fcallback"),
        "hosted paste-code page: {redirect}"
    );
    let state = query_param(&url, "state").expect("state param");

    // A code minted for someone else's flow (unknown state) is rejected — CSRF check.
    assert!(auth.complete_sign_in("bogus-state.code123").await.is_err());

    // The real paste: `state.code`. The exchange-minted token carries no org claim, so
    // the session lands in NeedsOrganization (the org gate).
    auth.complete_sign_in(&format!("{state}.code123"))
        .await
        .expect("paste-code sign-in");
    assert_eq!(edge.state.exchanges.load(Ordering::SeqCst), 1);
    assert!(
        matches!(auth.state(), AuthState::NeedsOrganization { user } if user.email == "w@example.com")
    );

    // Session persisted 0600 with the exchange's refresh token.
    let session_file = dir.path().join("session.json");
    let raw = std::fs::read_to_string(&session_file).expect("session persisted");
    assert!(raw.contains("refresh-1"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&session_file)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "session file must be private");
    }

    // Setup adopts the sole membership and scopes the session without asking.
    auth.ensure_personal_org().await.expect("automatic setup");
    assert!(
        matches!(auth.state(), AuthState::SignedIn { org_id: Some(org), .. } if org == "org_1")
    );
    assert_eq!(
        edge.state
            .refresh_tokens
            .lock()
            .expect("lock")
            .first()
            .map(String::as_str),
        Some("refresh-1"),
        "org refresh presents the stored refresh token"
    );
    // Rotation persisted.
    let raw = std::fs::read_to_string(&session_file).expect("session persisted");
    assert!(
        raw.contains("rotated-1"),
        "rotated refresh token stored: {raw}"
    );

    // Sign-out clears state and removes the persisted session.
    auth.sign_out();
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert!(!session_file.exists());
}

#[tokio::test]
async fn sign_out_fences_an_in_flight_code_exchange() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept exchange");
        let _ = read_request(&mut stream).await.expect("read exchange");
        reached_tx.send(()).ok();
        release_rx.await.ok();
        let body = serde_json::json!({
            "user": { "id": "user_1", "email": "w@example.com" },
            "accessToken": fake_jwt(3600, Some("org_1")),
            "refreshToken": "late-refresh",
        });
        respond(&mut stream, "200 OK", &body.to_string()).await;
    });
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Auth::new(workos_config(
        &format!("http://127.0.0.1:{port}"),
        dir.path(),
    ));
    let url = auth.start_headless_sign_in();
    let state = query_param(&url, "state").expect("state");
    let completing = {
        let auth = auth.clone();
        tokio::spawn(async move { auth.complete_sign_in(&format!("{state}.code")).await })
    };

    reached_rx.await.expect("exchange reached edge");
    auth.sign_out();
    release_tx.send(()).ok();

    let error = completing
        .await
        .expect("completion task")
        .expect_err("canceled exchange must not publish credentials");
    assert!(error.to_string().contains("canceled"));
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert!(!dir.path().join("session.json").exists());
    server.await.expect("server task");
}

#[tokio::test]
async fn sign_out_fences_an_in_flight_refresh() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept refresh");
        let _ = read_request(&mut stream).await.expect("read refresh");
        reached_tx.send(()).ok();
        release_rx.await.ok();
        let body = serde_json::json!({
            "accessToken": fake_jwt(3600, Some("org_1")),
            "refreshToken": "late-rotation",
        });
        respond(&mut stream, "200 OK", &body.to_string()).await;
    });
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("session.json"),
        r#"{"refreshToken":"refresh-1","user":{"id":"user_1","email":"w@example.com"},"orgId":"org_1"}"#,
    )
    .expect("seed session");
    let auth = Auth::new(workos_config(
        &format!("http://127.0.0.1:{port}"),
        dir.path(),
    ));
    let refreshing = {
        let auth = auth.clone();
        tokio::spawn(async move { auth.access_token().await })
    };

    reached_rx.await.expect("refresh reached edge");
    auth.sign_out();
    release_tx.send(()).ok();

    assert_eq!(refreshing.await.expect("refresh task"), None);
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert_eq!(auth.access_token().await, None);
    assert!(!dir.path().join("session.json").exists());
    server.await.expect("server task");
}

#[tokio::test]
async fn transient_refresh_failure_enters_cooldown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("session.json"),
        r#"{"refreshToken":"refresh-1","user":{"id":"user_1","email":"w@example.com"},"orgId":"org_1"}"#,
    )
    .expect("seed session");
    let auth = Auth::new(workos_config(
        &format!("http://127.0.0.1:{port}"),
        dir.path(),
    ));
    let first = {
        let auth = auth.clone();
        tokio::spawn(async move { auth.access_token().await })
    };
    let (mut stream, _) = listener.accept().await.expect("first refresh");
    let _ = read_request(&mut stream).await.expect("read refresh");
    respond(
        &mut stream,
        "503 Service Unavailable",
        r#"{"error":"offline"}"#,
    )
    .await;
    assert_eq!(first.await.expect("refresh task"), None);

    assert_eq!(auth.access_token().await, None);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), listener.accept())
            .await
            .is_err(),
        "cooldown must suppress immediate retry"
    );
    assert!(
        auth.state().is_signed_in(),
        "transient failures keep the session"
    );
}

#[tokio::test]
async fn first_sign_in_creates_a_personal_org_automatically() {
    let edge = StubEdge::start().await;
    edge.state.orgs.lock().expect("lock").clear();
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Auth::new(workos_config(&edge.url(), dir.path()));
    let url = auth.start_headless_sign_in();
    let state = query_param(&url, "state").expect("state");
    auth.complete_sign_in(&format!("{state}.codeX"))
        .await
        .expect("sign in");

    auth.ensure_personal_org().await.expect("automatic setup");

    assert_eq!(
        edge.state
            .created_org_names
            .lock()
            .expect("lock")
            .as_slice(),
        ["Personal"]
    );
    assert!(matches!(
        auth.state(),
        AuthState::SignedIn { org_id: Some(org), .. } if org == "org_new"
    ));
}

#[tokio::test]
async fn automatic_setup_rejects_ambiguous_multiple_orgs() {
    let edge = StubEdge::start().await;
    edge.state
        .orgs
        .lock()
        .expect("lock")
        .push(serde_json::json!({
            "id": "om_2", "organizationId": "org_2", "name": "Old"
        }));
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Auth::new(workos_config(&edge.url(), dir.path()));
    let url = auth.start_headless_sign_in();
    let state = query_param(&url, "state").expect("state");
    auth.complete_sign_in(&format!("{state}.codeX"))
        .await
        .expect("sign in");

    let err = auth
        .ensure_personal_org()
        .await
        .expect_err("multiple orgs must not be selected arbitrarily");
    assert!(err.to_string().contains("multiple organizations"));
    assert!(matches!(auth.state(), AuthState::NeedsOrganization { .. }));
}

#[tokio::test]
async fn short_lived_tokens_refresh_on_demand() {
    let edge = StubEdge::start().await;
    // Tokens live 20s < the 30s slack → every access_token() call refreshes.
    edge.state.token_ttl.store(20, Ordering::SeqCst);
    *edge.state.exchange_org.lock().expect("lock") = "org_1".into();
    *edge.state.default_refresh_org.lock().expect("lock") = Some("org_2".into());
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Auth::new(workos_config(&edge.url(), dir.path()));

    let url = auth.start_headless_sign_in();
    let state = query_param(&url, "state").expect("state");
    auth.complete_sign_in(&format!("{state}.codeX"))
        .await
        .expect("sign in");
    assert!(auth.state().is_signed_in());

    let first = auth.access_token().await.expect("token after refresh");
    assert_eq!(
        edge.state.refreshes.load(Ordering::SeqCst),
        1,
        "stale exchange token refreshed"
    );
    let second = auth.access_token().await.expect("token again");
    assert_eq!(
        edge.state.refreshes.load(Ordering::SeqCst),
        2,
        "still under slack → refreshed"
    );
    assert_eq!(first, second, "same claims → same fake token bytes");
    assert!(
        matches!(auth.state(), AuthState::SignedIn { org_id: Some(org), .. } if org == "org_1"),
        "routine refresh must preserve the selected workspace"
    );
    // Rotated refresh tokens are chained: refresh N presents rotation N-1's token.
    let seen = edge.state.refresh_tokens.lock().expect("lock").clone();
    assert_eq!(seen, vec!["refresh-1".to_string(), "rotated-1".to_string()]);
    let orgs = edge.state.refresh_orgs.lock().expect("lock").clone();
    assert_eq!(orgs, vec![Some("org_1".into()), Some("org_1".into())]);
}

#[tokio::test]
async fn mismatched_refresh_scope_stops_room_tokens_and_preserves_rotation() {
    let edge = StubEdge::start().await;
    edge.state.token_ttl.store(20, Ordering::SeqCst);
    *edge.state.exchange_org.lock().expect("lock") = "org_1".into();
    *edge.state.refresh_org_override.lock().expect("lock") = Some("org_2".into());
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Auth::new(workos_config(&edge.url(), dir.path()));

    let url = auth.start_headless_sign_in();
    let state = query_param(&url, "state").expect("state");
    auth.complete_sign_in(&format!("{state}.codeX"))
        .await
        .expect("sign in");

    assert_eq!(auth.access_token().await, None);
    assert!(matches!(auth.state(), AuthState::NeedsOrganization { .. }));
    let raw = std::fs::read_to_string(dir.path().join("session.json")).expect("session persisted");
    assert!(
        raw.contains("rotated-1"),
        "rotation must be preserved: {raw}"
    );
    assert!(
        !raw.contains("org_1"),
        "invalid scope must be cleared: {raw}"
    );
}

#[tokio::test]
async fn revoked_refresh_token_signs_out() {
    let edge = StubEdge::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    // A persisted session whose refresh token the edge rejects with a definitive 4xx.
    std::fs::write(
        dir.path().join("session.json"),
        r#"{"refreshToken":"dead","user":{"id":"user_1","email":"w@example.com"},"orgId":"org_1"}"#,
    )
    .expect("seed session");
    let auth = Auth::new(workos_config(&edge.url(), dir.path()));
    assert!(
        auth.state().is_signed_in(),
        "boots from the persisted session"
    );

    // The refresh is doomed → the session degrades to SignedOut and the file is gone.
    assert_eq!(auth.access_token().await, None);
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert!(!dir.path().join("session.json").exists());
}

#[tokio::test]
async fn loopback_callback_completes_headed_sign_in() {
    let edge = StubEdge::start().await;
    *edge.state.exchange_org.lock().expect("lock") = "org_1".into();
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Auth::new(workos_config(&edge.url(), dir.path()));

    let url = auth.start_sign_in().await.expect("authorize url");
    let redirect = query_param(&url, "redirect_uri").expect("redirect");
    assert!(
        redirect.starts_with("http%3A%2F%2F127.0.0.1%3A"),
        "loopback redirect: {redirect}"
    );
    let state = query_param(&url, "state").expect("state");
    let callback: String = redirect.replace("%3A", ":").replace("%2F", "/");

    // A wrong/expired state is rejected without touching the exchange endpoint.
    let bad = reqwest::get(format!("{callback}?code=abc&state=wrong"))
        .await
        .expect("bad cb");
    assert_eq!(bad.status().as_u16(), 400);
    assert_eq!(edge.state.exchanges.load(Ordering::SeqCst), 0);

    // The browser hits the loopback callback → the engine exchanges the code with the
    // edge and the session lands org-scoped.
    let ok = reqwest::get(format!("{callback}?code=abc&state={state}"))
        .await
        .expect("cb");
    assert_eq!(ok.status().as_u16(), 200);
    let mut state_rx = auth.watch_state();
    wait_for(&mut state_rx, |s| s.is_signed_in()).await;
    assert_eq!(edge.state.exchanges.load(Ordering::SeqCst), 1);
    assert!(
        matches!(auth.state(), AuthState::SignedIn { org_id: Some(org), user } if org == "org_1" && user.name.as_deref() == Some("Wing Test"))
    );
}

#[tokio::test]
async fn construction_does_not_probe_edge_auth_mode() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let dir = tempfile::tempdir().expect("tempdir");
    let config = workos_config(&format!("http://127.0.0.1:{port}"), dir.path());

    let auth = Auth::new(config);

    assert!(auth.workos_enabled());
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "auth construction must not contact Edge"
    );
}

#[tokio::test]
async fn signed_out_headless_stops_gracefully_over_ipc() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve port");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig {
        data_dir: dir.path().to_path_buf(),
        edge_url: "http://127.0.0.1:1".into(),
        edge_token: None,
        ipc_port: port,
        default_harness: HarnessId::Mock,
        org_id: None,
        workos_client_id: Some("client_test".into()),
    };
    let engine = tokio::spawn(Engine::new(config).run());
    let client = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(client) = connect_ws(&format!("ws://127.0.0.1:{port}")).await {
                break client;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("headless Local IPC starts");

    let response = client
        .call(methods::STOP_ENGINE, serde_json::json!({}))
        .await
        .expect("stop acknowledgement");
    assert_eq!(response, serde_json::json!({ "ok": true }));
    tokio::time::timeout(Duration::from_secs(5), engine)
        .await
        .expect("engine stops")
        .expect("engine task")
        .expect("clean shutdown");

    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "IPC listener released"
    );
    assert_eq!(
        InstanceLock::holder(dir.path()),
        None,
        "engine lock released"
    );
}

#[tokio::test]
async fn supervisor_provisions_the_hidden_personal_org_before_runtime_boot() {
    let edge = StubEdge::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Auth::new(workos_config(&edge.url(), dir.path()));
    let url = auth.start_headless_sign_in();
    let state = query_param(&url, "state").expect("state param");
    auth.complete_sign_in(&format!("{state}.code123"))
        .await
        .expect("sign in");

    let supervisor = EngineSupervisor::new(
        EngineConfig {
            data_dir: dir.path().to_path_buf(),
            edge_url: edge.url(),
            edge_token: None,
            ipc_port: 0,
            default_harness: HarnessId::Mock,
            org_id: None,
            workos_client_id: Some("client_test".into()),
        },
        auth.clone(),
    );
    let boot = supervisor.spawn_when_ready();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind supervisor RPC");
    let address = listener.local_addr().expect("supervisor address");
    let server = tokio::spawn(jolt_rpc::serve_ws_listener(listener, supervisor.clone()));
    let client = connect_ws(&format!("ws://{address}"))
        .await
        .expect("connect remote client");
    client
        .call(methods::ENSURE_PERSONAL_ORG, serde_json::json!({}))
        .await
        .expect("ensure personal org");
    assert!(matches!(
        auth.state(),
        AuthState::SignedIn { org_id: Some(org), .. } if org == "org_1"
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if client
                .call(methods::LOCAL_DEVICE, serde_json::json!({}))
                .await
                .is_ok()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime serves data RPCs");
    assert!(dir.path().join("scopes/accounts/org_1/user_1").is_dir());
    supervisor.shutdown().await;
    boot.abort();
    server.abort();
}
