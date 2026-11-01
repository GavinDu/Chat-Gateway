//! Owner-authorized Playwright login browser. No credential or image logging.
//! Node owns one ephemeral Chromium/context per provider. Rust is the only
//! process allowed to write snapshots to the encrypted session vault.
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    process::Stdio,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::io::{AsyncWriteExt, BufReader};
mod protocol;
mod session_control;
use protocol::{public_event, read_worker_line};
pub use session_control::revoke;

const TTL: Duration = Duration::from_secs(20);
const IDLE: Duration = Duration::from_secs(10 * 60);
const LIFETIME: Duration = Duration::from_secs(30 * 60);
const HELPER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/browser-login/worker.mjs");

#[derive(Default)]
struct Registry {
    tickets: HashMap<String, (String, Instant)>,
    active: HashSet<String>,
    controls: HashMap<String, tokio::sync::watch::Sender<bool>>,
    revoking: HashSet<String>,
}
impl Registry {
    fn consume(&mut self, token: &str, provider: &str, now: Instant) -> bool {
        let Some((bound, expires)) = self.tickets.remove(token) else {
            return false;
        };
        bound == provider
            && expires > now
            && !self.revoking.contains(provider)
            && self.active.insert(provider.to_owned())
    }
}
fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}
fn shutdown_channel() -> &'static tokio::sync::watch::Sender<bool> {
    static CHANNEL: OnceLock<tokio::sync::watch::Sender<bool>> = OnceLock::new();
    CHANNEL.get_or_init(|| tokio::sync::watch::channel(false).0)
}
pub fn shutdown_all() {
    shutdown_channel().send_replace(true);
}
struct Lease(String);
impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut registry) = registry().lock() {
            registry.active.remove(&self.0);
            registry.controls.remove(&self.0);
        }
    }
}
struct Profile(std::path::PathBuf);
impl Profile {
    fn create() -> anyhow::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::path::Path::new("/dev/shm")
            .join(format!("chat-gateway-login-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Profile {
    fn drop(&mut self) {
        // Only the UUID directory allocated by this login is ever removed.
        // This also runs when worker launch, vault restore or streaming fails.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn uses_separate_authorization(method: &Method, path: &str) -> bool {
    if *method != Method::GET {
        return false;
    }
    if matches!(path, "/login" | "/login/browser.js") {
        return true;
    }
    let parts: Vec<_> = path.split('/').collect();
    parts.len() == 6
        && parts[1] == "login"
        && parts[2] == "api"
        && crate::providers::Provider::from_name(parts[3]).is_some()
        && parts[4] == "browser"
        && parts[5] == "stream"
}

fn error(status: StatusCode, code: &'static str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"error":{"code":code}})),
    )
        .into_response()
}

pub async fn javascript() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("login_browser.js"),
    )
}

pub async fn ticket(
    State(app): State<Arc<crate::AppState>>,
    headers: HeaderMap,
    Path(provider): Path<String>,
) -> Response {
    if !same_origin(&headers) {
        return error(StatusCode::FORBIDDEN, "same_origin_required");
    }
    // Unlike the legacy local relay, interactive login is never anonymous.
    if app.cfg.auth.keys.is_empty()
        || crate::authorize_api_request(
            &app.cfg.auth,
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            "/login/api/browser",
            &Method::POST,
        )
        .ok()
        .flatten()
        .is_none_or(|key| {
            key.provider_allowlist
                .is_some_and(|allowed| !allowed.contains(&provider))
        })
    {
        return error(StatusCode::FORBIDDEN, "owner_authorization_required");
    }
    if crate::providers::Provider::from_name(&provider).is_none() {
        return error(StatusCode::BAD_REQUEST, "unknown_provider");
    }
    if require_vault().is_err() {
        return error(StatusCode::SERVICE_UNAVAILABLE, "encrypted_vault_required");
    }
    let now = Instant::now();
    let Ok(mut registry) = registry().lock() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "browser_unavailable");
    };
    registry.tickets.retain(|_, (_, until)| *until > now);
    if registry.active.contains(&provider)
        || registry.revoking.contains(&provider)
        || registry.tickets.len() >= 16
    {
        return error(StatusCode::CONFLICT, "browser_busy");
    }
    let ticket = uuid::Uuid::new_v4().to_string();
    registry
        .tickets
        .insert(ticket.clone(), (provider, now + TTL));
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"ticket":ticket})),
    )
        .into_response()
}

fn require_vault() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::var("CHAT_GATEWAY_SESSION_STORE").as_deref() == Ok("vault"),
        "encrypted vault required"
    );
    crate::session::validate_encrypted_store_configuration()
}

fn same_origin(headers: &HeaderMap) -> bool {
    headers.contains_key(header::ORIGIN) && crate::login_origin_is_same_authority(headers)
}

#[cfg(test)]
mod origin_tests {
    use super::*;
    #[test]
    fn browser_controls_require_explicit_matching_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8099".parse().unwrap());
        assert!(!same_origin(&headers));
        for (origin, expected) in [
            ("http://127.0.0.1:8099", true),
            ("http://127.0.0.1:8098", false),
            ("https://evil.invalid", false),
            ("null", false),
        ] {
            headers.insert(header::ORIGIN, origin.parse().unwrap());
            assert_eq!(same_origin(&headers), expected);
        }
    }
}

pub async fn stream(
    State(app): State<Arc<crate::AppState>>,
    headers: HeaderMap,
    Path(provider): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    if !same_origin(&headers) {
        return error(StatusCode::FORBIDDEN, "same_origin_required");
    }
    let protocol = headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(token) = protocol.strip_prefix("cg-login.") else {
        return error(StatusCode::UNAUTHORIZED, "invalid_browser_ticket");
    };
    let accepted = registry()
        .lock()
        .map(|mut r| r.consume(token, &provider, Instant::now()))
        .unwrap_or(false);
    if !accepted {
        return error(StatusCode::UNAUTHORIZED, "invalid_browser_ticket");
    }
    let lease = Lease(provider.clone());
    let (cancel, receiver) = tokio::sync::watch::channel(false);
    if let Ok(mut registry) = registry().lock() {
        if registry.revoking.contains(&provider) {
            return error(StatusCode::CONFLICT, "session_revoking");
        }
        registry.controls.insert(provider.clone(), cancel);
    } else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "browser_busy");
    }
    ws.protocols([protocol.to_owned()])
        .max_message_size(16 * 1024)
        .max_frame_size(16 * 1024)
        .on_upgrade(move |socket| browser(socket, app, provider, lease, receiver))
        .into_response()
}

async fn write_command(
    input: &mut tokio::process::ChildStdin,
    value: &Value,
) -> anyhow::Result<()> {
    // Never trace this buffer: it can contain input or restored session data.
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let result = tokio::time::timeout(Duration::from_secs(5), input.write_all(&bytes)).await;
    use zeroize::Zeroize;
    bytes.zeroize();
    result??;
    Ok(())
}

async fn send(socket: &mut WebSocket, value: Value) -> bool {
    tokio::time::timeout(
        Duration::from_secs(5),
        socket.send(Message::Text(value.to_string())),
    )
    .await
    .is_ok_and(|r| r.is_ok())
}

async fn browser(
    mut socket: WebSocket,
    app: Arc<crate::AppState>,
    provider: String,
    _lease: Lease,
    cancel: tokio::sync::watch::Receiver<bool>,
) {
    if run_browser(&mut socket, &app, &provider, cancel)
        .await
        .is_err()
    {
        let _ = send(
            &mut socket,
            json!({"type":"error","code":"browser_runtime_unavailable"}),
        )
        .await;
    }
    let _ = socket.close().await;
}

async fn run_browser(
    socket: &mut WebSocket,
    app: &Arc<crate::AppState>,
    provider: &str,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    require_vault()?;
    let p = crate::providers::Provider::from_name(provider)
        .ok_or_else(|| anyhow::anyhow!("unknown provider"))?;
    let profile = Profile::create()?;
    // Don't forward the gateway environment (keys, cookies, debug logging).
    let mut command = tokio::process::Command::new("node");
    command
        .arg(HELPER)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LANG", "C.UTF-8")
        .env("CHAT_GATEWAY_LOGIN_TMPDIR", &profile.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = std::env::var_os("CHAT_GATEWAY_CHROMIUM_PATH") {
        command.env("CHAT_GATEWAY_CHROMIUM_PATH", path);
    }
    let mut child = command.spawn()?;
    let mut input = child.stdin.take().ok_or_else(|| anyhow::anyhow!("pipe"))?;
    let output = child.stdout.take().ok_or_else(|| anyhow::anyhow!("pipe"))?;
    let mut lines = BufReader::new(output);
    let mut pending_line = zeroize::Zeroizing::new(Vec::new());
    let mut shutdown = shutdown_channel().subscribe();
    let result: anyhow::Result<()> = async {
        // Interactive login must never use the legacy plaintext fallback,
        // even if a future caller accidentally bypasses ticket admission.
        let restored = crate::session::SessionStore::load(&crate::session::VaultSessionStore, provider)?;
        write_command(
            &mut input,
            &json!({
                "type":"init", "provider":provider, "url":p.login_url(),
                "restore":restored
            }),
        )
        .await?;
        let start = Instant::now();
        let mut last_input = Instant::now();
        let mut saving = false;
        let deadline = tokio::time::sleep(LIFETIME);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = cancel.changed() => {
                    let _ = send(socket,json!({"type":"error","code":"session_revoked"})).await;
                    break;
                },
                _ = &mut deadline => {
                    let _ = send(socket,json!({"type":"error","code":"session_expired"})).await;
                    break;
                },
                _ = tokio::time::sleep(Duration::from_secs(15)) => {
                    if last_input.elapsed() > IDLE || start.elapsed() > LIFETIME { break; }
                }
                message = socket.recv() => {
                    let text = match message {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                        _ => break,
                    };
                    let Ok(value) = serde_json::from_str::<Value>(&text) else { break };
                    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
                    if kind == "heartbeat" { continue; }
                    if kind == "cancel" { break; }
                    if saving || !matches!(kind, "save"|"navigate"|"mouse"|"wheel"|"key"|"text"|"resize"|"page") {
                        continue;
                    }
                    if let Some(url) = value.get("url").and_then(Value::as_str) {
                        if crate::login::validate_navigation_url_for_provider(provider, url).is_err() {
                            if !send(socket, json!({"type":"error","code":"navigation_blocked"})).await { break; }
                            continue;
                        }
                    }
                    saving = kind == "save";
                    last_input = Instant::now();
                    write_command(&mut input, &value).await?;
                }
                line = read_worker_line(&mut lines, &mut pending_line) => {
                    let Some(line) = line? else {
                        anyhow::bail!("browser worker stopped");
                    };
                    if line.len() > 4 * 1024 * 1024 { break; }
                    let mut value: Value = serde_json::from_str(&line)?;
                    match value.get("type").and_then(Value::as_str) {
                        Some("snapshot") if saving => {
                            // Snapshot is IPC-only, never forwarded to the browser UI.
                            let file: crate::session::SessionFile =
                                serde_json::from_value(value.as_object_mut().and_then(|v| v.remove("session")).unwrap_or(Value::Null))?;
                            if crate::session::import_relay_session(provider, &file).is_err() {
                                saving = false;
                                if !send(socket,json!({"type":"error","code":"vault_save_failed"})).await { break; }
                                continue;
                            }
                            let active = if app.cfg.provider(provider).is_some_and(|p| p.enabled) {
                                app.pool.refresh_session(provider).await.is_ok()
                            } else { false };
                            if active { app.provider_sessions.record_success(provider); }
                            let _ = send(socket,json!({"type":"saved","active":active})).await;
                            break;
                        }
                        Some("frame" | "status" | "ready" | "error" | "pages") => {
                            if value.get("type").and_then(Value::as_str) == Some("error") {
                                saving = false;
                            }
                            if let Some(event) = public_event(&value) {
                                if !send(socket,event).await { break; }
                            }
                        }
                        _ => {}
                    }
                    if last_input.elapsed() > IDLE { break; }
                }
            }
        }
        Ok(())
    }.await;
    let _ = write_command(&mut input, &json!({"type":"cancel"})).await;
    drop(input);
    if tokio::time::timeout(Duration::from_secs(8), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ephemeral_profile_is_private_and_removed_on_failure_drop() {
        use std::os::unix::fs::PermissionsExt;
        let path;
        {
            let profile = Profile::create().unwrap();
            path = profile.0.clone();
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert!(!path.exists());
    }
    #[test]
    fn tickets_expire_are_single_use_and_provider_bound() {
        let now = Instant::now();
        let mut r = Registry::default();
        r.tickets.insert("a".into(), ("chatgpt".into(), now + TTL));
        assert!(!r.consume("a", "claude", now));
        assert!(!r.consume("a", "chatgpt", now));
        r.tickets.insert("b".into(), ("chatgpt".into(), now + TTL));
        assert!(r.consume("b", "chatgpt", now));
        assert!(!r.consume("b", "chatgpt", now));
        r.tickets.insert("c".into(), ("chatgpt".into(), now + TTL));
        assert!(!r.consume("c", "chatgpt", now));
        r.tickets.insert("d".into(), ("claude".into(), now));
        assert!(!r.consume("d", "claude", now));
    }
    #[test]
    fn authorization_exceptions_are_exact_and_get_only() {
        assert!(uses_separate_authorization(&Method::GET, "/login"));
        assert!(uses_separate_authorization(
            &Method::GET,
            "/login/api/chatgpt/browser/stream"
        ));
        for path in [
            "/login/api/chatgpt/browser/ticket",
            "/login/api/chatgpt/complete",
            "/login/api/status",
            "/login/api/unknown/browser/stream",
            "/login/api/chatgpt/browser/stream/extra",
        ] {
            assert!(!uses_separate_authorization(&Method::GET, path));
        }
        assert!(!uses_separate_authorization(
            &Method::POST,
            "/login/api/chatgpt/browser/stream"
        ));
    }
    #[test]
    fn websocket_requires_explicit_same_origin() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "127.0.0.1:8080".parse().unwrap());
        assert!(!same_origin(&h));
        h.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(!same_origin(&h));
        h.insert(header::ORIGIN, "http://127.0.0.1:8080".parse().unwrap());
        assert!(same_origin(&h));
    }
}
