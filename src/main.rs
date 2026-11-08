use axum::{
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Extension, Path, Query, State},
    http::HeaderMap,
    http::{header, Request, StatusCode},
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
mod app;
mod app_state;
mod browser;
pub mod chat_backend;
mod chat_handler;
mod chat_render;
mod chat_request;
mod chat_service;
mod cli;
mod config;
mod context;
mod diagnostics;
mod embedded_login;
mod login;
mod memory;
mod memory_api;
mod memory_egress;
mod metrics;
mod provider_guard;
mod provider_state;
mod providers;
mod relay;
mod request_security;
mod router;
mod session;

#[cfg(test)]
use api::add_gateway_headers;
use api::{api_error, estimate_tokens, validate_chat_req, ChatReq};
use app::shutdown_signal;
use app_state::{AppState, AuthKeyContext, ProviderEligibilityMode};
use browser::pool::ObscuraPool;
use chat_backend::{BackendRequest, ProviderId};
#[cfg(test)]
use chat_service::{completed_chat_outcome, FirstEvent};
use chat_service::{first_event, record_attempt_error, ChatService};
use cli::Cli;
use config::AppConfig;
use memory::{ChatMessage, ConversationStore, ProviderThread, UnifiedMemory};
use request_security::{
    local_request_authority_is_safe, login_origin_is_same_authority, remote_gateway_origin,
    remote_request_is_allowed, validate_remote_gateway_config, RemoteProxyTrust,
};
use router::ProviderRouter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let remote_proxy_trust = validate_remote_gateway_config()?;
    let cli = Cli::parse();
    if cli.show_memory_egress_policy {
        let policy = memory_egress::MemoryEgressPolicy::from_env()
            .map_err(|_| anyhow::anyhow!("provider memory egress policy is invalid"))?;
        let allowed = policy.allowed_provider_ids();
        println!("personal_long_term_memory_default=deny");
        for provider in providers::Provider::all() {
            let provider_id = provider.name();
            let decision = if allowed.iter().any(|entry| entry == provider_id) {
                "allow"
            } else {
                "deny"
            };
            println!("{provider_id}={decision}");
        }
        return Ok(());
    }
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
    session::validate_encrypted_store_configuration()?;
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
            let healthy = errors.is_empty()
                && report
                    .as_ref()
                    .is_some_and(diagnostics::adapter_report_is_healthy);

            results.push(diagnostics::AdapterDiagnostic {
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

    let (relay_state, legacy_pairing_code) = relay::RelayState::new()?;
    if remote_gateway_origin()?.is_some() {
        println!("Private remote relay pairing code (single use, expires in 10 minutes): {legacy_pairing_code}");
    } else {
        println!("Browser relay: connect from the extension and approve locally.");
    }
    let state = Arc::new(AppState {
        cfg: cfg.clone(),
        pool: pool.clone(),
        chat_service: ChatService::new(pool.clone()),
        memory: Arc::new(UnifiedMemory::from_env(cfg.memory.max_tokens)?),
        router: ProviderRouter::new(cfg.clone()),
        models_cache: Arc::new(RwLock::new(HashMap::new())),
        relay: Arc::new(relay_state),
        embedded_login: Arc::new(embedded_login::EmbeddedLoginRuntime::default()),
        provider_guard: pool.provider_guard(),
        provider_sessions: pool.provider_sessions(),
        metrics: pool.metrics(),
    });

    app::start_background_workers(&state);

    let app = Router::new()
        .route("/health", get(api::health::health))
        .route("/ready", get(api::health::readiness))
        .route("/metrics", get(api::ops::metrics_endpoint))
        .route(
            "/v1/admin/memory-egress-policy",
            get(api::ops::memory_egress_policy_status),
        )
        .route(
            "/v1/admin/providers/:provider/reset",
            post(reset_provider_for_retry),
        )
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/conversations", get(list_conversations))
        .route(
            "/v1/memory/settings",
            get(memory_api::get_long_term_memory_settings)
                .put(memory_api::put_long_term_memory_settings),
        )
        .route(
            "/v1/memory/items",
            get(memory_api::list_long_term_memory_items)
                .put(memory_api::save_long_term_memory_item)
                .delete(memory_api::delete_all_long_term_memory_items),
        )
        .route(
            "/v1/memory/export",
            get(memory_api::export_long_term_memory_items),
        )
        .route(
            "/v1/memory/items/:item_id",
            axum::routing::delete(memory_api::delete_long_term_memory_item),
        )
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
        .route("/login/browser.js", get(embedded_login::javascript))
        .route(
            "/login/api/:provider/browser/ticket",
            post(embedded_login::ticket),
        )
        .route(
            "/login/api/:provider/browser/session",
            axum::routing::delete(embedded_login::revoke),
        )
        .route(
            "/login/api/:provider/browser/stream",
            get(embedded_login::stream),
        )
        .route("/login/api/status", get(login_status))
        .route("/login/api/:provider/start", post(login_start))
        .route("/login/api/:provider/cancel", post(login_cancel))
        .route("/login/api/:provider/complete", post(login_complete))
        .route("/login/api/:provider/view", get(login_view))
        .route("/login/api/:provider/act", post(login_act))
        .route("/relay/v1/pair", post(relay::pair))
        .route("/relay/v1/connect/request", post(relay::connect_request))
        .route("/relay/v1/connect/poll", post(relay::connect_poll))
        .route(
            "/relay/approve/:id",
            get(relay::approval_page).post(relay::approve_connection),
        )
        // Local operator administration, not a provider or extension endpoint.
        .route(
            "/relay/v1/pairing-code/rotate",
            post(relay::rotate_pairing_code),
        )
        .route("/relay/v1/status", get(relay::status).post(relay::status))
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
        .layer(Extension(remote_proxy_trust))
        // Keep request IDs on both successful responses and middleware rejections.
        .layer(middleware::from_fn(attach_request_id))
        .layer(Extension(state.metrics.clone()));

    // Security default: the gateway exposes browser/session control routes and must not listen on public interfaces by default.
    // Remote access should be added only with explicit authentication and TLS.
    // Docker mode is an explicit opt-in used only by the local run script,
    // which publishes this port on host loopback (127.0.0.1). The default
    // process bind remains loopback-only.
    let docker_local_bind =
        std::env::var("CHAT_GATEWAY_DOCKER_LOCAL_BIND").is_ok_and(|value| value == "1");
    let addr = server_bind_addr(cfg.server.port, docker_local_bind);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("Chat Gateway v2 Obscura Native listening on {}", addr);
    // Bound graceful draining so a stuck stream cannot prevent shutdown forever.
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let state_for_shutdown = state.clone();
    let shutdown = async move {
        shutdown_signal(state_for_shutdown).await;
        let _ = shutdown_tx.send(());
    };
    let mut server = Box::pin(std::future::IntoFuture::into_future(
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown),
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
    state.memory.clone().checkpoint_blocking().await?;
    drop(state);
    Ok(())
}

/// Login delegate UI (see src/login.rs): relay-based provider login.
/// The gateway never presents its own credential form and never stores
/// credentials — relayed text lives in memory only for the action that
/// types it, and only cookies/localStorage are persisted on Complete.
/// The login relay can read and persist provider session material. Keep the HTTP
/// listener loopback-only by default; remote binding requires validated opt-in configuration.
fn server_bind_addr(port: u16, docker_local_bind: bool) -> SocketAddr {
    if docker_local_bind || std::env::var("CHAT_GATEWAY_REMOTE_ORIGIN").is_ok() {
        SocketAddr::from(([0, 0, 0, 0], port))
    } else {
        SocketAddr::from(([127, 0, 0, 1], port))
    }
}

/// Protect browser-control endpoints against cross-origin requests (CSRF).
/// Non-browser clients without an Origin header remain usable, while browser
/// requests must have an Origin authority identical to the request Host.
async fn protect_login_api(req: Request<Body>, next: Next) -> Response {
    protect_login_api_for_origin(req, next, remote_gateway_origin().ok().flatten()).await
}

async fn protect_login_api_for_origin(
    mut req: Request<Body>,
    next: Next,
    remote_origin: Option<(String, String)>,
) -> Response {
    let headers = req.headers();
    let path = req.uri().path();
    let is_login_control = path == "/login" || path.starts_with("/login/api/");
    let is_login_api = path.starts_with("/login/api/");
    let is_relay_api = path.starts_with("/relay/v1/");
    let is_pairing_admin = path == "/relay/v1/pairing-code/rotate";
    let origin_present = headers.contains_key(axum::http::header::ORIGIN);
    let fetch_site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());

    let origin_is_safe = if is_pairing_admin {
        !origin_present && remote_origin.is_none()
    } else if is_relay_api {
        relay::relay_request_origin_is_allowed(path, headers)
    } else if let Some((expected_origin, _)) = remote_origin.as_ref() {
        !origin_present
            || headers
                .get(axum::http::header::ORIGIN)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|origin| origin == expected_origin.as_str())
    } else {
        !origin_present || login_origin_is_same_authority(headers)
    };

    let authority_is_safe = if let Some((origin, authority)) = remote_origin.as_ref() {
        req.extensions()
            .get::<Option<RemoteProxyTrust>>()
            .and_then(Option::as_ref)
            .is_some_and(|trust| {
                trust.allows(
                    headers,
                    req.extensions()
                        .get::<ConnectInfo<SocketAddr>>()
                        .map(|peer| peer.0),
                )
            })
            && remote_request_is_allowed(
                headers,
                origin,
                authority,
                is_relay_api && relay::relay_request_origin_is_allowed(path, headers),
            )
    } else {
        local_request_authority_is_safe(headers)
    };

    if !authority_is_safe
        || !origin_is_safe
        || (is_login_api && fetch_site == Some("cross-site"))
        || (remote_origin.is_some() && is_login_control)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": {
                    "message": "request authority or origin rejected",
                    "type": "security_error",
                    "code": if remote_origin.is_some() { "unsafe_gateway_request" } else { "unsafe_local_request" }
                }
            })),
        )
            .into_response();
    }

    let allowed_relay_origin =
        if is_relay_api && relay::relay_request_origin_is_allowed(path, headers) {
            headers.get(axum::http::header::ORIGIN).cloned()
        } else {
            None
        };
    // Consume the edge credential at the boundary, before handlers/logging.
    req.headers_mut().remove("x-chat-gateway-proxy-key");
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

async fn login_page() -> impl IntoResponse {
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'self'; base-uri 'none'; form-action 'none'",
            ),
        ],
        Html(login::LOGIN_HTML),
    )
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
            state.provider_sessions.record_success(&provider);
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
/// Return the effective memory-egress policy without exposing memory values.
/// A manual operator retry releases challenge/session circuit breakers but does
/// not assert that a provider login is healthy. Requires a configured admin key.
async fn reset_provider_for_retry(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(provider): axum::extract::Path<String>,
) -> Response {
    if state.cfg.provider(&provider).is_none() {
        return api_error(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "provider_not_found",
            "provider does not exist",
        );
    }
    if state.provider_guard.reset_challenge(&provider).is_err()
        || state.provider_sessions.reset_for_retry(&provider).is_err()
    {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "provider_reset_failed",
            "provider state could not be reset safely",
        );
    }
    tracing::info!(provider = %provider, "operator reset provider for retry");
    Json(serde_json::json!({
        "provider": provider,
        "session_state": "unknown",
        "challenge_disabled": false,
    }))
    .into_response()
}

/// Add a gateway-generated request ID to every response, including security
/// middleware rejections, so callers can correlate failures with server logs
/// without trusting arbitrary client-supplied IDs.
async fn attach_request_id(mut req: Request<Body>, next: Next) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    let method = req.method().to_string();
    let route = metrics::normalize_route(req.uri().path());
    let metrics_registry = req.extensions().get::<metrics::MetricsRegistry>().cloned();
    let is_chat_completion = req.uri().path() == "/v1/chat/completions";
    if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
        req.headers_mut().insert("x-request-id", value);
    }
    let started = Instant::now();
    let mut response = next.run(req).await;
    let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    if let Some(registry) = metrics_registry {
        registry.record_http_request(&route, response.status().as_u16(), duration_ms);
    }
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
    if path.starts_with("/relay/v1/") || path.starts_with("/relay/approve/") {
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
        || path.starts_with("/v1/admin/")
        || path.starts_with("/v1/memory/")
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
    if (req.uri().path().starts_with("/login/api/")
        || req.uri().path().starts_with("/v1/admin/providers/"))
        && auth.keys.is_empty()
    {
        return api_error(
            StatusCode::FORBIDDEN,
            "authentication_error",
            "owner_authorization_required",
            "provider and browser controls require a configured admin API key",
        );
    }
    // The shell contains no private data. Browser WebSockets cannot set a
    // Bearer header: their handler instead consumes an admin-issued, expiring,
    // provider-bound, single-use capability, and requires a same-origin Origin.
    // protect_login_api still enforces local authority and rejects remote mode.
    if embedded_login::uses_separate_authorization(req.method(), req.uri().path()) {
        return next.run(req).await;
    }
    // An intermediary may select a different value when Authorization is
    // repeated. Never authenticate against an ambiguous header set, even
    // when both values happen to be identical.
    if req
        .headers()
        .get_all(axum::http::header::AUTHORIZATION)
        .iter()
        .count()
        > 1
    {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid_api_key",
            "exactly one Bearer API key header is permitted",
        );
    }
    let authorization = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let path = req.uri().path();
    if remote_gateway_origin().ok().flatten().is_some()
        && path != "/health"
        && !path.starts_with("/relay/v1/")
    {
        let expected = std::env::var("CHAT_GATEWAY_API_KEY").unwrap_or_default();
        let supplied = authorization
            .and_then(|header| header.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, secret)| hash_api_key(secret));
        let expected_hash = hash_api_key(&expected);
        if supplied
            .as_deref()
            .is_none_or(|digest| !constant_time_eq(digest.as_bytes(), expected_hash.as_bytes()))
        {
            return api_error(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "invalid_api_key",
                "a valid remote gateway Bearer credential is required",
            );
        }
    }
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
mod embedded_owner_auth_tests {
    use super::*;
    use tower::ServiceExt;

    #[tokio::test]
    async fn operator_provider_reset_is_forbidden_without_admin_key_configuration() {
        let app = Router::new()
            .route(
                "/v1/admin/providers/:provider/reset",
                post(|| async { StatusCode::OK }),
            )
            .layer(middleware::from_fn_with_state(
                config::AuthConfig::default(),
                authenticate_api_key,
            ));
        let request = Request::builder()
            .method(axum::http::Method::POST)
            .uri("/v1/admin/providers/chatgpt/reset")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN,
        );
    }

    #[tokio::test]
    async fn duplicate_api_authorization_is_rejected_before_auth_policy() {
        let app = Router::new()
            .route("/health", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                config::AuthConfig::default(),
                authenticate_api_key,
            ));

        let mut ambiguous = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        ambiguous.headers_mut().append(
            header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer synthetic-token"),
        );
        ambiguous.headers_mut().append(
            header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer synthetic-token"),
        );
        assert_eq!(
            app.clone().oneshot(ambiguous).await.unwrap().status(),
            StatusCode::UNAUTHORIZED,
        );

        // No other authentication behavior changes for a single header.
        let mut unambiguous = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        unambiguous.headers_mut().insert(
            header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer synthetic-token"),
        );
        assert_eq!(
            app.oneshot(unambiguous).await.unwrap().status(),
            StatusCode::OK,
        );
    }

    #[tokio::test]
    async fn anonymous_mode_cannot_control_either_login_backend() {
        let app = Router::new()
            .route("/login", get(|| async { "public shell" }))
            .route("/login/api/status", get(|| async { "private" }))
            .route(
                "/login/api/chatgpt/browser/stream",
                get(|| async { "private" }),
            )
            .layer(middleware::from_fn_with_state(
                config::AuthConfig::default(),
                authenticate_api_key,
            ));
        for path in ["/login/api/status", "/login/api/chatgpt/browser/stream"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        let shell = app
            .oneshot(
                Request::builder()
                    .uri("/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(shell.status(), StatusCode::OK);
    }
}

#[cfg(test)]
fn delete_conversation_response(memory: &UnifiedMemory, conversation_id: &str) -> Response {
    delete_conversation_response_for_owner(memory, "local", conversation_id)
}

#[cfg(test)]
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

#[cfg(test)]
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
        .clone()
        .list_conversations_async(owner_key_id.to_owned(), limit, offset)
        .await
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
        .clone()
        .get_provider_thread_async(
            owner_key_id.to_owned(),
            conversation_id.clone(),
            provider.clone(),
        )
        .await
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
        .clone()
        .upsert_provider_thread_async(owner_key_id.to_owned(), thread.clone())
        .await
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
    memory::transcript_marker(messages)
}

async fn persist_provider_thread(
    state: &AppState,
    owner_key_id: &str,
    conversation_id: &str,
    provider: &str,
    model: &str,
    synced_marker: String,
) {
    let thread_url = match state.pool.current_url(provider).await {
        Ok(url) if is_reopenable_provider_thread_url(provider, &url) => url,
        Ok(_) => return,
        Err(error) => {
            tracing::debug!(provider, %error, "provider thread URL unavailable after successful send");
            return;
        }
    };
    let thread = ProviderThread {
        conversation_id: conversation_id.to_owned(),
        provider: provider.to_owned(),
        thread_url,
        synced_through_message_id: Some(synced_marker),
        model: Some(model.to_owned()),
        updated_at: chrono::Utc::now().timestamp(),
    };
    if let Err(error) = state
        .memory
        .clone()
        .upsert_provider_thread_async(owner_key_id.to_owned(), thread)
        .await
    {
        tracing::warn!(provider, %error, "provider thread mapping could not be persisted");
    }
}

async fn persist_completed_turn(
    state: &AppState,
    owner_key_id: &str,
    conversation_id: &str,
    user_messages: Vec<ChatMessage>,
    assistant_content: String,
    provider: &str,
    model: &str,
) -> anyhow::Result<()> {
    let synced_marker = state
        .memory
        .clone()
        .persist_turn(
            owner_key_id.to_owned(),
            conversation_id.to_owned(),
            user_messages,
            ChatMessage {
                role: "assistant".to_owned(),
                content: assistant_content,
            },
            provider.to_owned(),
            None,
        )
        .await?;
    persist_provider_thread(
        state,
        owner_key_id,
        conversation_id,
        provider,
        model,
        synced_marker,
    )
    .await;
    Ok(())
}

fn is_reopenable_provider_thread_url(provider: &str, value: &str) -> bool {
    let Some(provider_kind) = providers::Provider::from_name(provider) else {
        return false;
    };
    if !providers::supports_url_addressable_threads(provider_kind)
        || !valid_provider_thread_url(provider, value)
    {
        return false;
    }
    let Some((_, location)) = value["https://".len()..].split_once('/') else {
        return false;
    };
    let location = format!("/{location}");
    // A generic prefix like /c/ is not a remote conversation identity.
    // Treat only an exact, nonempty ID segment as safely reopenable; a URL
    // without an ID may redirect back into another warm provider thread.
    let path = location
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');
    // A persisted URL is untrusted storage input. Do not treat dot segments,
    // encoded separators or encoded query delimiters as a provider thread ID:
    // browser URL normalization could navigate somewhere other than the
    // expected conversation while Delta-mode assumes the prior thread.
    let valid_thread_segment = |id: &str| {
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    };
    let has_thread_id = |prefix: &str| path.strip_prefix(prefix).is_some_and(valid_thread_segment);
    match provider {
        "chatgpt" => {
            has_thread_id("/c/")
                || path
                    .strip_prefix("/g/")
                    .and_then(|path| path.split_once("/c/"))
                    .is_some_and(|(gpt, thread)| {
                        valid_thread_segment(gpt) && valid_thread_segment(thread)
                    })
        }
        "claude" => has_thread_id("/chat/"),
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
    if !valid_conversation_id(&conversation_id) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_conversation_id",
            "conversation ID must be 8-128 ASCII letters, digits, '_' or '-'",
        );
    }
    match state
        .memory
        .clone()
        .export_conversation_async(owner_key_id.to_owned(), conversation_id.clone())
        .await
    {
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

async fn export_conversation_markdown(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<String>,
    auth_context: Option<Extension<AuthKeyContext>>,
) -> Response {
    let owner_key_id = auth_context
        .as_ref()
        .map(|Extension(context)| context.key_id.as_str())
        .unwrap_or("local");
    if !valid_conversation_id(&conversation_id) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_conversation_id",
            "conversation ID must be 8-128 ASCII letters, digits, '_' or '-'",
        );
    }
    match state
        .memory
        .clone()
        .export_conversation_async(owner_key_id.to_owned(), conversation_id.clone())
        .await
    {
        Ok(Some(conversation)) => {
            let mut markdown = format!("# Conversation {}\n\n", conversation.id);
            if let Some(provider) = conversation.provider.as_deref() {
                markdown.push_str(&format!("- Provider: {provider}\n"));
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

#[cfg(test)]
fn export_conversation_markdown_response(
    memory: &UnifiedMemory,
    conversation_id: &str,
) -> Response {
    export_conversation_markdown_response_for_owner(memory, "local", conversation_id)
}

#[cfg(test)]
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
    if !valid_conversation_id(&conversation_id) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_conversation_id",
            "conversation ID must be 8-128 ASCII letters, digits, '_' or '-'",
        );
    }
    match state
        .memory
        .clone()
        .delete_conversation_async(owner_key_id.to_owned(), conversation_id.clone())
        .await
    {
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

fn retain_model_owners(model_list: &mut serde_json::Value, available: impl Fn(&str) -> bool) {
    if let Some(models) = model_list
        .get_mut("data")
        .and_then(serde_json::Value::as_array_mut)
    {
        models.retain(|model| {
            model
                .get("owned_by")
                .and_then(serde_json::Value::as_str)
                .is_some_and(&available)
        });
    }
}

async fn models(
    State(state): State<Arc<AppState>>,
    auth_context: Option<Extension<AuthKeyContext>>,
) -> impl IntoResponse {
    let cache = state.models_cache.read().await;
    let mut model_list = build_model_list(&state.cfg, &cache);
    retain_model_owners(&mut model_list, |provider| {
        state.provider_eligible(provider, ProviderEligibilityMode::ReadOnly)
    });
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
    memory.build_context_for_owner_with_history("local", conversation_id, messages)
}

async fn read_conversation_context_with_transcript_for_owner(
    memory: Arc<dyn memory::ConversationStore>,
    owner_key_id: String,
    conversation_id: Option<String>,
    messages: Vec<ChatMessage>,
    token_budget: usize,
) -> anyhow::Result<(String, Vec<ChatMessage>, bool, Vec<ChatMessage>)> {
    memory
        .build_context_with_transcript(owner_key_id, conversation_id, messages, token_budget)
        .await
}

fn budget_personal_memory_items(
    items: Vec<memory::LongTermMemoryItem>,
    token_budget: usize,
) -> (Vec<memory_egress::PersonalMemoryItem>, usize) {
    let mut used = 0usize;
    let mut selected = Vec::new();
    for item in items {
        let content = format!("{}: {}", item.memory_key, item.value);
        let cost = context::estimate_message_tokens(&content).saturating_add(8);
        if cost <= token_budget.saturating_sub(used) {
            used = used.saturating_add(cost);
            selected.push(memory_egress::PersonalMemoryItem {
                id: item.id,
                content,
            });
        }
    }
    (selected, used)
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
    chat_handler::run(state, headers, auth_context, body).await
}

fn unavailable_provider_response(_last_upstream_error: Option<&str>) -> Response {
    api_error(
        axum::http::StatusCode::BAD_GATEWAY,
        "provider_error",
        "provider_unavailable",
        "all configured providers failed; check provider session status",
    )
}

#[cfg(test)]
mod provider_failure_redaction_tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn exhausted_provider_response_does_not_echo_upstream_details() {
        let raw_upstream_error = "cookie=session-secret; Authorization: Bearer sensitive-token";
        // The public response must never serialize an arbitrary upstream
        // error, even if it resembles a credential or cookie.
        let response = unavailable_provider_response(Some(raw_upstream_error));
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "provider_unavailable");
        assert_eq!(
            json["error"]["message"],
            "all configured providers failed; check provider session status"
        );
        assert!(!String::from_utf8_lossy(&body).contains("session-secret"));
        assert!(!String::from_utf8_lossy(&body).contains("sensitive-token"));
    }
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
            completion_idle_ms: 15_000,
            queue_timeout_seconds: 30,
            session_pool_size: 1,
            min_request_interval_ms: 5_000,
            daily_request_cap: 100,
            models: models.iter().map(|model| (*model).to_owned()).collect(),
        }
    }

    #[test]
    fn model_list_omits_providers_that_are_not_currently_routable() {
        let mut list = serde_json::json!({
            "object": "list",
            "data": [
                { "id": "chatgpt-model", "owned_by": "chatgpt" },
                { "id": "claude-model", "owned_by": "claude" },
                { "id": "unowned-model" }
            ]
        });
        retain_model_owners(&mut list, |provider| provider == "chatgpt");
        assert_eq!(list["data"].as_array().unwrap().len(), 1);
        assert_eq!(list["data"][0]["id"], "chatgpt-model");
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
    use axum::body::{to_bytes, Body};
    use std::{path::PathBuf, sync::Arc};
    use tower::ServiceExt;

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
        match memory
            .clone()
            .list_conversations_async("local".to_owned(), limit, offset)
            .await
        {
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
                get(|| async { api::health::readiness_response(1, true, 0) }),
            )
            .route("/v1/conversations", get(list_endpoint))
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

    fn real_chat_app() -> Router {
        let (router, _) =
            super::real_chat_router_scripted_backend_tests::fixture(&["chatgpt"], vec![], None);
        router.layer(middleware::from_fn(attach_request_id))
    }

    async fn json_body(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
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
        let (app, _, backend) = super::real_chat_router_scripted_backend_tests::fixture_with_memory(
            &["chatgpt"],
            vec![],
            None,
            memory,
        );
        let app = app.layer(middleware::from_fn(attach_request_id));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .header("X-Conversation-Id", "conversation_123")
                    .body(Body::from(
                        r#"{"model":"auto","messages":[{"role":"user","content":"hello"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(response.headers().contains_key("x-request-id"));
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "memory_read_failed");
        assert!(backend.observed_requests().await.is_empty());
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
        let response = real_chat_app()
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

        let response = real_chat_app()
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
        let oversized = "x".repeat(1024 * 1024 + 1);
        let response = real_chat_app()
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
        assert!(body.get("tab_id").is_none());
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
#[path = "main_tests/login_security_middleware_http_tests.rs"]
mod login_security_middleware_http_tests;

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
        assert_eq!(
            authorize_api_request(
                &config,
                Some("Bearer test-secret"),
                "/v1/admin/memory-egress-policy",
                &axum::http::Method::GET
            ),
            Err(AuthFailure::InsufficientScope)
        );
    }

    #[test]
    fn operator_reset_route_requires_admin_scope() {
        let path = "/v1/admin/providers/chatgpt/reset";
        assert_eq!(
            authorize_api_request(
                &auth(&["chat"]),
                Some("Bearer test-secret"),
                path,
                &axum::http::Method::POST,
            ),
            Err(AuthFailure::InsufficientScope)
        );
        assert!(authorize_api_request(
            &auth(&["admin"]),
            Some("Bearer test-secret"),
            path,
            &axum::http::Method::POST,
        )
        .is_ok());
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
            "/v1/admin/memory-egress-policy",
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
mod provider_thread_sync_marker_tests {
    use super::transcript_marker;
    use crate::memory::ChatMessage;

    #[test]
    fn transcript_fingerprint_is_stable_and_changes_when_history_changes() {
        let first = vec![ChatMessage {
            role: "user".to_owned(),
            content: "first question".to_owned(),
        }];
        assert_eq!(transcript_marker(&first), transcript_marker(&first));

        let edited = vec![ChatMessage {
            role: "user".to_owned(),
            content: "different question".to_owned(),
        }];
        assert_ne!(transcript_marker(&first), transcript_marker(&edited));

        let extended = vec![
            first[0].clone(),
            ChatMessage {
                role: "assistant".to_owned(),
                content: "answer".to_owned(),
            },
        ];
        assert_ne!(transcript_marker(&first), transcript_marker(&extended));
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

    #[test]
    fn reopenable_thread_urls_match_the_supported_provider_paths() {
        let supported = [
            ("chatgpt", "https://chatgpt.com/c/123"),
            ("chatgpt", "https://chatgpt.com/c/123/?model=default"),
            ("chatgpt", "https://chatgpt.com/g/g-123/c/thread-456"),
            ("claude", "https://claude.ai/chat/123"),
            ("claude", "https://claude.ai/chat/123?source=sidebar"),
        ];
        for (provider, url) in supported {
            assert!(
                is_reopenable_provider_thread_url(provider, url),
                "expected {provider} thread URL to be reopenable: {url}"
            );
        }

        // Only ChatGPT and Claude have a reviewed URL-resume contract so far.
        // A URL shape alone does not prove a provider can reopen the right thread;
        // all other adapters remain on bounded transcript replay until verified.
        assert!(!is_reopenable_provider_thread_url(
            "metaai",
            "https://meta.ai/c/123"
        ));
    }

    #[test]
    fn reopenable_thread_urls_reject_landing_pages_and_foreign_hosts() {
        let invalid = [
            ("chatgpt", "https://chatgpt.com/"),
            ("claude", "https://claude.ai/"),
            ("gemini", "https://gemini.google.com/"),
            ("grok", "https://grok.com/"),
            ("qwen", "https://chat.qwen.ai/"),
            ("mistral", "https://chat.mistral.ai/"),
            ("perplexity", "https://perplexity.ai/"),
            ("chatgpt", "https://chatgpt.com.attacker.invalid/c/123"),
            ("claude", "https://attacker.invalid/chat/123"),
            ("chatgpt", "https://chatgpt.com/c/"),
            ("chatgpt", "https://chatgpt.com/c/?source=sidebar"),
            ("chatgpt", "https://chatgpt.com/c/#fragment"),
            ("chatgpt", "https://chatgpt.com/c/123/extra"),
            ("chatgpt", "https://chatgpt.com/c/.."),
            ("chatgpt", "https://chatgpt.com/c/%2e%2e"),
            ("chatgpt", "https://chatgpt.com/c/%2fprivate"),
            ("chatgpt", "https://chatgpt.com/c/thread%3Fother"),
            ("chatgpt", "https://chatgpt.com/g/../c/thread-123"),
            ("chatgpt", "https://chatgpt.com/g/g-123/c/%2f"),
            ("claude", "https://claude.ai/chat/.."),
            ("claude", "https://claude.ai/chat/%2e%2e"),
            ("claude", "https://claude.ai/chat/thread%2fother"),
            ("chatgpt", "https://chatgpt.com/g/g-123/c/"),
            ("chatgpt", "https://chatgpt.com/g//c/thread-123"),
            ("chatgpt", "https://chatgpt.com/g/g-123/c/thread-123/extra"),
            ("claude", "https://claude.ai/chat/"),
            ("claude", "https://claude.ai/chat/?source=sidebar"),
            ("claude", "https://claude.ai/chat/123/extra"),
        ];
        for (provider, url) in invalid {
            assert!(
                !is_reopenable_provider_thread_url(provider, url),
                "unexpectedly accepted {provider} thread URL: {url}"
            );
        }
    }
}

#[cfg(test)]
mod initial_stream_chunk_tests {
    use crate::chat_service::usable_initial_stream_chunk;

    #[test]
    fn only_nonempty_content_can_start_a_stream() {
        assert!(usable_initial_stream_chunk("first token", false));
        assert!(!usable_initial_stream_chunk("", false));
        assert!(!usable_initial_stream_chunk("", true));
        assert!(usable_initial_stream_chunk("content marked done", true));
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
        tx.send(Err(crate::chat_backend::GatewayError::session_expired(
            "session_expired: please sign in",
        )))
        .await
        .unwrap();
        drop(tx);

        let event = first_event(&mut rx, Duration::from_millis(100)).await;
        let FirstEvent::Error(error) = event else {
            panic!("expected first-event error");
        };
        let provider_sessions = crate::provider_state::ProviderSessionRegistry::default();
        let metrics_registry = crate::metrics::MetricsRegistry::default();
        record_attempt_error(
            &provider_sessions,
            &metrics_registry,
            provider,
            Some(&error),
            0,
            Instant::now(),
            Instant::now(),
        );

        assert_eq!(
            provider_sessions.status(provider).state,
            crate::provider_state::ProviderSessionState::NeedsLogin
        );
        let metrics = metrics_registry.render_with_provider_states(&[]);
        assert!(metrics.contains(&format!(
            "gateway_provider_requests_total{{provider=\"{provider}\",outcome=\"error\"}} 1"
        )));
    }

    #[test]
    fn first_attempt_auth_state_follows_error_kind_not_provider_text() {
        let provider = "test-first-event-typed-provider-failure";
        let registry = crate::provider_state::ProviderSessionRegistry::default();
        let metrics = crate::metrics::MetricsRegistry::default();
        let upstream = crate::chat_backend::GatewayError::provider_failure(
            "session expired; please sign in; cookie=synthetic-secret",
        );
        record_attempt_error(
            &registry,
            &metrics,
            provider,
            Some(&upstream),
            0,
            Instant::now(),
            Instant::now(),
        );
        assert_eq!(
            registry.status(provider).state,
            crate::provider_state::ProviderSessionState::Degraded,
            "an ordinary typed failure must not force login from untrusted text"
        );

        let untouched = "test-first-event-non-provider-error";
        let caller_error = crate::chat_backend::GatewayError::typed(
            crate::chat_backend::GatewayErrorKind::InvalidRequest,
            "session expired; please sign in",
            false,
        );
        record_attempt_error(
            &registry,
            &metrics,
            untouched,
            Some(&caller_error),
            0,
            Instant::now(),
            Instant::now(),
        );
        assert_eq!(
            registry.status(untouched).state,
            crate::provider_state::ProviderSessionState::Unknown
        );
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

#[cfg(test)]
mod long_term_memory_retrieval_tests {
    use super::*;

    #[test]
    fn retrieval_honors_zero_limit_and_ranks_confidence_deterministically() {
        let memory = UnifiedMemory::new(8000);
        memory
            .set_long_term_memory_enabled("owner-a", true)
            .unwrap();
        memory
            .save_long_term_memory_item("owner-a", "low", "lower confidence", None, 0.2, None)
            .unwrap();
        memory
            .save_long_term_memory_item("owner-a", "high", "higher confidence", None, 0.9, None)
            .unwrap();

        assert!(memory
            .retrieve_long_term_memory_items("owner-a", 0)
            .unwrap()
            .is_empty());

        let items = memory
            .retrieve_long_term_memory_items("owner-a", 2)
            .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].memory_key, "high");
        assert_eq!(items[1].memory_key, "low");
    }
}

#[cfg(test)]
mod long_term_memory_export_tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn export_is_versioned_owner_scoped_json_attachment() {
        let memory = UnifiedMemory::new(8000);
        memory
            .set_long_term_memory_enabled("owner-a", true)
            .unwrap();
        memory
            .save_long_term_memory_item("owner-a", "timezone", "America/New_York", None, 0.9, None)
            .unwrap();
        memory
            .set_long_term_memory_enabled("owner-b", true)
            .unwrap();
        memory
            .save_long_term_memory_item("owner-b", "timezone", "Europe/London", None, 1.0, None)
            .unwrap();

        let items = memory.list_long_term_memory_items("owner-a").unwrap();
        let response = memory_api::long_term_memory_export_response(true, items);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_DISPOSITION).unwrap(),
            "attachment; filename=\"long-term-memory.json\""
        );
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let export: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(export["format_version"], 1);
        assert_eq!(export["enabled"], true);
        assert_eq!(export["data"].as_array().unwrap().len(), 1);
        assert_eq!(export["data"][0]["memory_key"], "timezone");
        assert_eq!(export["data"][0]["value"], "America/New_York");
    }
}

#[cfg(test)]
mod personal_memory_budget_tests {
    use super::*;

    #[test]
    fn personal_memory_selection_never_exceeds_its_reserved_token_budget() {
        let items = vec![
            memory::LongTermMemoryItem {
                id: "small".into(),
                memory_key: "timezone".into(),
                value: "America/New_York".into(),
                source_conversation_id: None,
                confidence: 1.0,
                created_at: 1,
                updated_at: 1,
                expires_at: None,
            },
            memory::LongTermMemoryItem {
                id: "large".into(),
                memory_key: "long-note".into(),
                value: "x".repeat(20_000),
                source_conversation_id: None,
                confidence: 0.9,
                created_at: 1,
                updated_at: 1,
                expires_at: None,
            },
        ];
        let (selected, used) = budget_personal_memory_items(items, 32);
        assert!(used <= 32);
        assert!(selected.iter().all(|item| item.id != "large"));
        let (none, used) = budget_personal_memory_items(
            vec![memory::LongTermMemoryItem {
                id: "small".into(),
                memory_key: "timezone".into(),
                value: "America/New_York".into(),
                source_conversation_id: None,
                confidence: 1.0,
                created_at: 1,
                updated_at: 1,
                expires_at: None,
            }],
            0,
        );
        assert!(none.is_empty());
        assert_eq!(used, 0);
    }
}

#[cfg(test)]
mod gateway_completion_outcome_tests {
    use super::completed_chat_outcome;

    #[test]
    fn gateway_success_requires_stream_and_persistence_success() {
        assert_eq!(completed_chat_outcome(false, false), "success");
        assert_eq!(completed_chat_outcome(true, false), "error");
        assert_eq!(completed_chat_outcome(false, true), "error");
        assert_eq!(completed_chat_outcome(true, true), "error");
    }
}

#[cfg(test)]
#[path = "main_tests/real_chat_router_scripted_backend_tests.rs"]
mod real_chat_router_scripted_backend_tests;
