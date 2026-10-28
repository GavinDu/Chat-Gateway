use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Extension, Path, Query, State},
    http::HeaderMap,
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    response::{
        sse::{Event, Sse},
        Html, IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Instant};
use tokio::sync::RwLock;

mod api;
mod browser;
mod config;
mod context;
mod login;
mod memory;
mod metrics;
mod provider_guard;
mod provider_state;
mod providers;
mod relay;
mod router;
mod session;

use api::{add_gateway_headers, api_error, estimate_tokens, validate_chat_req, ChatReq};
use browser::pool::ObscuraPool;
use config::AppConfig;
use context::{ContextBuilder, ContextPlan, ThreadSyncState};
use memory::{ChatMessage, ProviderThread, UnifiedMemory};
use router::ProviderRouter;

#[derive(Parser, Debug)]
#[command(
    name = "chat-gateway-rs",
    about = "Chat Gateway v2 - Obscura native, unified memory over 8 providers"
)]
struct Cli {
    #[arg(long, default_value = "config.yaml")]
    config: std::path::PathBuf,

    #[arg(
        long,
        conflicts_with = "all",
        help = "Test one adapter live: discover models + show selectors, then exit"
    )]
    test_adapter: Option<String>,

    #[arg(
        long,
        conflicts_with = "test_adapter",
        help = "Run live diagnostics for every enabled provider"
    )]
    all: bool,

    #[arg(long, help = "Emit machine-readable JSON for adapter diagnostics")]
    json: bool,

    #[arg(
        long,
        value_name = "NAME",
        help = "Create a random chat API key, print it once and exit"
    )]
    create_api_key: Option<String>,

    #[arg(
        long,
        conflicts_with_all = ["all", "test_adapter", "create_api_key", "vault_delete_provider", "vault_rotate_key"],
        help = "List provider vault records and whether each is valid with CHAT_GATEWAY_VAULT_KEY"
    )]
    vault_list: bool,

    #[arg(
        long,
        value_name = "PROVIDER",
        conflicts_with_all = ["all", "test_adapter", "create_api_key", "vault_list", "vault_rotate_key"],
        help = "Delete the named provider's persisted session snapshot (vault and legacy file)"
    )]
    vault_delete_provider: Option<String>,

    #[arg(
        long,
        conflicts_with_all = ["all", "test_adapter", "create_api_key", "vault_list", "vault_delete_provider"],
        help = "Rotate encrypted vault records using CHAT_GATEWAY_VAULT_KEY and CHAT_GATEWAY_VAULT_NEW_KEY"
    )]
    vault_rotate_key: bool,

    #[arg(
        long,
        value_name = "BACKUP_DIR",
        conflicts_with_all = ["all", "test_adapter", "create_api_key", "vault_list", "vault_delete_provider", "vault_rotate_key"],
        help = "Restore encrypted vault records from a sessions/.vault-rotation-* backup using the old key"
    )]
    vault_restore_backup: Option<String>,
}

fn adapter_report_is_healthy(report: &browser::pool::TestAdapterReport) -> bool {
    report.errors.is_empty() && report.session_ok && report.input_found && report.response_token
}

#[derive(serde::Serialize)]
struct AdapterDiagnostic {
    provider: String,
    queue_ms: Option<u128>,
    report: Option<browser::pool::TestAdapterReport>,
    models: Option<Vec<String>>,
    selectors: Option<String>,
    errors: Vec<String>,
    healthy: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AuthKeyContext {
    /// Stable configured key name; never the raw bearer secret.
    key_id: String,
    provider_allowlist: Option<Vec<String>>,
}

struct AppState {
    cfg: Arc<AppConfig>,
    pool: Arc<ObscuraPool>,
    memory: Arc<UnifiedMemory>,
    router: ProviderRouter,
    models_cache: Arc<RwLock<HashMap<String, Vec<String>>>>,
    relay: Arc<relay::RelayState>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    if let Some(name) = cli.create_api_key.as_deref() {
        if name.trim().is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            anyhow::bail!("key name must contain only ASCII letters, digits, '_' or '-'");
        }
        let secret = format!(
            "cg_{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let hash = hash_api_key(&secret);
        println!("API key secret (shown once; store it securely): {secret}");
        println!("Add this entry under auth.keys in config.yaml:");
        println!("  - name: {name}");
        println!("    secret_hash: \"{hash}\"");
        println!("    scopes: [chat]");
        return Ok(());
    }
    if cli.vault_list {
        for entry in session::list_vault_sessions()? {
            println!("{}\t{}", entry.provider, entry.status);
        }
        return Ok(());
    }
    if let Some(provider) = cli.vault_delete_provider.as_deref() {
        let removed = session::delete_session_snapshot(provider)?;
        println!("provider={provider} removed={removed}");
        return Ok(());
    }
    if let Some(backup_dir) = cli.vault_restore_backup.as_deref() {
        let restored = session::restore_vault_backup(backup_dir)?;
        println!("Restored {restored} encrypted provider vault record(s) from {backup_dir}.");
        println!("Keep CHAT_GATEWAY_VAULT_KEY set to the old key until a fresh rotation succeeds.");
        return Ok(());
    }
    if cli.vault_rotate_key {
        let encoded =
            zeroize::Zeroizing::new(std::env::var("CHAT_GATEWAY_VAULT_NEW_KEY").map_err(|_| {
                anyhow::anyhow!("CHAT_GATEWAY_VAULT_NEW_KEY is required for vault key rotation")
            })?);
        let new_key = session::parse_vault_key(&encoded).map_err(|_| {
            anyhow::anyhow!("CHAT_GATEWAY_VAULT_NEW_KEY must be exactly 64 hexadecimal characters")
        })?;
        let (rotated, backup_dir) = session::rotate_vault_key(&new_key)?;
        if rotated == 0 {
            println!("No encrypted provider vault records found; nothing rotated.");
        } else {
            println!("Rotated {rotated} encrypted provider vault record(s).");
            println!("Encrypted recovery copies retained at: {backup_dir}");
            println!(
                "Update CHAT_GATEWAY_VAULT_KEY to the new key before restarting Chat Gateway."
            );
            println!(
                "Keep the old key and recovery copies until restart restoration has been verified."
            );
        }
        return Ok(());
    }
    if cli.json && !cli.all && cli.test_adapter.is_none() {
        anyhow::bail!("--json requires --all or --test-adapter <provider>");
    }
    let cfg = Arc::new(AppConfig::load(&cli.config)?);
    if cfg.auth.keys.is_empty() {
        tracing::warn!("API-key authentication is disabled; the listener remains loopback-only");
    }

    let pool = ObscuraPool::new(cfg.clone()).await?;
    pool.warm_all().await?;

    if cli.all || cli.test_adapter.is_some() {
        let names = if cli.all {
            cfg.providers
                .iter()
                .filter(|provider| provider.enabled)
                .map(|provider| provider.name.clone())
                .collect::<Vec<_>>()
        } else {
            vec![cli.test_adapter.clone().expect("checked above")]
        };
        let registry = providers::build_registry(&cfg);
        let mut results = Vec::with_capacity(names.len());

        for provider in names {
            let mut errors = Vec::new();
            let queue_ms = match pool.acquire(&provider).await {
                Ok(permit) => {
                    let queue_ms = permit.queue_ms;
                    drop(permit);
                    Some(queue_ms)
                }
                Err(error) => {
                    errors.push(format!("provider acquisition failed: {error}"));
                    None
                }
            };

            let report = if queue_ms.is_some() {
                match pool.test_adapter(&provider).await {
                    Ok(report) => Some(report),
                    Err(error) => {
                        errors.push(format!("live adapter test failed: {error}"));
                        None
                    }
                }
            } else {
                None
            };

            let (models, discovery_error) = match pool.discover(&provider).await {
                Ok(models) => (Some(models), None),
                Err(error) => (None, Some(error.to_string())),
            };
            if let Some(error) = &discovery_error {
                errors.push(format!("model discovery failed: {error}"));
            }

            let selectors = providers::Provider::from_name(&provider)
                .and_then(|provider_id| registry.get(&provider_id))
                .map(|adapter| format!("{:?}", adapter.selectors()));
            let healthy =
                errors.is_empty() && report.as_ref().is_some_and(adapter_report_is_healthy);

            results.push(AdapterDiagnostic {
                provider,
                queue_ms,
                report,
                models,
                selectors,
                errors,
                healthy,
            });
        }

        let all_healthy = results.iter().all(|result| result.healthy);
        if cli.json {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "ok": all_healthy,
                    "results": results,
                }))?
            );
        } else {
            for result in &results {
                println!(
                    "test-adapter '{}': healthy={} queue_ms={:?}",
                    result.provider, result.healthy, result.queue_ms
                );
                if let Some(report) = &result.report {
                    println!(
                        "  phases: session_ok={} input_found={} send_clicked={} response_token={}",
                        report.session_ok,
                        report.input_found,
                        report.send_clicked,
                        report.response_token
                    );
                    for error in &report.errors {
                        println!("  error: {error}");
                    }
                }
                for error in &result.errors {
                    println!("  error: {error}");
                }
                if let Some(models) = &result.models {
                    println!("  discovered models: {models:?}");
                }
                if let Some(selectors) = &result.selectors {
                    println!("  selectors: {selectors}");
                }
            }
        }

        if !all_healthy {
            anyhow::bail!("one or more adapter diagnostics failed");
        }
        return Ok(());
    }

    let (relay_state, pairing_code) = relay::RelayState::new()?;
    println!("Browser relay pairing code (single use, expires in 10 minutes): {pairing_code}");
    let state = Arc::new(AppState {
        cfg: cfg.clone(),
        pool: pool.clone(),
        memory: Arc::new(UnifiedMemory::from_env(cfg.memory.max_tokens)?),
        router: ProviderRouter::new(cfg.clone()),
        models_cache: Arc::new(RwLock::new(HashMap::new())),
        relay: Arc::new(relay_state),
    });

    // Background model discovery (reads pickers without sending messages)
    {
        let state = state.clone();
        tokio::spawn(async move {
            refresh_models(&state).await;
        });
    }

    // Retention is opt-in: a zero-day setting leaves conversation history untouched.
    let retention_days = cfg.memory.retention_days;
    if retention_days > 0 {
        let memory = state.memory.clone();
        let retention_seconds = i64::from(retention_days) * 86_400;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .min(i64::MAX as u64) as i64;
                let cutoff = now.saturating_sub(retention_seconds);
                match memory.purge_expired_conversations(cutoff) {
                    Ok(deleted) if deleted > 0 => {
                        tracing::info!(deleted, retention_days, "purged expired conversations");
                    }
                    Ok(_) => {}
                    Err(error) => tracing::error!(%error, "conversation retention purge failed"),
                }
                interval.tick().await;
            }
        });
    }

    let app = Router::new()
        .route("/health", get(api::health::health))
        .route("/ready", get(api::health::readiness))
        .route("/metrics", get(metrics_endpoint))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/conversations", get(list_conversations))
        .route(
            "/v1/conversations/:conversation_id/provider-threads/:provider",
            get(get_provider_thread).put(upsert_provider_thread),
        )
        .route(
            "/v1/conversations/:conversation_id/export.md",
            get(export_conversation_markdown),
        )
        .route(
            "/v1/conversations/:conversation_id",
            get(export_conversation).delete(delete_conversation),
        )
        .route("/login", get(login_page))
        .route("/login/api/status", get(login_status))
        .route("/login/api/:provider/start", post(login_start))
        .route("/login/api/:provider/cancel", post(login_cancel))
        .route("/login/api/:provider/complete", post(login_complete))
        .route("/login/api/:provider/view", get(login_view))
        .route("/login/api/:provider/act", post(login_act))
        .route("/relay/v1/pair", post(relay::pair))
        .route("/relay/v1/status", get(relay::status))
        .route(
            "/relay/v1/session/:provider",
            post(relay::import_session).delete(relay::unlink),
        )
        .route("/relay/v1/unpair", post(relay::unpair))
        .route("/relay/v1/*path", axum::routing::options(relay::preflight))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(
            state.cfg.auth.clone(),
            authenticate_api_key,
        ))
        // Semantic message limits run after JSON parsing; cap the raw body too
        // so an oversized payload cannot consume unbounded memory before
        // request validation has a chance to reject it.
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn(protect_login_api))
        // Keep request IDs on both successful responses and middleware rejections.
        .layer(middleware::from_fn(attach_request_id));

    // Security default: the gateway exposes browser/session control routes and must not listen on public interfaces by default.
    // Remote access should be added only with explicit authentication and TLS.
    let addr = server_bind_addr(cfg.server.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("Chat Gateway v2 Obscura Native listening on {}", addr);
    // Bound graceful draining so a stuck stream cannot prevent shutdown forever.
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let shutdown = async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(());
    };
    let mut server = Box::pin(std::future::IntoFuture::into_future(
        axum::serve(listener, app).with_graceful_shutdown(shutdown),
    ));
    tokio::select! {
        result = &mut server => result?,
        _ = &mut shutdown_rx => {
            match tokio::time::timeout(std::time::Duration::from_secs(30), &mut server).await {
                Ok(result) => result?,
                Err(_) => tracing::warn!("graceful shutdown exceeded 30 seconds; forcing shutdown"),
            }
        }
    }
    // HTTP requests have drained or the grace period expired; stop the browser worker before flushing storage.
    state.pool.shutdown().await?;
    state.memory.checkpoint()?;
    drop(state);
    Ok(())
}

/// Stop accepting new connections on SIGINT/SIGTERM and let Axum drain
/// in-flight requests before the process exits.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut terminate =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    tracing::warn!(%error, "failed to listen for Ctrl-C");
                }
            }
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "failed to listen for Ctrl-C");
        }
    }

    tracing::info!("shutdown signal received; draining in-flight HTTP requests");
}

async fn refresh_models(state: &Arc<AppState>) {
    for p in &state.cfg.providers {
        if !p.enabled {
            continue;
        }
        match state.pool.discover(&p.name).await {
            Ok(models) => {
                state
                    .models_cache
                    .write()
                    .await
                    .insert(p.name.clone(), models);
            }
            Err(e) => tracing::warn!(provider = %p.name, "model discovery failed: {e}"),
        }
    }
}

/// Login delegate UI (see src/login.rs): relay-based provider login.
/// The gateway never presents its own credential form and never stores
/// credentials — relayed text lives in memory only for the action that
/// types it, and only cookies/localStorage are persisted on Complete.
/// The login relay can read and persist provider session material. Keep the HTTP
/// listener loopback-only until an authenticated remote-access mode is designed.
fn server_bind_addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Protect browser-control endpoints against cross-origin requests (CSRF).
/// Non-browser clients without an Origin header remain usable, while browser
/// requests must have an Origin authority identical to the request Host.
async fn protect_login_api(req: Request<Body>, next: Next) -> Response {
    let headers = req.headers();
    // The service binds to loopback, but DNS rebinding can still send a
    // browser request with an attacker-controlled Host that resolves to
    // 127.0.0.1. Enforce the loopback authority on every route, not only
    // the browser-control endpoints.
    let host_is_loopback = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(is_loopback_authority)
        .unwrap_or(false);
    let origin_present = headers.contains_key(axum::http::header::ORIGIN);
    let fetch_site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());
    let is_login_api = req.uri().path().starts_with("/login/api/");
    let is_relay_api = req.uri().path().starts_with("/relay/v1/");
    let origin_is_safe = if is_relay_api {
        relay::relay_origin_is_allowed(headers)
    } else {
        !origin_present || login_origin_is_same_authority(headers)
    };
    if !host_is_loopback || (is_login_api && fetch_site == Some("cross-site")) || !origin_is_safe {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": {
                    "message": "request authority or origin rejected",
                    "type": "security_error",
                    "code": "unsafe_local_request"
                }
            })),
        )
            .into_response();
    }
    let allowed_relay_origin = if is_relay_api {
        headers.get(axum::http::header::ORIGIN).cloned()
    } else {
        None
    };
    let mut response = next.run(req).await;
    if let Some(origin) = allowed_relay_origin {
        response
            .headers_mut()
            .insert(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        response.headers_mut().insert(
            axum::http::header::VARY,
            axum::http::HeaderValue::from_static("Origin"),
        );
    }
    response
}

fn login_origin_is_same_authority(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        // CLI and local automation clients commonly omit Origin. The
        // Sec-Fetch-Site check above still rejects explicitly cross-site calls.
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };

    // Comparing Origin only to the request Host is insufficient: DNS rebinding
    // can make an attacker's hostname resolve to loopback, causing both values
    // to match. The login relay is local-only, so accept only explicit loopback
    // authorities and reject arbitrary DNS names even when they match Origin.
    authority.eq_ignore_ascii_case(host) && is_loopback_authority(host)
}

fn is_loopback_authority(authority: &str) -> bool {
    if authority.contains('@') || authority.contains('/') || authority.contains('\\') {
        return false;
    }
    let (host, port) = if authority.starts_with('[') {
        let Some((host, rest)) = authority.split_once(']') else {
            return false;
        };
        let port = if rest.is_empty() {
            None
        } else if let Some(port) = rest.strip_prefix(':') {
            Some(port)
        } else {
            return false;
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => (host, Some(port)),
            Some(_) => return false,
            None => (authority, None),
        }
    };
    if let Some(port) = port {
        if port.parse::<u16>().ok().filter(|p| *p != 0).is_none() {
            return false;
        }
    }
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "[::1" | "[::1]"
    )
}

async fn login_page() -> impl IntoResponse {
    Html(login::LOGIN_HTML)
}

fn login_err(msg: String) -> axum::response::Response {
    (
        axum::http::StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

async fn login_status(State(state): State<Arc<AppState>>) -> axum::response::Response {
    let active = match state.pool.login_status_all().await {
        Ok(active) => active,
        Err(_error) => {
            tracing::error!("login status lookup failed");
            return login_status_read_error();
        }
    };
    let by_name: HashMap<String, browser::pool::LoginActiveInfo> = active
        .into_iter()
        .map(|a| (a.provider.clone(), a))
        .collect();
    let mut providers = Vec::new();
    for p in providers::Provider::all() {
        let name = p.name();
        let enabled = state.cfg.provider(name).map(|c| c.enabled).unwrap_or(false);
        let (session_present, session_ts) = match session::load_session_file(name) {
            Ok(Some(file)) => (true, Some(file.ts.clone())),
            Ok(None) => (false, None),
            Err(_error) => {
                tracing::error!(provider = name, "persisted login session read failed");
                return login_status_read_error();
            }
        };
        let active_login = by_name.get(name);
        providers.push(serde_json::json!({
            "name": name,
            "enabled": enabled,
            "login_url": p.login_url(),
            "session_present": session_present,
            "session_ts": session_ts,
            "login_active": active_login.is_some(),
            "current_url": active_login.map(|x| x.url.clone()),
            "title": active_login.map(|x| x.title.clone()),
            "logged_in": active_login.and_then(|x| x.logged_in),
            "input_found": active_login.map(|x| x.input_found),
            "state": active_login.map(|x| x.state.clone()),
        }));
    }
    Json(serde_json::json!({ "providers": providers })).into_response()
}

fn login_status_read_error() -> axum::response::Response {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "login_status_read_failed",
        "login status could not be read safely",
    )
}
async fn login_start(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
) -> impl IntoResponse {
    match state.pool.login_start(&provider).await {
        Ok(url) => Json(serde_json::json!({ "ok": true, "provider": provider, "url": url }))
            .into_response(),
        Err(e) => login_err(e.to_string()),
    }
}

async fn login_cancel(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
) -> impl IntoResponse {
    match state.pool.login_cancel(&provider).await {
        Ok(()) => Json(serde_json::json!({ "ok": true, "provider": provider })).into_response(),
        Err(e) => login_err(e.to_string()),
    }
}

async fn login_complete(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
) -> impl IntoResponse {
    match state.pool.login_complete(&provider).await {
        Ok(r) => {
            // Successful human re-authentication must make the provider routable
            // again; otherwise needs_login is sticky because routing skips sends.
            provider_state::record_success(&provider);
            Json(serde_json::json!({
                "ok": true, "provider": provider,
                "session_path": r.session_path,
                "warm_swapped": r.warm_swapped,
                "warning": r.warning,
            }))
            .into_response()
        }
        Err(e) => login_err(e.to_string()),
    }
}
async fn login_view(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
) -> impl IntoResponse {
    match state.pool.login_view(&provider).await {
        Ok(v) => Json(serde_json::to_value(v).unwrap_or(serde_json::json!({}))).into_response(),
        Err(e) => login_err(e.to_string()),
    }
}

#[derive(serde::Deserialize, Default)]
struct LoginActReq {
    action: String,
    #[serde(default)]
    selector: Option<String>,
    // Relayed text: used only to drive human_type; never logged or
    // echoed back in any response (see login.rs).
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    index: Option<usize>,
}

/// Validate relayed actions at the HTTP boundary, before browser-thread dispatch.
/// Navigation needs the provider-specific allowlist in addition to the generic
/// URL safety checks performed by LoginAction::validate().
fn validate_login_action_for_provider(
    provider: &str,
    action: &login::LoginAction,
) -> anyhow::Result<()> {
    action.validate()?;
    if let login::LoginAction::Navigate { url } = action {
        login::validate_navigation_url_for_provider(provider, url)?;
    }
    Ok(())
}

#[cfg(test)]
mod corrupt_message_history_regression_tests {
    use super::*;

    #[tokio::test]
    async fn dropping_messages_table_after_startup_fails_closed_without_sql_details() {
        let dir =
            std::env::temp_dir().join(format!("gateway-corrupt-history-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let db = dir.join("memory.sqlite3");
        let memory = UnifiedMemory::open_path_for_test(&db, None, 8000).unwrap();
        memory
            .store_turn(
                "conversation-123".into(),
                vec![ChatMessage {
                    role: "user".into(),
                    content: "first".into(),
                }],
                ChatMessage {
                    role: "assistant".into(),
                    content: "answer".into(),
                },
                "chatgpt".into(),
                "tab-123".into(),
            )
            .unwrap();

        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("DROP TABLE messages;")
            .unwrap();

        let response = match read_conversation_context(
            &memory,
            Some("conversation-123"),
            &[ChatMessage {
                role: "user".into(),
                content: "follow-up".into(),
            }],
        ) {
            Err(_) => conversation_context_read_error(Some("conversation-123")),
            Ok(_) => panic!("corrupt message history must fail closed"),
        };
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "memory_read_failed");
        assert_eq!(body["error"]["type"], "server_error");
        assert_eq!(
            body["error"]["message"],
            "conversation history could not be read safely"
        );
        assert!(!body.to_string().contains("DROP TABLE"));
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod request_body_error_tests {
    use super::*;

    #[tokio::test]
    async fn oversized_body_error_uses_stable_openai_envelope() {
        let response = oversized_chat_body_error();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "application/json"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], "request_too_large");
    }
}

#[cfg(test)]
mod login_status_error_tests {
    use super::*;

    #[tokio::test]
    async fn login_status_read_failure_returns_sanitized_structured_error() {
        let response = login_status_read_error();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["type"], "server_error");
        assert_eq!(body["error"]["code"], "login_status_read_failed");
        assert_eq!(
            body["error"]["message"],
            "login status could not be read safely"
        );
    }
}

#[cfg(test)]
mod login_action_validation_tests {
    use super::*;

    #[test]
    fn login_action_rejects_unrelated_navigation_before_dispatch() {
        let action = login::LoginAction::Navigate {
            url: "https://attacker.example.org/collect".to_owned(),
        };
        assert!(validate_login_action_for_provider("chatgpt", &action).is_err());
    }

    #[test]
    fn login_action_allows_provider_and_common_sso_destinations() {
        let provider = login::LoginAction::Navigate {
            url: "https://chatgpt.com/".to_owned(),
        };
        let sso = login::LoginAction::Navigate {
            url: "https://accounts.google.com/o/oauth2/".to_owned(),
        };
        assert!(validate_login_action_for_provider("chatgpt", &provider).is_ok());
        assert!(validate_login_action_for_provider("chatgpt", &sso).is_ok());
    }

    #[test]
    fn login_action_rejects_another_provider_domain() {
        let action = login::LoginAction::Navigate {
            url: "https://claude.ai/".to_owned(),
        };
        assert!(validate_login_action_for_provider("chatgpt", &action).is_err());
    }
}

/// Convert an untrusted HTTP payload into a complete action before any browser
/// dispatch. Missing fields are errors, never defaults that could trigger an
/// unintended click or type operation.
fn parse_login_action(req: LoginActReq) -> anyhow::Result<login::LoginAction> {
    fn required_selector(selector: Option<String>, field: &str) -> anyhow::Result<String> {
        let selector = selector.ok_or_else(|| anyhow::anyhow!("{field} is required"))?;
        if selector.trim().is_empty() {
            anyhow::bail!("{field} must not be blank");
        }
        Ok(selector)
    }

    match req.action.as_str() {
        "click" => Ok(login::LoginAction::Click {
            selector: required_selector(req.selector, "selector")?,
        }),
        "click_button" => Ok(login::LoginAction::ClickButton {
            index: req
                .index
                .ok_or_else(|| anyhow::anyhow!("index is required"))?,
        }),
        "type" => Ok(login::LoginAction::Type {
            selector: required_selector(req.selector, "selector")?,
            text: req
                .text
                .ok_or_else(|| anyhow::anyhow!("text is required"))?,
        }),
        "enter" => Ok(login::LoginAction::Enter {
            selector: required_selector(req.selector, "selector")?,
        }),
        "navigate" => {
            let url = req.url.ok_or_else(|| anyhow::anyhow!("url is required"))?;
            if url.trim().is_empty() {
                anyhow::bail!("url must not be blank");
            }
            Ok(login::LoginAction::Navigate { url })
        }
        other => anyhow::bail!("unknown login action: {other}"),
    }
}

fn legacy_login_relay_enabled_from(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    })
}

fn legacy_login_relay_enabled() -> bool {
    legacy_login_relay_enabled_from(
        std::env::var("CHAT_GATEWAY_LEGACY_LOGIN_RELAY")
            .ok()
            .as_deref(),
    )
}

async fn login_act(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    Json(req): Json<LoginActReq>,
) -> impl IntoResponse {
    let action = match parse_login_action(req) {
        Ok(action) => action,
        Err(error) => return login_err(error.to_string()),
    };
    if matches!(&action, login::LoginAction::Type { .. }) && !legacy_login_relay_enabled() {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "legacy text relay is disabled; use the browser extension or explicitly set CHAT_GATEWAY_LEGACY_LOGIN_RELAY=1 for trusted local development"
            })),
        ).into_response();
    }
    if let Err(error) = validate_login_action_for_provider(&provider, &action) {
        return login_err(error.to_string());
    }
    match state.pool.login_act(&provider, action).await {
        // Deliberately no echo of the request: the detail strings never
        // contain relayed text.
        Ok(detail) => Json(serde_json::json!({ "ok": true, "detail": detail })).into_response(),
        Err(e) => login_err(e.to_string()),
    }
}

#[cfg(test)]
mod login_action_payload_tests {
    use super::*;

    fn parse(value: serde_json::Value) -> anyhow::Result<login::LoginAction> {
        parse_login_action(serde_json::from_value(value)?)
    }

    #[test]
    fn rejects_missing_or_blank_selectors() {
        for payload in [
            serde_json::json!({"action":"click"}),
            serde_json::json!({"action":"click","selector":""}),
            serde_json::json!({"action":"click","selector":"   "}),
            serde_json::json!({"action":"type","text":"secret"}),
            serde_json::json!({"action":"enter","selector":""}),
        ] {
            assert!(parse(payload).is_err());
        }
    }

    #[test]
    fn rejects_missing_navigation_url_and_button_index() {
        for payload in [
            serde_json::json!({"action":"navigate"}),
            serde_json::json!({"action":"navigate","url":" "}),
            serde_json::json!({"action":"click_button"}),
        ] {
            assert!(parse(payload).is_err());
        }
    }

    #[test]
    fn rejects_missing_type_text_and_unknown_actions() {
        assert!(parse(serde_json::json!({"action":"type","selector":"#email"})).is_err());
        assert!(parse(serde_json::json!({"action":"hover"})).is_err());
    }

    #[test]
    fn accepts_complete_action_payloads_without_changing_values() {
        assert!(matches!(
            parse(serde_json::json!({"action":"click","selector":"#continue"})).unwrap(),
            login::LoginAction::Click { selector } if selector == "#continue"
        ));
        assert!(matches!(
            parse(serde_json::json!({"action":"click_button","index":0})).unwrap(),
            login::LoginAction::ClickButton { index: 0 }
        ));
        assert!(matches!(
            parse(serde_json::json!({"action":"navigate","url":"https://chatgpt.com/"})).unwrap(),
            login::LoginAction::Navigate { url } if url == "https://chatgpt.com/"
        ));
    }
}

/// Readiness probe for process supervisors and local deployment wrappers.
/// Startup validates configuration and warms the browser pool before binding.
/// Readiness requires at least one enabled provider whose session is not known
/// to be expired; providers marked `needs_login` do not count as routable.
async fn metrics_endpoint(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let provider_states = state
        .cfg
        .providers
        .iter()
        .map(|provider| {
            (
                provider.name.clone(),
                provider_state::status(&provider.name)
                    .state
                    .as_str()
                    .to_owned(),
            )
        })
        .collect::<Vec<_>>();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    (
        headers,
        metrics::render_with_provider_states(&provider_states),
    )
}

/// Add a gateway-generated request ID to every response, including security
/// middleware rejections, so callers can correlate failures with server logs
/// without trusting arbitrary client-supplied IDs.
async fn attach_request_id(mut req: Request<Body>, next: Next) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    let method = req.method().to_string();
    let route = metrics::normalize_route(req.uri().path());
    let is_chat_completion = req.uri().path() == "/v1/chat/completions";
    if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
        req.headers_mut().insert("x-request-id", value);
    }
    let started = Instant::now();
    let mut response = next.run(req).await;
    let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    metrics::record_http_request(&route, response.status().as_u16(), duration_ms);
    tracing::info!(
        request_id = %request_id,
        method = %method,
        route = %route,
        status = response.status().as_u16(),
        duration_ms,
        "http request handled"
    );

    // Axum's DefaultBodyLimit rejection happens before the Bytes handler is
    // invoked. Normalize that framework rejection to the same stable error
    // envelope used by the chat endpoint, while preserving HTTP 413.
    if is_chat_completion && response.status() == StatusCode::PAYLOAD_TOO_LARGE {
        response = api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "request_too_large",
            "request body exceeds the 1 MiB limit",
        );
    }

    if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

fn hash_api_key(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (a, b)| difference | (*a ^ *b))
        == 0
}

#[derive(Debug, PartialEq, Eq)]
enum AuthFailure {
    MissingOrInvalid,
    InsufficientScope,
}

fn authorize_api_request(
    auth: &config::AuthConfig,
    authorization: Option<&str>,
    path: &str,
    method: &axum::http::Method,
) -> Result<Option<AuthKeyContext>, AuthFailure> {
    if path.starts_with("/relay/v1/") {
        // Relay handlers enforce a separate extension-origin + install-token protocol.
        return Ok(None);
    }
    if auth.keys.is_empty() || path == "/health" {
        return Ok(None);
    }
    let header = authorization.ok_or(AuthFailure::MissingOrInvalid)?;
    let (scheme, secret) = header
        .split_once(' ')
        .ok_or(AuthFailure::MissingOrInvalid)?;
    if !scheme.eq_ignore_ascii_case("bearer") || secret.is_empty() || secret.contains(' ') {
        return Err(AuthFailure::MissingOrInvalid);
    }
    let digest = hash_api_key(secret);
    let key = auth
        .keys
        .iter()
        .find(|key| {
            constant_time_eq(
                key.secret_hash.to_ascii_lowercase().as_bytes(),
                digest.as_bytes(),
            )
        })
        .ok_or(AuthFailure::MissingOrInvalid)?;

    let admin_route = path.starts_with("/login")
        || path.starts_with("/relay/")
        || path == "/metrics"
        || path == "/v1/conversations"
        || path.contains("/provider-threads/")
        || (path.starts_with("/v1/conversations/")
            && (*method == axum::http::Method::GET || *method == axum::http::Method::DELETE));
    let required_scope = if admin_route { "admin" } else { "chat" };
    if !key.scopes.iter().any(|scope| scope == required_scope) {
        return Err(AuthFailure::InsufficientScope);
    }
    Ok(Some(AuthKeyContext {
        key_id: format!("key:{}", key.name),
        provider_allowlist: key.provider_allowlist.clone(),
    }))
}

async fn authenticate_api_key(
    State(auth): State<config::AuthConfig>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let authorization = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    match authorize_api_request(&auth, authorization, req.uri().path(), req.method()) {
        Ok(Some(context)) => {
            req.extensions_mut().insert(context);
            next.run(req).await
        }
        Ok(None) => next.run(req).await,
        Err(AuthFailure::MissingOrInvalid) => api_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid_api_key",
            "a valid Bearer API key is required",
        ),
        Err(AuthFailure::InsufficientScope) => api_error(
            StatusCode::FORBIDDEN,
            "authentication_error",
            "insufficient_scope",
            "the API key does not have the required scope",
        ),
    }
}

#[cfg(test)]
fn delete_conversation_response(memory: &UnifiedMemory, conversation_id: &str) -> Response {
    delete_conversation_response_for_owner(memory, "local", conversation_id)
}

fn delete_conversation_response_for_owner(
    memory: &UnifiedMemory,
    owner_key_id: &str,
    conversation_id: &str,
) -> Response {
    if !valid_conversation_id(conversation_id) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_conversation_id",
            "conversation ID must be 8-128 ASCII letters, digits, '_' or '-'",
        );
    }
    match memory.delete_conversation_for_owner(owner_key_id, conversation_id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_error(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "conversation_not_found",
            "conversation does not exist",
        ),
        Err(_error) => {
            tracing::error!(conversation_id = %conversation_id, "conversation deletion failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_persistence_failed",
                "conversation could not be deleted durably",
            )
        }
    }
}

#[cfg(test)]
fn export_conversation_response(memory: &UnifiedMemory, conversation_id: &str) -> Response {
    export_conversation_response_for_owner(memory, "local", conversation_id)
}

fn export_conversation_response_for_owner(
    memory: &UnifiedMemory,
    owner_key_id: &str,
    conversation_id: &str,
) -> Response {
    if !valid_conversation_id(conversation_id) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_conversation_id",
            "conversation ID must be 8-128 ASCII letters, digits, '_' or '-'",
        );
    }
    match memory.export_conversation_for_owner(owner_key_id, conversation_id) {
        Ok(Some(conversation)) => Json(conversation).into_response(),
        Ok(None) => api_error(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "conversation_not_found",
            "conversation does not exist",
        ),
        Err(_error) => {
            tracing::error!(conversation_id = %conversation_id, "conversation export failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_persistence_failed",
                "conversation could not be exported safely",
            )
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct ConversationListQuery {
    limit: Option<usize>,
    offset: Option<usize>,
}

async fn list_conversations(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ConversationListQuery>,
    auth_context: Option<Extension<AuthKeyContext>>,
) -> Response {
    let owner_key_id = auth_context
        .as_ref()
        .map(|Extension(context)| context.key_id.as_str())
        .unwrap_or("local");
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let offset = query.offset.unwrap_or(0);
    match state
        .memory
        .list_conversations_for_owner(owner_key_id, limit, offset)
    {
        Ok(conversations) => Json(serde_json::json!({
            "data": conversations,
            "limit": limit,
            "offset": offset,
        }))
        .into_response(),
        Err(_error) => {
            tracing::error!("conversation list failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_persistence_failed",
                "conversations could not be listed safely",
            )
        }
    }
}

async fn get_provider_thread(
    State(state): State<Arc<AppState>>,
    Path((conversation_id, provider)): Path<(String, String)>,
    auth_context: Option<Extension<AuthKeyContext>>,
) -> Response {
    let owner_key_id = auth_context
        .as_ref()
        .map(|Extension(context)| context.key_id.as_str())
        .unwrap_or("local");
    if !valid_conversation_id(&conversation_id) || !valid_provider_name(&provider) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_provider_thread_key",
            "conversation ID or provider is invalid",
        );
    }
    match state
        .memory
        .get_provider_thread_for_owner(owner_key_id, &conversation_id, &provider)
    {
        Ok(Some(thread)) => Json(thread).into_response(),
        Ok(None) => api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "provider_thread_not_found",
            "no provider thread is stored for this conversation and provider",
        ),
        Err(error) => {
            tracing::error!(%error, "provider thread lookup failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_read_failed",
                "provider thread could not be read safely",
            )
        }
    }
}

async fn upsert_provider_thread(
    State(state): State<Arc<AppState>>,
    Path((conversation_id, provider)): Path<(String, String)>,
    auth_context: Option<Extension<AuthKeyContext>>,
    Json(mut thread): Json<ProviderThread>,
) -> Response {
    let owner_key_id = auth_context
        .as_ref()
        .map(|Extension(context)| context.key_id.as_str())
        .unwrap_or("local");
    if !valid_conversation_id(&conversation_id)
        || !valid_provider_name(&provider)
        || thread.conversation_id != conversation_id
        || thread.provider != provider
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "provider_thread_path_mismatch",
            "body conversation_id and provider must match the valid request path",
        );
    }
    if !valid_provider_thread_url(&provider, &thread.thread_url) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_provider_thread_url",
            "thread_url must be an HTTPS URL on the expected provider domain",
        );
    }
    thread.updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64;
    match state
        .memory
        .upsert_provider_thread_for_owner(owner_key_id, &thread)
    {
        Ok(()) => Json(thread).into_response(),
        Err(error) => {
            tracing::error!(%error, conversation_id, provider, "provider thread persistence failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_persistence_failed",
                "provider thread could not be stored safely",
            )
        }
    }
}

fn transcript_marker(messages: &[ChatMessage]) -> String {
    let encoded = serde_json::to_vec(messages).unwrap_or_default();
    format!("{:x}", Sha256::digest(encoded))
}

async fn persist_provider_thread(
    state: &AppState,
    owner_key_id: &str,
    conversation_id: &str,
    provider: &str,
    model: &str,
) {
    let thread_url = match state.pool.current_url(provider).await {
        Ok(url) if is_reopenable_provider_thread_url(provider, &url) => url,
        Ok(_) => return,
        Err(error) => {
            tracing::debug!(provider, %error, "provider thread URL unavailable after successful send");
            return;
        }
    };
    let messages = match state
        .memory
        .conversation_messages_for_owner(owner_key_id, conversation_id)
    {
        Ok(messages) => messages,
        Err(error) => {
            tracing::warn!(provider, %error, "provider thread sync marker could not be read");
            return;
        }
    };
    let thread = ProviderThread {
        conversation_id: conversation_id.to_owned(),
        provider: provider.to_owned(),
        thread_url,
        synced_through_message_id: Some(transcript_marker(&messages)),
        model: Some(model.to_owned()),
        updated_at: chrono::Utc::now().timestamp(),
    };
    if let Err(error) = state
        .memory
        .upsert_provider_thread_for_owner(owner_key_id, &thread)
    {
        tracing::warn!(provider, %error, "provider thread mapping could not be persisted");
    }
}

fn is_reopenable_provider_thread_url(provider: &str, value: &str) -> bool {
    if !valid_provider_thread_url(provider, value) {
        return false;
    }
    let Some((_, location)) = value["https://".len()..].split_once('/') else {
        return false;
    };
    let location = format!("/{location}");
    match provider {
        "chatgpt" => {
            location.starts_with("/c/") || (location.starts_with("/g/") && location.contains("/c/"))
        }
        "claude" => location.starts_with("/chat/") && location.len() > "/chat/".len(),
        "gemini" => location.starts_with("/app/") && location.len() > "/app/".len(),
        "grok" => {
            location.starts_with("/c/")
                || location.starts_with("/chat/")
                || location.contains("?conversation=")
        }
        "qwen" => location.starts_with("/c/") && location.len() > "/c/".len(),
        "mistral" => location.starts_with("/chat/") && location.len() > "/chat/".len(),
        "perplexity" => location.starts_with("/search/") || location.starts_with("/page/"),
        _ => false,
    }
}

fn valid_provider_name(provider: &str) -> bool {
    matches!(
        provider,
        "chatgpt" | "claude" | "gemini" | "grok" | "qwen" | "mistral" | "perplexity" | "metaai"
    )
}

fn valid_provider_thread_url(provider: &str, value: &str) -> bool {
    if value.len() > 4096 || !value.starts_with("https://") || value.contains('@') {
        return false;
    }
    let authority = value["https://".len()..]
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if authority.is_empty() || authority.contains(':') {
        return false;
    }
    let allowed_hosts: &[&str] = match provider {
        "chatgpt" => &["chatgpt.com", "chat.openai.com"],
        "claude" => &["claude.ai"],
        "gemini" => &["gemini.google.com"],
        "grok" => &["grok.com", "x.com"],
        "qwen" => &["chat.qwen.ai", "qwen.ai"],
        "mistral" => &["chat.mistral.ai", "lechat.mistral.ai"],
        "perplexity" => &["perplexity.ai"],
        "metaai" => &["meta.ai"],
        _ => &[],
    };
    allowed_hosts.contains(&authority.as_str())
}

async fn export_conversation(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<String>,
    auth_context: Option<Extension<AuthKeyContext>>,
) -> Response {
    let owner_key_id = auth_context
        .as_ref()
        .map(|Extension(context)| context.key_id.as_str())
        .unwrap_or("local");
    export_conversation_response_for_owner(&state.memory, owner_key_id, &conversation_id)
}

async fn export_conversation_markdown(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<String>,
    auth_context: Option<Extension<AuthKeyContext>>,
) -> Response {
    let owner_key_id = auth_context
        .as_ref()
        .map(|Extension(context)| context.key_id.as_str())
        .unwrap_or("local");
    export_conversation_markdown_response_for_owner(&state.memory, owner_key_id, &conversation_id)
}

#[cfg(test)]
fn export_conversation_markdown_response(
    memory: &UnifiedMemory,
    conversation_id: &str,
) -> Response {
    export_conversation_markdown_response_for_owner(memory, "local", conversation_id)
}

fn export_conversation_markdown_response_for_owner(
    memory: &UnifiedMemory,
    owner_key_id: &str,
    conversation_id: &str,
) -> Response {
    if !valid_conversation_id(conversation_id) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_conversation_id",
            "conversation ID must be 8-128 ASCII letters, digits, '_' or '-'",
        );
    }
    match memory.export_conversation_for_owner(owner_key_id, conversation_id) {
        Ok(Some(conversation)) => {
            let mut markdown = format!("# Conversation {}\n\n", conversation.id);
            if let Some(provider) = conversation.provider.as_deref() {
                markdown.push_str(&format!("- Provider: {provider}\n"));
            }
            if let Some(tab_id) = conversation.tab_id.as_deref() {
                markdown.push_str(&format!("- Tab: {tab_id}\n"));
            }
            markdown.push('\n');
            for (index, message) in conversation.messages.iter().enumerate() {
                markdown.push_str(&format!(
                    "## {} — {}\n\n{}\n\n",
                    index + 1,
                    message.role,
                    message.content
                ));
            }
            (
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/markdown; charset=utf-8",
                )],
                markdown,
            )
                .into_response()
        }
        Ok(None) => api_error(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "conversation_not_found",
            "conversation does not exist",
        ),
        Err(_error) => {
            tracing::error!(conversation_id = %conversation_id, "conversation Markdown export failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_persistence_failed",
                "conversation could not be exported safely",
            )
        }
    }
}

async fn delete_conversation(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<String>,
    auth_context: Option<Extension<AuthKeyContext>>,
) -> Response {
    let owner_key_id = auth_context
        .as_ref()
        .map(|Extension(context)| context.key_id.as_str())
        .unwrap_or("local");
    delete_conversation_response_for_owner(&state.memory, owner_key_id, &conversation_id)
}

fn build_model_list(cfg: &AppConfig, cache: &HashMap<String, Vec<String>>) -> serde_json::Value {
    let enabled = cfg
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .map(|provider| provider.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut data = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for provider in cfg.providers.iter().filter(|provider| provider.enabled) {
        for model in &provider.models {
            if seen.insert(model.clone()) {
                data.push(serde_json::json!({
                    "id": model,
                    "object": "model",
                    "owned_by": provider.name
                }));
            }
        }
    }

    for (provider, models) in cache {
        if !enabled.contains(provider.as_str()) {
            continue;
        }
        for model in models {
            if seen.insert(model.clone()) {
                data.push(serde_json::json!({
                    "id": model,
                    "object": "model",
                    "owned_by": provider,
                    "discovered": true
                }));
            }
        }
    }
    serde_json::json!({ "object": "list", "data": data })
}

async fn models(
    State(state): State<Arc<AppState>>,
    auth_context: Option<Extension<AuthKeyContext>>,
) -> impl IntoResponse {
    let cache = state.models_cache.read().await;
    let mut model_list = build_model_list(&state.cfg, &cache);
    if let Some(Extension(context)) = auth_context {
        if let Some(allowed) = context.provider_allowlist {
            if let Some(models) = model_list
                .get_mut("data")
                .and_then(serde_json::Value::as_array_mut)
            {
                models.retain(|model| {
                    model
                        .get("owned_by")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|provider| allowed.iter().any(|item| item == provider))
                });
            }
        }
    }
    Json(model_list)
}

/// Conversation IDs are echoed into response IDs and tab affinity. Restrict
/// them to bounded ASCII so formatting and byte slicing cannot panic on a
/// malicious multibyte header value.
fn conversation_context_read_error(conversation_id: Option<&str>) -> Response {
    tracing::error!(conversation_id = ?conversation_id, "conversation context read failed");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "memory_read_failed",
        "conversation history could not be read safely",
    )
}

#[cfg(test)]
fn read_conversation_context(
    memory: &UnifiedMemory,
    conversation_id: Option<&str>,
    messages: &[ChatMessage],
) -> anyhow::Result<(String, Vec<ChatMessage>, bool)> {
    read_conversation_context_for_owner(memory, "local", conversation_id, messages)
}

#[cfg(test)]
fn read_conversation_context_for_owner(
    memory: &UnifiedMemory,
    owner_key_id: &str,
    conversation_id: Option<&str>,
    messages: &[ChatMessage],
) -> anyhow::Result<(String, Vec<ChatMessage>, bool)> {
    memory.build_context_for_owner_with_history(owner_key_id, conversation_id, messages)
}

fn read_conversation_context_with_transcript_for_owner(
    memory: &UnifiedMemory,
    owner_key_id: &str,
    conversation_id: Option<&str>,
    messages: &[ChatMessage],
) -> anyhow::Result<(String, Vec<ChatMessage>, bool, Vec<ChatMessage>)> {
    memory.build_context_for_owner_with_transcript(owner_key_id, conversation_id, messages)
}

fn usable_initial_stream_chunk(content: &str, done: bool) -> bool {
    !done && !content.is_empty()
}

enum FirstEvent {
    Chunk(providers::ProviderChunk),
    EmptyCompletion,
    Error(anyhow::Error),
    Closed,
    Timeout,
}

fn record_attempt_error(
    provider: &str,
    error: &str,
    queue_ms: u64,
    provider_started: Instant,
    request_started: Instant,
) {
    let provider_work_ms = provider_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let total_ms = request_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    provider_state::record_error(provider, error);
    metrics::record_provider_timings(
        provider,
        "error",
        queue_ms,
        None,
        provider_work_ms,
        total_ms,
    );
}

async fn first_event(
    rx: &mut tokio::sync::mpsc::Receiver<anyhow::Result<providers::ProviderChunk>>,
    timeout: std::time::Duration,
) -> FirstEvent {
    match tokio::time::timeout(timeout, async {
        loop {
            match rx.recv().await {
                Some(Ok(chunk)) if usable_initial_stream_chunk(&chunk.content, chunk.done) => {
                    break Some(Ok(chunk));
                }
                Some(Ok(chunk)) if chunk.done => break Some(Ok(chunk)),
                Some(Ok(_empty_chunk)) => continue,
                Some(Err(error)) => break Some(Err(error)),
                None => break None,
            }
        }
    })
    .await
    {
        Ok(Some(Ok(chunk))) if usable_initial_stream_chunk(&chunk.content, chunk.done) => {
            FirstEvent::Chunk(chunk)
        }
        Ok(Some(Ok(_))) => FirstEvent::EmptyCompletion,
        Ok(Some(Err(error))) => FirstEvent::Error(error),
        Ok(None) => FirstEvent::Closed,
        Err(_) => FirstEvent::Timeout,
    }
}

fn valid_conversation_id(value: &str) -> bool {
    (8..=128).contains(&value.len())
        && value.is_ascii()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn oversized_chat_body_error() -> Response {
    api_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "invalid_request_error",
        "request_too_large",
        "request body exceeds the 1 MiB limit",
    )
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    auth_context: Option<Extension<AuthKeyContext>>,
    body: Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let request_id = headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_owned();
    let request_started = Instant::now();
    let body = match body {
        Ok(body) => body,
        Err(_) => {
            // Bytes extraction happens before the handler body runs. Catch the
            // rejection explicitly so oversized payloads keep the public API
            // error envelope instead of Axum's default rejection body.
            return oversized_chat_body_error();
        }
    };
    let req = match parse_and_validate_chat_req(&body) {
        Ok(req) => req,
        Err(response) => return response,
    };

    let owner_key_id = auth_context
        .as_ref()
        .map(|Extension(context)| context.key_id.as_str())
        .unwrap_or("local")
        .to_owned();

    let conversation_id = match headers.get("X-Conversation-Id") {
        None => None,
        Some(value) => {
            let value = match value.to_str() {
                Ok(value) => value,
                Err(_) => {
                    return api_error(
                        axum::http::StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        "invalid_conversation_id",
                        "X-Conversation-Id must be a valid ASCII identifier",
                    )
                }
            };
            if !valid_conversation_id(value) {
                return api_error(
                    axum::http::StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "invalid_conversation_id",
                    "X-Conversation-Id must be 8-128 ASCII letters, digits, '_' or '-'",
                );
            }
            Some(value.to_string())
        }
    };

    // Read all context-dependent memory before any provider request. A read
    // failure must not degrade into an empty prompt or an unauthenticated turn.
    let (context_prompt, windowed, has_history, stored_messages) =
        match read_conversation_context_with_transcript_for_owner(
            &state.memory,
            &owner_key_id,
            conversation_id.as_deref(),
            &req.messages,
        ) {
            Ok(context) => context,
            Err(_error) => {
                return conversation_context_read_error(conversation_id.as_deref());
            }
        };
    let prompt = context_prompt;
    if prompt.is_empty() {
        return api_error(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "empty_prompt",
            "prompt must not be empty",
        );
    }

    let mut candidates = match state.router.resolve_for_owner(
        &owner_key_id,
        &req.model,
        conversation_id.as_deref(),
        &state.memory,
    ) {
        Ok(candidates) => candidates,
        Err(_error) => {
            tracing::error!(conversation_id = ?conversation_id, "provider affinity read failed");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_read_failed",
                "conversation routing state could not be read safely",
            );
        }
    };
    let original_candidate_count = candidates.len();
    if let Some(Extension(context)) = &auth_context {
        if let Some(allowed) = &context.provider_allowlist {
            candidates.retain(|provider| allowed.iter().any(|item| item == provider));
            if candidates.is_empty() && original_candidate_count > 0 {
                return api_error(
                    StatusCode::FORBIDDEN,
                    "authentication_error",
                    "provider_not_allowed",
                    "the API key is not allowed to use the requested provider",
                );
            }
        }
    }
    let provider_model = req
        .model
        .split_once('/')
        .map(|(_, model)| model)
        .unwrap_or(req.model.as_str())
        .to_string();
    tracing::info!(model = %req.model, ?candidates, conv = ?conversation_id, "routing request");
    if candidates.is_empty() {
        return api_error(
            axum::http::StatusCode::NOT_FOUND,
            "invalid_request_error",
            "model_not_found",
            format!("model '{}' is not configured for an enabled provider; use /v1/models or model='auto'", req.model),
        );
    }

    let candidate_count = candidates.len();
    let mut busy_count = 0usize;
    let mut last_err: Option<String> = None;
    for provider_name in candidates {
        if !provider_state::is_routable(&provider_name) {
            last_err = Some(format!("provider '{provider_name}' needs login"));
            continue;
        }
        if let Some(reason) = provider_guard::provider_disabled_reason(&provider_name) {
            last_err = Some(format!("provider '{provider_name}' disabled: {reason}"));
            continue;
        }
        let provider_cfg = match state.cfg.provider(&provider_name) {
            Some(cfg) => cfg,
            None => {
                last_err = Some(format!(
                    "provider '{provider_name}' configuration disappeared"
                ));
                continue;
            }
        };
        if let Some(reason) = provider_guard::provider_disabled_reason(&provider_name) {
            last_err = Some(format!("provider '{provider_name}' disabled: {reason}"));
            continue;
        }
        // Reserve/wait for the provider's pacing slot before taking its
        // exclusive browser permit. Otherwise a long interval blocks every
        // other caller behind a permit that is not doing useful work.
        if let Err(error) = provider_guard::wait_for_request_slot(
            &provider_name,
            provider_cfg.min_request_interval_ms,
            provider_cfg.daily_request_cap,
        )
        .await
        {
            last_err = Some(error.to_string());
            continue;
        }

        let permit = match state.pool.acquire(&provider_name).await {
            Ok(p) => p,
            Err(e) => {
                if e.downcast_ref::<browser::pool::ProviderBusyError>()
                    .is_some()
                {
                    busy_count += 1;
                }
                last_err = Some(e.to_string());
                continue;
            }
        };
        tracing::debug!(provider = %provider_name, queue_ms = permit.queue_ms, "acquired provider slot");
        let queue_ms = permit.queue_ms.min(u64::MAX as u128) as u64;

        let saved_thread = match conversation_id.as_deref() {
            Some(id) => {
                match state
                    .memory
                    .get_provider_thread_for_owner(&owner_key_id, id, &provider_name)
                {
                    Ok(thread) => thread.filter(|thread| {
                        is_reopenable_provider_thread_url(&provider_name, &thread.thread_url)
                    }),
                    Err(_) => {
                        return api_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "server_error",
                            "memory_read_failed",
                            "provider thread state could not be read safely",
                        )
                    }
                }
            }
            None => None,
        };
        let supports_reopen = providers::ADAPTERS
            .iter()
            .find(|spec| spec.id == provider_name)
            .is_some_and(|spec| matches!(spec.threads, providers::ThreadSupport::UrlAddressable))
            && saved_thread
                .as_ref()
                .is_some_and(|thread| thread.model.as_deref() == Some(req.model.as_str()));
        let last_marker = transcript_marker(&stored_messages);
        let thread_state = saved_thread.as_ref().map(|thread| ThreadSyncState {
            provider: thread.provider.clone(),
            thread_url: thread.thread_url.clone(),
            synced_through_message_id: thread.synced_through_message_id.clone(),
            last_stored_message_id: Some(last_marker),
        });
        let send_plan = ContextBuilder::plan(
            &stored_messages,
            &req.messages,
            &provider_name,
            thread_state.as_ref(),
            supports_reopen,
            8000,
        );
        let (provider_prompt, thread_url) = match send_plan {
            ContextPlan::Delta { messages } => (
                context::render_prompt(&messages),
                saved_thread.map(|thread| thread.thread_url),
            ),
            ContextPlan::Replay { preamble, .. } => (format!("{preamble}\n{prompt}"), None),
        };

        let mut rx = match state
            .pool
            .send_stream(
                &provider_name,
                provider_prompt.clone(),
                provider_model.clone(),
                has_history && thread_url.is_some(),
                thread_url,
            )
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                provider_state::record_error(&provider_name, &e.to_string());
                last_err = Some(e.to_string());
                continue;
            }
        };
        let provider_started = Instant::now();

        let mut first_stream_chunk: Option<providers::ProviderChunk> = None;
        if req.stream {
            let first_chunk_timeout = std::time::Duration::from_secs(
                state
                    .cfg
                    .provider(&provider_name)
                    .map(|provider| provider.timeout_seconds)
                    .unwrap_or(120),
            );
            let first = match first_event(&mut rx, first_chunk_timeout).await {
                FirstEvent::Chunk(chunk) => chunk,
                FirstEvent::EmptyCompletion => {
                    let error = "provider completed without output before stream started";
                    record_attempt_error(
                        &provider_name,
                        error,
                        queue_ms,
                        provider_started,
                        request_started,
                    );
                    last_err = Some(error.to_string());
                    continue;
                }
                FirstEvent::Error(error) => {
                    let message = error.to_string();
                    record_attempt_error(
                        &provider_name,
                        &message,
                        queue_ms,
                        provider_started,
                        request_started,
                    );
                    last_err = Some(message);
                    continue;
                }
                FirstEvent::Closed => {
                    let error = "provider stream closed before first output";
                    record_attempt_error(
                        &provider_name,
                        error,
                        queue_ms,
                        provider_started,
                        request_started,
                    );
                    last_err = Some(error.to_string());
                    continue;
                }
                FirstEvent::Timeout => {
                    let error = "provider timed out before first output";
                    let provider_work_ms =
                        provider_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    let total_ms =
                        request_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    provider_state::record_error(&provider_name, error);
                    metrics::record_provider_timings(
                        &provider_name,
                        "error",
                        queue_ms,
                        None,
                        provider_work_ms,
                        total_ms,
                    );
                    last_err = Some(error.to_string());
                    continue;
                }
            };
            first_stream_chunk = Some(first);
        }

        let new_conv_id = conversation_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        if req.stream {
            let first_chunk =
                first_stream_chunk.expect("streaming path prefetches its first chunk");
            let state_c = state.clone();
            let owner_key_id_c = owner_key_id.clone();
            let windowed_c = windowed.clone();
            let provider_c = provider_name.clone();
            let model_c = req.model.clone();
            let prompt_tokens_c = estimate_tokens(&provider_prompt);
            let provider_model_c = provider_model.clone();
            let conv_stream = new_conv_id.clone();
            let request_started_c = request_started;
            let provider_started_c = provider_started;
            let queue_ms_c = queue_ms;
            let request_id_c = request_id.clone();
            let sse_stream = async_stream::stream! {
                let _permit = permit;
                let mut full = String::new();
                let mut ttft_ms: Option<u64> = None;
                // OpenAI-compatible clients expect the assistant role on the
                // first stream chunk, before content deltas.
                let role = serde_json::json!({
                    "id": format!("chatcmpl-{}", &conv_stream[..8]),
                    "object": "chat.completion.chunk",
                    "created": chrono::Utc::now().timestamp(),
                    "model": model_c,
                    "choices": [{ "delta": { "role": "assistant" }, "index": 0, "finish_reason": null }],
                });
                yield Ok::<_, anyhow::Error>(Event::default().data(role.to_string()));
                let mut stream_error: Option<String> = None;
                // Keep the raw detail only for internal session-state classification;
                // client-facing stream errors remain generic and never expose it.
                let mut provider_error_detail: Option<String> = None;
                let mut pending: Option<Result<providers::ProviderChunk, anyhow::Error>> = Some(Ok(first_chunk));
                while let Some(item) = match pending.take() {
                    Some(item) => Some(item),
                    None => rx.recv().await,
                } {
                    match item {
                        Ok(c) => {
                            if c.done { break; }
                            if ttft_ms.is_none() {
                                ttft_ms = Some(provider_started_c.elapsed().as_millis().min(u64::MAX as u128) as u64);
                            }
                            full.push_str(&c.content);
                            let data = serde_json::json!({
                                "id": format!("chatcmpl-{}", &conv_stream[..8]),
                                "object": "chat.completion.chunk",
                                "created": chrono::Utc::now().timestamp(),
                                "model": model_c,
                                "choices": [{ "delta": { "content": c.content }, "index": 0, "finish_reason": null }],
                            });
                            yield Ok::<_, anyhow::Error>(Event::default().data(data.to_string()));
                        }
                        Err(e) => {
                            tracing::warn!(provider = %provider_c, "provider stream failed: {e}");
                            provider_error_detail = Some(e.to_string());
                            stream_error = Some("provider stream failed; partial response was not committed".to_string());
                            break;
                        }
                    }
                }

                let provider_work_ms = provider_started_c.elapsed().as_millis().min(u64::MAX as u128) as u64;
                let total_ms = request_started_c.elapsed().as_millis().min(u64::MAX as u128) as u64;
                metrics::record_provider_timings(
                    &provider_c,
                    if stream_error.is_some() { "error" } else { "success" },
                    queue_ms_c,
                    ttft_ms,
                    provider_work_ms,
                    total_ms,
                );
                tracing::info!(
                    request_id = %request_id_c,
                    provider = %provider_c,
                    queue_ms = queue_ms_c,
                    ttft_ms = ?ttft_ms,
                    provider_work_ms,
                    total_ms,
                    outcome = if stream_error.is_some() { "error" } else { "success" },
                    "provider request completed"
                );
                if let Some(message) = stream_error {
                    // Preserve sanitized session-expiry state for routing and health reporting.
                    provider_state::record_error(
                        &provider_c,
                        provider_error_detail.as_deref().unwrap_or(&message),
                    );
                    let error = serde_json::json!({
                        "error": { "message": message, "type": "provider_error", "code": "stream_failed" }
                    });
                    yield Ok(Event::default().data(error.to_string()));
                } else {
                    provider_state::record_success(&provider_c);
                    if let Err(_error) = state_c.memory.store_turn_for_owner_with_tab_id(
                        &owner_key_id_c,
                        conv_stream.clone(),
                        windowed_c.clone(),
                        ChatMessage { role: "assistant".to_string(), content: full.clone() },
                        provider_c.clone(),
                        None,
                    ) {
                        tracing::error!(conversation_id = %conv_stream, "conversation persistence failed after stream");
                        let error = serde_json::json!({
                            "error": { "message": "conversation could not be persisted", "type": "server_error", "code": "memory_persistence_failed" }
                        });
                        yield Ok(Event::default().data(error.to_string()));
                        yield Ok(Event::default().data("[DONE]"));
                        return;
                    }
                    persist_provider_thread(&state_c, &owner_key_id_c, &conv_stream, &provider_c, &model_c).await;
                    let completion_tokens = estimate_tokens(&full);
                    let finish = serde_json::json!({
                        "id": format!("chatcmpl-{}", &conv_stream[..8]),
                        "object": "chat.completion.chunk",
                        "created": chrono::Utc::now().timestamp(),
                        "model": model_c,
                        "choices": [{ "delta": {}, "index": 0, "finish_reason": "stop" }],
                        "usage": {
                            "prompt_tokens": prompt_tokens_c,
                            "completion_tokens": completion_tokens,
                            "total_tokens": prompt_tokens_c + completion_tokens
                        },
                        "x_usage_estimated": true,
                    });
                    yield Ok(Event::default().data(finish.to_string()));
                }
                yield Ok(Event::default().data("[DONE]"));
            };
            let mut resp_headers = axum::http::HeaderMap::new();
            resp_headers.insert("X-Conversation-Id", new_conv_id.parse().unwrap());
            resp_headers.insert("Content-Type", "text/event-stream".parse().unwrap());
            resp_headers.insert("Cache-Control", "no-cache".parse().unwrap());
            resp_headers.insert("X-Accel-Buffering", "no".parse().unwrap());
            add_gateway_headers(&mut resp_headers, &provider_name, &provider_model_c);
            return (resp_headers, Sse::new(sse_stream)).into_response();
        }

        // Non-streaming: retry another provider only if no response bytes were
        // emitted. A partial response is an error, never a successful turn.
        let mut full = String::new();
        let mut failed: Option<String> = None;
        let mut ttft_ms: Option<u64> = None;
        while let Some(item) = rx.recv().await {
            match item {
                Ok(c) => {
                    if c.done {
                        break;
                    }
                    if ttft_ms.is_none() {
                        ttft_ms = Some(
                            provider_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                        );
                    }
                    full.push_str(&c.content);
                }
                Err(e) => {
                    failed = Some(e.to_string());
                    break;
                }
            }
        }
        let provider_work_ms = provider_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let total_ms = request_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        if let Some(e) = failed {
            provider_state::record_error(&provider_name, &e);
            metrics::record_provider_timings(
                &provider_name,
                "error",
                queue_ms,
                ttft_ms,
                provider_work_ms,
                total_ms,
            );
            tracing::info!(
                request_id = %request_id,
                provider = %provider_name,
                queue_ms,
                ttft_ms = ?ttft_ms,
                provider_work_ms,
                total_ms,
                outcome = "error",
                "provider request completed"
            );
            if full.is_empty() {
                tracing::warn!(provider = %provider_name, "provider failed before output: {e}");
                last_err = Some(e);
                continue;
            }
            tracing::warn!(provider = %provider_name, "provider failed after partial output");
            return api_error(
                axum::http::StatusCode::BAD_GATEWAY,
                "provider_error",
                "partial_response",
                "provider failed after emitting a partial response; the turn was not committed",
            );
        }

        provider_state::record_success(&provider_name);
        metrics::record_provider_timings(
            &provider_name,
            "success",
            queue_ms,
            ttft_ms,
            provider_work_ms,
            total_ms,
        );
        tracing::info!(
            request_id = %request_id,
            provider = %provider_name,
            queue_ms,
            ttft_ms = ?ttft_ms,
            provider_work_ms,
            total_ms,
            outcome = "success",
            "provider request completed"
        );
        if let Err(_error) = state.memory.store_turn_for_owner_with_tab_id(
            &owner_key_id,
            new_conv_id.clone(),
            windowed.clone(),
            ChatMessage {
                role: "assistant".to_string(),
                content: full.clone(),
            },
            provider_name.clone(),
            None,
        ) {
            tracing::error!(conversation_id = %new_conv_id, "conversation persistence failed");
            return api_error(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_persistence_failed",
                "conversation could not be persisted",
            );
        }

        persist_provider_thread(
            &state,
            &owner_key_id,
            &new_conv_id,
            &provider_name,
            &req.model,
        )
        .await;
        let prompt_tokens = estimate_tokens(&provider_prompt);
        let completion_tokens = estimate_tokens(&full);
        let body = serde_json::json!({
            "id": format!("chatcmpl-{}", &new_conv_id[..8]),
            "object": "chat.completion",
            "created": chrono::Utc::now().timestamp(),
            "model": req.model,
            "choices": [{ "message": { "role": "assistant", "content": full }, "finish_reason": "stop", "index": 0 }],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens
            },
            "x_usage_estimated": true,
            "conversation_id": new_conv_id,
        });
        let mut resp_headers = axum::http::HeaderMap::new();
        resp_headers.insert("X-Conversation-Id", new_conv_id.parse().unwrap());
        add_gateway_headers(&mut resp_headers, &provider_name, &provider_model);
        return (resp_headers, Json(body)).into_response();
    }

    if candidate_count > 0 && busy_count == candidate_count {
        let mut response = api_error(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "server_error",
            "provider_busy",
            "all eligible providers are busy; retry shortly",
        );
        response.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from_static("1"),
        );
        return response;
    }

    api_error(
        axum::http::StatusCode::BAD_GATEWAY,
        "provider_error",
        "provider_unavailable",
        format!("all providers failed: {:?}", last_err),
    )
}

// Returning a ready-to-send Axum response keeps validation errors consistent at the handler boundary.
#[allow(clippy::result_large_err)]
fn parse_and_validate_chat_req(body: &[u8]) -> Result<ChatReq, Response> {
    let req = serde_json::from_slice::<ChatReq>(body).map_err(|error| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_json",
            format!("invalid or unsupported request field: {error}"),
        )
    })?;
    validate_chat_req(&req).map_err(|(code, message)| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            code,
            message,
        )
    })?;
    Ok(req)
}

#[cfg(test)]
mod model_list_tests {
    use super::*;

    fn provider(name: &str, enabled: bool, models: &[&str]) -> config::ProviderConfig {
        config::ProviderConfig {
            name: name.to_owned(),
            enabled,
            priority: 1,
            timeout_seconds: 30,
            queue_timeout_seconds: 30,
            session_pool_size: 1,
            min_request_interval_ms: 5_000,
            daily_request_cap: 100,
            models: models.iter().map(|model| (*model).to_owned()).collect(),
        }
    }

    #[test]
    fn excludes_configured_and_discovered_models_for_disabled_providers() {
        let cfg = AppConfig {
            server: config::ServerConfig::default(),
            memory: config::MemoryConfig::default(),
            obscura: config::ObscuraConfig::default(),
            auth: config::AuthConfig::default(),
            providers: vec![
                provider("chatgpt", true, &["gpt-live"]),
                provider("claude", false, &["claude-disabled"]),
            ],
        };
        let cache = HashMap::from([
            ("chatgpt".to_owned(), vec!["gpt-discovered".to_owned()]),
            ("claude".to_owned(), vec!["claude-cached".to_owned()]),
        ]);

        let value = build_model_list(&cfg, &cache);
        let ids = value["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["gpt-live", "gpt-discovered"]);
    }

    #[test]
    fn configured_ids_win_over_duplicate_discovered_ids() {
        let cfg = AppConfig {
            server: config::ServerConfig::default(),
            memory: config::MemoryConfig::default(),
            obscura: config::ObscuraConfig::default(),
            auth: config::AuthConfig::default(),
            providers: vec![provider("chatgpt", true, &["gpt-live"])],
        };
        let cache = HashMap::from([(
            "chatgpt".to_owned(),
            vec!["gpt-live".to_owned(), "gpt-extra".to_owned()],
        )]);

        let value = build_model_list(&cfg, &cache);
        let data = value["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["owned_by"], "chatgpt");
        assert!(data[0].get("discovered").is_none());
        assert_eq!(data[1]["id"], "gpt-extra");
        assert_eq!(data[1]["discovered"], true);
    }
}

#[cfg(test)]
mod login_origin_tests {
    use super::*;

    #[test]
    fn allows_matching_local_origin_and_host() {
        let mut headers = HeaderMap::new();
        headers.insert("origin", "http://127.0.0.1:8080".parse().unwrap());
        headers.insert("host", "127.0.0.1:8080".parse().unwrap());
        assert!(login_origin_is_same_authority(&headers));
    }

    #[test]
    fn rejects_origin_with_different_authority() {
        let mut headers = HeaderMap::new();
        headers.insert("origin", "https://evil.example".parse().unwrap());
        headers.insert("host", "127.0.0.1:8080".parse().unwrap());
        assert!(!login_origin_is_same_authority(&headers));
    }

    #[test]
    fn rejects_non_loopback_host_authority() {
        assert!(!is_loopback_authority("attacker.example:8080"));
        assert!(!is_loopback_authority("localhost.attacker.example:8080"));
    }

    #[test]
    fn rejects_dns_rebinding_hostname_even_when_origin_matches_host() {
        let mut headers = HeaderMap::new();
        headers.insert("origin", "http://attacker.example:8080".parse().unwrap());
        headers.insert("host", "attacker.example:8080".parse().unwrap());
        assert!(!login_origin_is_same_authority(&headers));
    }

    #[test]
    fn accepts_only_explicit_loopback_authorities() {
        assert!(is_loopback_authority("127.0.0.1:8080"));
        assert!(is_loopback_authority("localhost:8080"));
        assert!(is_loopback_authority("[::1]:8080"));
        assert!(!is_loopback_authority("localhost.attacker.example:8080"));
        assert!(!is_loopback_authority("127.0.0.2:8080"));
        assert!(!is_loopback_authority("localhost:0"));
    }

    #[test]
    fn allows_originless_local_automation_clients() {
        let headers = HeaderMap::new();
        assert!(login_origin_is_same_authority(&headers));
    }
}

#[cfg(test)]
mod api_contract_tests {
    use super::*;

    #[test]
    fn rejects_empty_model_and_messages() {
        let empty_model: ChatReq = serde_json::from_value(serde_json::json!({
            "model": "  ",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
        assert_eq!(
            validate_chat_req(&empty_model).unwrap_err().0,
            "model_required"
        );

        let empty_messages: ChatReq = serde_json::from_value(serde_json::json!({
            "model": "auto",
            "messages": []
        }))
        .unwrap();
        assert_eq!(
            validate_chat_req(&empty_messages).unwrap_err().0,
            "messages_required"
        );
    }

    #[test]
    fn rejects_unsupported_roles_and_temperature() {
        let invalid_role: ChatReq = serde_json::from_value(serde_json::json!({
            "model": "auto",
            "messages": [{"role": "function", "content": "hello"}]
        }))
        .unwrap();
        assert_eq!(
            validate_chat_req(&invalid_role).unwrap_err().0,
            "invalid_message_role"
        );

        let temperature: ChatReq = serde_json::from_value(serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "hello"}],
            "temperature": 0.2
        }))
        .unwrap();
        assert_eq!(
            validate_chat_req(&temperature).unwrap_err().0,
            "unsupported_parameter"
        );
    }

    #[test]
    fn rejects_excessive_message_count_and_content_size() {
        let too_many: ChatReq = serde_json::from_value(serde_json::json!({
            "model": "auto",
            "messages": (0..257).map(|_| serde_json::json!({"role":"user","content":"x"})).collect::<Vec<_>>()
        })).unwrap();
        assert_eq!(
            validate_chat_req(&too_many).unwrap_err().0,
            "too_many_messages"
        );

        let too_large: ChatReq = serde_json::from_value(serde_json::json!({
            "model": "auto",
            "messages": [{"role":"user","content":"x".repeat(128 * 1024 + 1)}]
        }))
        .unwrap();
        assert_eq!(
            validate_chat_req(&too_large).unwrap_err().0,
            "message_too_large"
        );
    }

    #[test]
    fn rejects_total_content_over_limit_even_when_each_message_is_valid() {
        let req: ChatReq = serde_json::from_value(serde_json::json!({
            "model": "auto",
            "messages": [
                {"role":"user","content":"x".repeat(100 * 1024)},
                {"role":"assistant","content":"y".repeat(100 * 1024)},
                {"role":"user","content":"z".repeat(100 * 1024)},
                {"role":"assistant","content":"w".repeat(100 * 1024)},
                {"role":"user","content":"q".repeat(100 * 1024)},
                {"role":"assistant","content":"r".repeat(100 * 1024)}
            ]
        }))
        .unwrap();
        assert_eq!(
            validate_chat_req(&req).unwrap_err().0,
            "request_content_too_large"
        );
    }

    #[test]
    fn accepts_supported_roles_and_no_unsupported_parameters() {
        let req: ChatReq = serde_json::from_value(serde_json::json!({
            "model": "auto",
            "messages": [
                {"role": "system", "content": "be concise"},
                {"role": "developer", "content": "follow policy"},
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"},
                {"role": "tool", "content": "result"}
            ]
        }))
        .unwrap();
        assert!(validate_chat_req(&req).is_ok());
    }

    #[test]
    fn accepts_minimal_openai_chat_request() {
        let parsed: ChatReq = serde_json::from_value(serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .expect("minimal request should deserialize");
        assert_eq!(parsed.model, "auto");
        assert_eq!(parsed.messages.len(), 1);
        assert!(!parsed.stream);
        assert!(parsed.temperature.is_none());
    }

    #[test]
    fn explicitly_rejects_unsupported_stream_options_include_usage() {
        let req = serde_json::from_value::<ChatReq>(serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true,
            "stream_options": {"include_usage": true}
        }))
        .expect("stream_options should deserialize into the unsupported-field map");
        let error = validate_chat_req(&req).unwrap_err();
        assert_eq!(error.0, "unsupported_parameter");
        assert!(error.1.contains("stream_options"));
    }

    #[test]
    fn reports_unknown_request_fields_as_unsupported_parameters() {
        let req = serde_json::from_value::<ChatReq>(serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [],
            "top_p": 0.5
        }))
        .expect("unknown parameters should be captured for validation");
        let error = validate_chat_req(&req).unwrap_err();
        assert_eq!(error.0, "unsupported_parameter");
        assert!(error.1.contains("tools"));
        assert!(error.1.contains("top_p"));
    }

    #[test]
    fn structured_api_errors_have_openai_error_envelope() {
        let response = api_error(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "model_not_found",
            "model does not exist",
        );
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn rejects_malformed_messages_before_provider_execution() {
        let missing_messages = serde_json::from_value::<ChatReq>(serde_json::json!({
            "model": "auto"
        }));
        assert!(missing_messages.is_err());

        let missing_model = serde_json::from_value::<ChatReq>(serde_json::json!({
            "messages": [{"role": "user", "content": "hello"}]
        }));
        assert!(missing_model.is_err());
    }
}

#[cfg(test)]
mod conversation_id_tests {
    use super::*;

    #[test]
    fn accepts_generated_uuid_and_safe_ids() {
        assert!(valid_conversation_id(
            "8c1d8e2a-1e2a-4f7c-93f8-84c5c39a8b12"
        ));
        assert!(valid_conversation_id("conversation_123"));
    }

    #[test]
    fn rejects_short_unicode_and_unsafe_ids() {
        assert!(!valid_conversation_id("short"));
        assert!(!valid_conversation_id("conversation-é"));
        assert!(!valid_conversation_id("conversation/123"));
        assert!(!valid_conversation_id(&"a".repeat(129)));
    }
}

#[cfg(test)]
mod http_api_regression_tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        routing::post,
    };
    use std::{
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };
    use tower::ServiceExt;

    async fn validation_endpoint(
        body: Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>,
    ) -> Response {
        let body = match body {
            Ok(body) => body,
            Err(_) => {
                return api_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "invalid_request_error",
                    "request_too_large",
                    "request body exceeds the 1 MiB limit",
                );
            }
        };
        match parse_and_validate_chat_req(&body) {
            Ok(request) => Json(serde_json::json!({
                "accepted": true,
                "model": request.model,
            }))
            .into_response(),
            Err(response) => response,
        }
    }

    async fn export_endpoint(
        State(memory): State<Arc<UnifiedMemory>>,
        Path(conversation_id): Path<String>,
    ) -> Response {
        export_conversation_response(&memory, &conversation_id)
    }

    async fn export_markdown_endpoint(
        State(memory): State<Arc<UnifiedMemory>>,
        Path(conversation_id): Path<String>,
    ) -> Response {
        export_conversation_markdown_response(&memory, &conversation_id)
    }

    async fn list_endpoint(
        State(memory): State<Arc<UnifiedMemory>>,
        Query(query): Query<ConversationListQuery>,
    ) -> Response {
        let limit = query.limit.unwrap_or(50).clamp(1, 100);
        let offset = query.offset.unwrap_or(0);
        match memory.list_conversations(limit, offset) {
            Ok(conversations) => Json(serde_json::json!({
                "data": conversations,
                "limit": limit,
                "offset": offset,
            }))
            .into_response(),
            Err(_) => api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_persistence_failed",
                "conversations could not be listed safely",
            ),
        }
    }

    async fn delete_endpoint(
        State(memory): State<Arc<UnifiedMemory>>,
        Path(conversation_id): Path<String>,
    ) -> Response {
        delete_conversation_response(&memory, &conversation_id)
    }

    fn test_db(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "chat-gateway-http-{label}-{}.sqlite3",
            uuid::Uuid::new_v4()
        ))
    }

    fn app(memory: Arc<UnifiedMemory>) -> Router {
        Router::new()
            .route(
                "/ready",
                get(|| async { api::health::readiness_response(1, true) }),
            )
            .route("/v1/conversations", get(list_endpoint))
            .route("/v1/chat/completions", post(validation_endpoint))
            .route(
                "/v1/conversations/:conversation_id/export.md",
                get(export_markdown_endpoint),
            )
            .route(
                "/v1/conversations/:conversation_id",
                get(export_endpoint).delete(delete_endpoint),
            )
            .with_state(memory)
            .layer(DefaultBodyLimit::max(1024 * 1024))
            .layer(middleware::from_fn(attach_request_id))
    }

    async fn json_body(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn context_test_endpoint(
        State((memory, provider_calls)): State<(Arc<UnifiedMemory>, Arc<AtomicUsize>)>,
        Json(payload): Json<serde_json::Value>,
    ) -> Response {
        let conversation_id = payload.get("conversation_id").and_then(|v| v.as_str());
        let messages = payload
            .get("messages")
            .cloned()
            .and_then(|value| serde_json::from_value::<Vec<ChatMessage>>(value).ok())
            .unwrap_or_default();
        match read_conversation_context(&memory, conversation_id, &messages) {
            Ok((prompt, _, has_history)) => {
                provider_calls.fetch_add(1, Ordering::SeqCst);
                Json(serde_json::json!({
                    "accepted": true,
                    "prompt": prompt,
                    "has_history": has_history
                }))
                .into_response()
            }
            Err(_) => conversation_context_read_error(conversation_id),
        }
    }

    #[tokio::test]
    async fn http_conversation_list_returns_paged_metadata_without_messages() {
        let memory = Arc::new(UnifiedMemory::new(8000));
        for (id, text) in [("list_http_a", "alpha"), ("list_http_b", "bravo")] {
            memory
                .store_turn(
                    id.to_owned(),
                    vec![ChatMessage {
                        role: "user".into(),
                        content: text.into(),
                    }],
                    ChatMessage {
                        role: "assistant".into(),
                        content: "answer".into(),
                    },
                    "chatgpt".into(),
                    "tab".into(),
                )
                .unwrap();
        }
        let response = app(memory)
            .oneshot(
                Request::builder()
                    .uri("/v1/conversations?limit=1&offset=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["limit"], 1);
        assert_eq!(body["offset"], 1);
        assert_eq!(body["data"].as_array().unwrap().len(), 1);
        assert_eq!(body["data"][0]["id"], "list_http_b");
        assert_eq!(body["data"][0]["message_count"], 2);
        assert!(body["data"][0].get("messages").is_none());
    }

    #[tokio::test]
    async fn http_markdown_export_returns_markdown_content_type_and_messages() {
        let memory = Arc::new(UnifiedMemory::new(8000));
        memory
            .store_turn(
                "conversation_md_export".into(),
                vec![ChatMessage {
                    role: "user".into(),
                    content: "hello markdown".into(),
                }],
                ChatMessage {
                    role: "assistant".into(),
                    content: "answer".into(),
                },
                "chatgpt".into(),
                "tab".into(),
            )
            .unwrap();
        let response = app(memory)
            .oneshot(
                Request::builder()
                    .uri("/v1/conversations/conversation_md_export/export.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"],
            "text/markdown; charset=utf-8"
        );
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("# Conversation conversation_md_export"));
        assert!(body.contains("hello markdown"));
        assert!(body.contains("## 2 — assistant"));
    }

    #[tokio::test]
    async fn http_memory_read_failure_returns_structured_error_without_provider_call() {
        let memory = Arc::new(UnifiedMemory::new(8000));
        memory.drop_conversations_table_for_test().unwrap();
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/v1/context-test", post(context_test_endpoint))
            .with_state((memory, provider_calls.clone()))
            .layer(middleware::from_fn(attach_request_id));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/context-test")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"conversation_id":"conversation_123","messages":[{"role":"user","content":"hello"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(response.headers().contains_key("x-request-id"));
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "memory_read_failed");
        assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn http_readiness_and_request_id_contract() {
        let memory = Arc::new(UnifiedMemory::new(8000));
        let response = app(memory)
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let request_id = response
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(uuid::Uuid::parse_str(request_id).is_ok());
        assert_eq!(json_body(response).await["status"], "ready");

        let response = app(Arc::new(UnifiedMemory::new(8000)))
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .header("host", "127.0.0.1:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.headers().contains_key("x-request-id"));
    }

    #[tokio::test]
    async fn http_chat_validation_returns_structured_openai_errors_without_provider_access() {
        let memory = Arc::new(UnifiedMemory::new(8000));
        let response = app(memory.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model":"auto","messages":[{"role":"user","content":"hi"}],"tools":[]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response.headers().contains_key("x-request-id"));
        let body = json_body(response).await;
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], "unsupported_parameter");

        let response = app(memory)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from("{ malformed"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(response).await["error"]["code"], "invalid_json");
    }

    #[tokio::test]
    async fn oversized_chat_body_keeps_structured_error_and_request_id() {
        let memory = Arc::new(UnifiedMemory::new(8000));
        let oversized = "x".repeat(1024 * 1024 + 1);
        let response = app(memory)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(oversized))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(response.headers().contains_key("x-request-id"));
        let body = json_body(response).await;
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], "request_too_large");
        assert_eq!(
            body["error"]["message"],
            "request body exceeds the 1 MiB limit"
        );
    }

    #[tokio::test]
    async fn http_export_returns_complete_conversation_and_missing_is_not_found() {
        let memory = Arc::new(UnifiedMemory::new(8000));
        memory
            .store_turn(
                "conversation_export_1".into(),
                vec![ChatMessage {
                    role: "user".into(),
                    content: "export prompt".into(),
                }],
                ChatMessage {
                    role: "assistant".into(),
                    content: "export answer".into(),
                },
                "chatgpt".into(),
                "tab-export".into(),
            )
            .unwrap();

        let response = app(memory.clone())
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/conversations/conversation_export_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key("x-request-id"));
        let body = json_body(response).await;
        assert_eq!(body["id"], "conversation_export_1");
        assert_eq!(body["provider"], "chatgpt");
        assert_eq!(body["tab_id"], "tab-export");
        assert_eq!(body["messages"][0]["content"], "export prompt");
        assert_eq!(body["messages"][1]["content"], "export answer");

        let missing = app(memory)
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/conversations/conversation_export_missing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let body = json_body(missing).await;
        assert_eq!(body["error"]["code"], "conversation_not_found");
    }

    #[tokio::test]
    async fn http_delete_is_durable_across_database_restart() {
        let path = test_db("delete");
        let memory = Arc::new(UnifiedMemory::open_path_for_test(&path, None, 8000).unwrap());
        memory
            .store_turn(
                "conversation_http_1".into(),
                vec![ChatMessage {
                    role: "user".into(),
                    content: "private prompt".into(),
                }],
                ChatMessage {
                    role: "assistant".into(),
                    content: "private answer".into(),
                },
                "chatgpt".into(),
                "tab-http".into(),
            )
            .unwrap();
        let response = app(memory.clone())
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/conversations/conversation_http_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(response.headers().contains_key("x-request-id"));
        drop(memory);

        let restored = UnifiedMemory::open_path_for_test(&path, None, 8000).unwrap();
        assert!(!restored.has_history("conversation_http_1").unwrap());
        drop(restored);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-journal"));
    }

    #[tokio::test]
    async fn http_delete_rejects_invalid_and_missing_conversation_ids() {
        let memory = Arc::new(UnifiedMemory::new(8000));
        let response = app(memory.clone())
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/conversations/short")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(response).await["error"]["code"],
            "invalid_conversation_id"
        );

        let response = app(memory)
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/conversations/conversation_missing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_body(response).await["error"]["code"],
            "conversation_not_found"
        );
    }
}

#[cfg(test)]
mod login_security_middleware_http_tests {
    use super::*;
    use axum::{body::Body, routing::get};
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route("/login/api/status", get(|| async { "ok" }))
            .route("/health", get(|| async { "ok" }))
            .layer(middleware::from_fn(protect_login_api))
            .layer(middleware::from_fn(attach_request_id))
    }

    async fn request(uri: &str, headers: &[(&str, &str)]) -> Response {
        let mut builder = Request::builder().uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        app()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn rejects_non_loopback_host_at_http_boundary() {
        let response = request("/login/api/status", &[("host", "attacker.example")]).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(response.headers().contains_key("x-request-id"));
    }

    #[tokio::test]
    async fn rejects_mismatched_origin_at_http_boundary() {
        let response = request(
            "/login/api/status",
            &[
                ("host", "127.0.0.1:8080"),
                ("origin", "https://attacker.example"),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "unsafe_local_request");
    }

    #[tokio::test]
    async fn rejects_cross_site_login_mutation_metadata() {
        let response = request(
            "/login/api/status",
            &[("host", "127.0.0.1:8080"), ("sec-fetch-site", "cross-site")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn allows_matching_loopback_origin_and_originless_local_clients() {
        let matching = request(
            "/login/api/status",
            &[
                ("host", "127.0.0.1:8080"),
                ("origin", "http://127.0.0.1:8080"),
            ],
        )
        .await;
        assert_eq!(matching.status(), StatusCode::OK);

        let originless = request("/login/api/status", &[("host", "localhost:8080")]).await;
        assert_eq!(originless.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rejects_missing_host_and_dns_rebinding_even_when_origin_matches() {
        let missing_host = request("/login/api/status", &[]).await;
        assert_eq!(missing_host.status(), StatusCode::FORBIDDEN);

        let rebinding = request(
            "/login/api/status",
            &[
                ("host", "attacker.example:8080"),
                ("origin", "http://attacker.example:8080"),
            ],
        )
        .await;
        assert_eq!(rebinding.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn protects_every_session_control_action_from_cross_origin_requests() {
        for (method, path) in [
            ("POST", "/login/api/openai/start"),
            ("POST", "/login/api/openai/cancel"),
            ("POST", "/login/api/openai/complete"),
            ("GET", "/login/api/openai/view"),
            ("POST", "/login/api/openai/act"),
        ] {
            let response = app()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("host", "127.0.0.1:8080")
                        .header("origin", "https://attacker.example")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
            assert!(
                response.headers().contains_key("x-request-id"),
                "{method} {path}"
            );
        }
    }

    #[test]
    fn server_listener_is_loopback_only_by_default() {
        assert!(server_bind_addr(8080).ip().is_loopback());
        assert_eq!(server_bind_addr(8080).port(), 8080);
    }
    #[tokio::test]
    async fn host_protection_also_applies_to_non_login_routes() {
        let response = request("/health", &[("host", "attacker.example")]).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}

#[cfg(test)]
mod loopback_authority_edge_tests {
    use super::is_loopback_authority;

    #[test]
    fn accepts_only_valid_loopback_authorities_and_ports() {
        for authority in [
            "localhost",
            "localhost:8080",
            "127.0.0.1",
            "127.0.0.1:8080",
            "[::1]",
            "[::1]:8080",
        ] {
            assert!(
                is_loopback_authority(authority),
                "expected loopback authority to be accepted: {authority}"
            );
        }

        for authority in [
            "localhost:",
            "localhost:0",
            "localhost:65536",
            "localhost:not-a-port",
            "localhost:8080/path",
            "user@localhost:8080",
            "localhost\\\\@attacker.example:8080",
            "[::1",
            "[::1]junk:8080",
            "[::1]:0",
            "[::1]:65536",
            "127.0.0.2:8080",
            "localhost.attacker.example:8080",
        ] {
            assert!(
                !is_loopback_authority(authority),
                "expected unsafe authority to be rejected: {authority}"
            );
        }
    }
}

#[cfg(test)]
mod adapter_diagnostic_tests {
    use super::*;

    fn passing_report() -> browser::pool::TestAdapterReport {
        browser::pool::TestAdapterReport {
            session_ok: true,
            probe: vec![("input".to_string(), Some("#prompt".to_string()))],
            input_found: true,
            send_clicked: true,
            response_token: true,
            errors: Vec::new(),
        }
    }

    #[test]
    fn healthy_report_requires_all_critical_phases() {
        assert!(adapter_report_is_healthy(&passing_report()));

        let mut report = passing_report();
        report.session_ok = false;
        assert!(!adapter_report_is_healthy(&report));

        let mut report = passing_report();
        report.input_found = false;
        assert!(!adapter_report_is_healthy(&report));

        let mut report = passing_report();
        report.response_token = false;
        assert!(!adapter_report_is_healthy(&report));
    }

    #[test]
    fn any_report_error_marks_adapter_unhealthy() {
        let mut report = passing_report();
        report.errors.push("send failed".to_string());
        assert!(!adapter_report_is_healthy(&report));
    }
}

#[cfg(test)]
mod api_key_auth_tests {
    use super::*;
    use crate::config::{ApiKeyConfig, AuthConfig};

    fn auth(scopes: &[&str]) -> AuthConfig {
        AuthConfig {
            keys: vec![ApiKeyConfig {
                name: "test-client".into(),
                secret_hash: hash_api_key("test-secret"),
                scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
                provider_allowlist: Some(vec!["chatgpt".into()]),
            }],
        }
    }

    #[test]
    fn authenticated_owner_id_is_stable_and_does_not_expose_the_secret() {
        let context = authorize_api_request(
            &auth(&["chat"]),
            Some("Bearer test-secret"),
            "/v1/models",
            &axum::http::Method::GET,
        )
        .unwrap()
        .unwrap();
        assert_eq!(context.key_id, "key:test-client");
        assert!(!context.key_id.contains("test-secret"));
    }

    #[test]
    fn authentication_is_optional_only_when_no_keys_are_configured() {
        assert!(authorize_api_request(
            &AuthConfig::default(),
            None,
            "/v1/models",
            &axum::http::Method::GET
        )
        .is_ok());
        assert!(authorize_api_request(
            &AuthConfig::default(),
            None,
            "/health",
            &axum::http::Method::GET
        )
        .is_ok());
        assert_eq!(
            authorize_api_request(
                &auth(&["chat"]),
                None,
                "/v1/models",
                &axum::http::Method::GET
            ),
            Err(AuthFailure::MissingOrInvalid)
        );
    }

    #[test]
    fn bearer_secret_is_verified_by_hash_and_scope() {
        let config = auth(&["chat"]);
        assert_eq!(
            authorize_api_request(
                &config,
                Some("Bearer wrong"),
                "/v1/models",
                &axum::http::Method::GET
            ),
            Err(AuthFailure::MissingOrInvalid)
        );
        assert!(authorize_api_request(
            &config,
            Some("Bearer test-secret"),
            "/v1/models",
            &axum::http::Method::GET
        )
        .is_ok());
        let context = authorize_api_request(
            &config,
            Some("Bearer test-secret"),
            "/v1/models",
            &axum::http::Method::GET,
        )
        .unwrap()
        .unwrap();
        assert_eq!(context.provider_allowlist, Some(vec!["chatgpt".to_owned()]));
        assert_eq!(
            authorize_api_request(
                &config,
                Some("Bearer test-secret"),
                "/login/api/status",
                &axum::http::Method::GET
            ),
            Err(AuthFailure::InsufficientScope)
        );
        assert_eq!(
            authorize_api_request(
                &config,
                Some("Bearer test-secret"),
                "/v1/conversations/conversation_1",
                &axum::http::Method::DELETE
            ),
            Err(AuthFailure::InsufficientScope)
        );
        assert_eq!(
            authorize_api_request(
                &config,
                Some("Bearer test-secret"),
                "/v1/conversations/conversation_1",
                &axum::http::Method::GET
            ),
            Err(AuthFailure::InsufficientScope)
        );
        assert_eq!(
            authorize_api_request(
                &config,
                Some("Bearer test-secret"),
                "/v1/conversations",
                &axum::http::Method::GET
            ),
            Err(AuthFailure::InsufficientScope)
        );
    }

    #[test]
    fn admin_key_can_access_admin_routes_and_health_needs_no_key() {
        let config = auth(&["admin"]);
        assert!(authorize_api_request(
            &config,
            Some("Bearer test-secret"),
            "/login/api/status",
            &axum::http::Method::GET
        )
        .is_ok());
        assert!(authorize_api_request(
            &config,
            Some("Bearer test-secret"),
            "/v1/conversations/conversation_1",
            &axum::http::Method::GET
        )
        .is_ok());
        assert!(authorize_api_request(
            &config,
            Some("Bearer test-secret"),
            "/v1/conversations",
            &axum::http::Method::GET
        )
        .is_ok());
        assert!(authorize_api_request(
            &config,
            Some("Bearer test-secret"),
            "/v1/conversations/conversation_1",
            &axum::http::Method::DELETE
        )
        .is_ok());
        assert!(authorize_api_request(&config, None, "/health", &axum::http::Method::GET).is_ok());
    }

    #[tokio::test]
    async fn http_middleware_rejects_missing_keys_and_enforces_admin_scope() {
        use tower::ServiceExt;

        let app = Router::new()
            .route("/v1/models", get(|| async { StatusCode::OK }))
            .route("/login/api/status", get(|| async { StatusCode::OK }))
            .layer(middleware::from_fn_with_state(
                auth(&["chat"]),
                authenticate_api_key,
            ));

        let missing = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        let chat = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header(axum::http::header::AUTHORIZATION, "Bearer test-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(chat.status(), StatusCode::OK);

        let admin = app
            .oneshot(
                Request::builder()
                    .uri("/login/api/status")
                    .header(axum::http::header::AUTHORIZATION, "Bearer test-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(admin.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn bearer_scheme_is_case_insensitive_but_token_format_is_strict() {
        let config = auth(&["chat"]);
        assert!(authorize_api_request(
            &config,
            Some("bearer test-secret"),
            "/v1/models",
            &axum::http::Method::GET
        )
        .is_ok());
        assert_eq!(
            authorize_api_request(
                &config,
                Some("Bearer test-secret extra"),
                "/v1/models",
                &axum::http::Method::GET
            ),
            Err(AuthFailure::MissingOrInvalid)
        );
    }
}

#[cfg(test)]
mod provider_thread_url_tests {
    use super::{is_reopenable_provider_thread_url, valid_provider_thread_url};

    #[test]
    fn provider_thread_urls_require_https_and_provider_owned_hosts() {
        assert!(valid_provider_thread_url(
            "chatgpt",
            "https://chatgpt.com/c/123"
        ));
        assert!(valid_provider_thread_url(
            "claude",
            "https://claude.ai/chat/123"
        ));
        assert!(is_reopenable_provider_thread_url(
            "chatgpt",
            "https://chatgpt.com/c/123"
        ));
        assert!(!is_reopenable_provider_thread_url(
            "chatgpt",
            "https://chatgpt.com/"
        ));
        assert!(!is_reopenable_provider_thread_url(
            "claude",
            "https://claude.ai/"
        ));
        assert!(!valid_provider_thread_url(
            "chatgpt",
            "http://chatgpt.com/c/123"
        ));
        assert!(!valid_provider_thread_url(
            "chatgpt",
            "https://chatgpt.com.attacker.invalid/c/123"
        ));
        assert!(!valid_provider_thread_url(
            "chatgpt",
            "https://attacker.invalid/c/123"
        ));
        assert!(!valid_provider_thread_url(
            "chatgpt",
            "https://user@chatgpt.com/c/123"
        ));
        assert!(!valid_provider_thread_url(
            "unknown",
            "https://chatgpt.com/c/123"
        ));
        assert!(!valid_provider_thread_url(
            "chatgpt",
            "https://chatgpt.com:8443/c/123"
        ));
    }
}

#[cfg(test)]
mod initial_stream_chunk_tests {
    use super::usable_initial_stream_chunk;

    #[test]
    fn only_nonempty_content_can_start_a_stream() {
        assert!(usable_initial_stream_chunk("first token", false));
        assert!(!usable_initial_stream_chunk("", false));
        assert!(!usable_initial_stream_chunk("", true));
        assert!(!usable_initial_stream_chunk("content marked done", true));
    }
}

#[cfg(test)]
mod legacy_login_relay_flag_tests {
    use super::legacy_login_relay_enabled_from;

    #[test]
    fn legacy_login_relay_is_disabled_by_default() {
        assert!(!legacy_login_relay_enabled_from(None));
        assert!(!legacy_login_relay_enabled_from(Some("")));
        assert!(!legacy_login_relay_enabled_from(Some("false")));
        assert!(!legacy_login_relay_enabled_from(Some("0")));
    }

    #[test]
    fn legacy_login_relay_requires_explicit_truthy_opt_in() {
        assert!(legacy_login_relay_enabled_from(Some("1")));
        assert!(legacy_login_relay_enabled_from(Some("true")));
        assert!(legacy_login_relay_enabled_from(Some(" YES ")));
    }
}

#[cfg(test)]
mod usage_estimate_tests {
    use super::{add_gateway_headers, estimate_tokens};

    #[test]
    fn token_estimate_is_stable_and_handles_empty_text() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn provider_headers_identify_the_actual_route() {
        let mut headers = axum::http::HeaderMap::new();
        add_gateway_headers(&mut headers, "claude", "claude-sonnet");
        assert_eq!(headers["X-Gateway-Provider"], "claude");
        assert_eq!(headers["X-Gateway-Model"], "claude-sonnet");
    }

    #[test]
    fn provider_headers_reject_invalid_header_values() {
        let mut headers = axum::http::HeaderMap::new();
        add_gateway_headers(&mut headers, "claude\r\nX-Injected: yes", "model");
        assert!(!headers.contains_key("X-Gateway-Provider"));
        assert_eq!(headers["X-Gateway-Model"], "model");
    }
}

#[cfg(test)]
mod openai_compatibility_metadata_tests {
    use super::{add_gateway_headers, estimate_tokens};

    #[test]
    fn estimated_token_count_is_nonzero_for_nonempty_unicode_text() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("hello"), 2);
        assert_eq!(estimate_tokens("你好"), 2);
    }

    #[test]
    fn gateway_headers_report_selected_provider_and_model() {
        let mut headers = axum::http::HeaderMap::new();
        add_gateway_headers(&mut headers, "chatgpt", "gpt-4o");
        assert_eq!(headers.get("X-Gateway-Provider").unwrap(), "chatgpt");
        assert_eq!(headers.get("X-Gateway-Model").unwrap(), "gpt-4o");
    }

    #[test]
    fn invalid_gateway_header_values_are_omitted() {
        let mut headers = axum::http::HeaderMap::new();
        add_gateway_headers(&mut headers, "bad\nprovider", "bad\nmodel");
        assert!(!headers.contains_key("X-Gateway-Provider"));
        assert!(!headers.contains_key("X-Gateway-Model"));
    }
}
#[cfg(test)]
mod first_event_tests {
    use super::{first_event, record_attempt_error, FirstEvent};
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn first_event_surfaces_session_expiry_before_any_chunk() {
        let provider = "test-first-event-session-expiry";
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(Err(anyhow::anyhow!("session_expired: please sign in")))
            .await
            .unwrap();
        drop(tx);

        let event = first_event(&mut rx, Duration::from_millis(100)).await;
        let FirstEvent::Error(error) = event else {
            panic!("expected first-event error");
        };
        record_attempt_error(
            provider,
            &error.to_string(),
            0,
            Instant::now(),
            Instant::now(),
        );

        assert_eq!(
            crate::provider_state::status(provider).state,
            crate::provider_state::ProviderSessionState::NeedsLogin
        );
        let metrics = crate::metrics::render_with_provider_states(&[]);
        assert!(metrics.contains(&format!(
            "gateway_provider_requests_total{{provider=\"{provider}\",outcome=\"error\"}} 1"
        )));
    }

    #[tokio::test]
    async fn first_event_distinguishes_closed_and_timed_out_streams() {
        let (tx, mut closed_rx) = tokio::sync::mpsc::channel(1);
        drop(tx);
        assert!(matches!(
            first_event(&mut closed_rx, Duration::from_millis(100)).await,
            FirstEvent::Closed
        ));

        let (_tx, mut idle_rx) = tokio::sync::mpsc::channel(1);
        assert!(matches!(
            first_event(&mut idle_rx, Duration::from_millis(1)).await,
            FirstEvent::Timeout
        ));
    }
}
