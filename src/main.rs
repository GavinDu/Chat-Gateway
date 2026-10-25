use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, State},
    http::HeaderMap,
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    response::{
        sse::{Event, Sse},
        Html, IntoResponse,
    },
    routing::{delete, get, post},
    Json, Router,
};
use clap::Parser;
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::sync::RwLock;

mod browser;
mod config;
mod login;
mod memory;
mod providers;
mod router;
mod session;

use browser::pool::ObscuraPool;
use config::AppConfig;
use memory::{ChatMessage, UnifiedMemory};
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
        help = "Test one adapter live: discover models + show selectors, then exit"
    )]
    test_adapter: Option<String>,
}

struct AppState {
    cfg: Arc<AppConfig>,
    pool: Arc<ObscuraPool>,
    memory: Arc<UnifiedMemory>,
    router: ProviderRouter,
    models_cache: Arc<RwLock<HashMap<String, Vec<String>>>>,
}

#[derive(serde::Deserialize)]
struct ChatReq {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    temperature: Option<f32>,
    // Capture unknown OpenAI-style fields so validation can return a clear
    // unsupported_parameter error instead of misreporting them as malformed JSON.
    #[serde(flatten)]
    unsupported_fields: HashMap<String, serde_json::Value>,
}

/// Validate request semantics separately from JSON decoding so the contract
/// can be exercised without initializing browser-backed providers.
fn validate_chat_req(req: &ChatReq) -> Result<(), (&'static str, String)> {
    const MAX_MESSAGES: usize = 256;
    const MAX_MESSAGE_BYTES: usize = 128 * 1024;
    const MAX_TOTAL_CONTENT_BYTES: usize = 512 * 1024;

    if req.model.trim().is_empty() {
        return Err(("model_required", "model must not be empty".to_string()));
    }
    if req.messages.is_empty() {
        return Err((
            "messages_required",
            "messages must contain at least one message".to_string(),
        ));
    }
    if req.messages.len() > MAX_MESSAGES {
        return Err((
            "too_many_messages",
            format!("messages may contain at most {MAX_MESSAGES} entries"),
        ));
    }
    let mut total_content_bytes = 0usize;
    for (index, message) in req.messages.iter().enumerate() {
        if message.content.len() > MAX_MESSAGE_BYTES {
            return Err((
                "message_too_large",
                format!("messages[{index}].content exceeds the {MAX_MESSAGE_BYTES}-byte limit"),
            ));
        }
        total_content_bytes = total_content_bytes.saturating_add(message.content.len());
        if total_content_bytes > MAX_TOTAL_CONTENT_BYTES {
            return Err((
                "request_content_too_large",
                format!(
                    "combined message content exceeds the {MAX_TOTAL_CONTENT_BYTES}-byte limit"
                ),
            ));
        }
        if !matches!(
            message.role.as_str(),
            "system" | "developer" | "user" | "assistant" | "tool"
        ) {
            return Err((
                "invalid_message_role",
                format!("messages[{index}].role '{}' is not supported", message.role),
            ));
        }
    }
    if req.temperature.is_some() {
        return Err((
            "unsupported_parameter",
            "temperature is not supported by the current browser-backed adapters".to_string(),
        ));
    }
    if !req.unsupported_fields.is_empty() {
        let mut fields = req.unsupported_fields.keys().cloned().collect::<Vec<_>>();
        fields.sort();
        return Err((
            "unsupported_parameter",
            format!("unsupported request parameter(s): {}", fields.join(", ")),
        ));
    }
    Ok(())
}

fn api_error(
    status: axum::http::StatusCode,
    error_type: &str,
    code: &str,
    message: impl Into<String>,
) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({
            "error": {
                "message": message.into(),
                "type": error_type,
                "code": code,
            }
        })),
    )
        .into_response()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let cfg = Arc::new(AppConfig::load(&cli.config)?);

    let pool = ObscuraPool::new(cfg.clone()).await?;
    pool.warm_all().await?;

    if let Some(provider) = cli.test_adapter {
        let permit = pool.acquire(&provider).await?;
        println!(
            "Testing adapter '{}' with embedded Obscura (queue {}ms)...",
            provider, permit.queue_ms
        );
        drop(permit);
        // Live-send phases (Go RunAdapterTest, restored-tree form) for
        // ALL providers — Rust v1 diagnostics only did discover+print.
        match pool.test_adapter(&provider).await {
            Ok(rep) => {
                println!("test-adapter '{}': session_ok={}", provider, rep.session_ok);
                for (role, hit) in &rep.probe {
                    match hit {
                        Some(sel) => println!("  probe {role}: OK ({sel})"),
                        None => println!("  probe {role}: ALL selectors failed"),
                    }
                }
                println!(
                    "  phases: input_found={} send_clicked={} (false = Enter used) response_token={}",
                    rep.input_found, rep.send_clicked, rep.response_token
                );
                for e in &rep.errors {
                    println!("  error: {e}");
                }
                if rep.errors.is_empty() && rep.session_ok && rep.input_found && rep.response_token
                {
                    println!("test-adapter: all phases passed — adapter is healthy");
                }
            }
            Err(e) => println!("test-adapter failed: {}", e),
        }
        // Plus the Rust v1 diagnostics output (discover + selectors).
        match pool.discover(&provider).await {
            Ok(models) => println!("Discovered models: {:?}", models),
            Err(e) => println!("Discovery failed: {}", e),
        }
        if let Some(p) = providers::Provider::from_name(&provider) {
            let registry = providers::build_registry(&cfg);
            if let Some(adapter) = registry.get(&p) {
                println!("Selectors: {:?}", adapter.selectors());
            }
        }
        return Ok(());
    }

    let state = Arc::new(AppState {
        cfg: cfg.clone(),
        pool: pool.clone(),
        memory: Arc::new(UnifiedMemory::from_env(cfg.memory.max_tokens)?),
        router: ProviderRouter::new(cfg.clone()),
        models_cache: Arc::new(RwLock::new(HashMap::new())),
    });

    // Background model discovery (reads pickers without sending messages)
    {
        let state = state.clone();
        tokio::spawn(async move {
            refresh_models(&state).await;
        });
    }

    let app = Router::new()
        .route("/health", get(health))
        .route("/ready", get(readiness))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .route(
            "/v1/conversations/:conversation_id",
            delete(delete_conversation),
        )
        .route("/login", get(login_page))
        .route("/login/api/status", get(login_status))
        .route("/login/api/:provider/start", post(login_start))
        .route("/login/api/:provider/cancel", post(login_cancel))
        .route("/login/api/:provider/complete", post(login_complete))
        .route("/login/api/:provider/view", get(login_view))
        .route("/login/api/:provider/act", post(login_act))
        .with_state(state)
        // Semantic message limits run after JSON parsing; cap the raw body too
        // so an oversized payload cannot consume unbounded memory before
        // request validation has a chance to reject it.
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn(protect_login_api))
        // Keep request IDs on both successful responses and middleware rejections.
        .layer(middleware::from_fn(attach_request_id));

    // Security default: the gateway exposes browser/session control routes and must not listen on public interfaces by default.
    // Remote access should be added only with explicit authentication and TLS.
    let addr = SocketAddr::from(([127, 0, 0, 1], cfg.server.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("Chat Gateway v2 Obscura Native listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
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
    let origin_is_safe = !origin_present || login_origin_is_same_authority(headers);
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
    next.run(req).await
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

async fn login_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let active = state.pool.login_status_all().await.unwrap_or_default();
    let by_name: HashMap<String, browser::pool::LoginActiveInfo> = active
        .into_iter()
        .map(|a| (a.provider.clone(), a))
        .collect();
    let providers = providers::Provider::all()
        .iter()
        .map(|p| {
            let name = p.name();
            let enabled = state.cfg.provider(name).map(|c| c.enabled).unwrap_or(false);
            let (session_present, session_ts) = match session::load_session_file(name) {
                Ok(Some(f)) => (true, Some(f.ts)),
                _ => (false, None),
            };
            let a = by_name.get(name);
            serde_json::json!({
                "name": name,
                "enabled": enabled,
                "login_url": p.login_url(),
                "session_present": session_present,
                "session_ts": session_ts,
                "login_active": a.is_some(),
                "current_url": a.map(|x| x.url.clone()),
                "title": a.map(|x| x.title.clone()),
                "logged_in": a.and_then(|x| x.logged_in),
                "input_found": a.map(|x| x.input_found),
                "state": a.map(|x| x.state.clone()),
            })
        })
        .collect::<Vec<_>>();
    Json(serde_json::json!({ "providers": providers }))
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
        Ok(r) => Json(serde_json::json!({
            "ok": true, "provider": provider,
            "session_path": r.session_path,
            "warm_swapped": r.warm_swapped,
            "warning": r.warning,
        }))
        .into_response(),
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

async fn login_act(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    Json(req): Json<LoginActReq>,
) -> impl IntoResponse {
    let action = match req.action.as_str() {
        "click" => login::LoginAction::Click {
            selector: req.selector.unwrap_or_default(),
        },
        "click_button" => login::LoginAction::ClickButton {
            index: req.index.unwrap_or(0),
        },
        "type" => login::LoginAction::Type {
            selector: req.selector.unwrap_or_default(),
            text: req.text.unwrap_or_default(),
        },
        "enter" => login::LoginAction::Enter {
            selector: req.selector.unwrap_or_default(),
        },
        "navigate" => login::LoginAction::Navigate {
            url: req.url.unwrap_or_default(),
        },
        other => return login_err(format!("unknown login action: {other}")),
    };
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

/// Readiness probe for process supervisors and local deployment wrappers.
/// Startup already validates configuration and warms the browser pool before
/// binding the listener; this endpoint reports whether at least one provider
/// is configured as enabled.
fn readiness_response(enabled: usize) -> Response {
    if enabled == 0 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "not_ready",
                "reason": "no_enabled_provider",
            })),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ready",
            "enabled_providers": enabled,
        })),
    )
        .into_response()
}

async fn readiness(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    readiness_response(state.cfg.providers.iter().filter(|p| p.enabled).count())
}

/// Add a gateway-generated request ID to every response, including security
/// middleware rejections, so callers can correlate failures with server logs
/// without trusting arbitrary client-supplied IDs.
async fn attach_request_id(req: Request<Body>, next: Next) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut response = next.run(req).await;
    if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

fn delete_conversation_response(memory: &UnifiedMemory, conversation_id: &str) -> Response {
    if !valid_conversation_id(conversation_id) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_conversation_id",
            "conversation ID must be 8-128 ASCII letters, digits, '_' or '-'",
        );
    }
    match memory.delete_conversation(conversation_id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_error(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "conversation_not_found",
            "conversation does not exist",
        ),
        Err(error) => {
            tracing::error!(conversation_id = %conversation_id, "conversation deletion failed: {error}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_persistence_failed",
                "conversation could not be deleted durably",
            )
        }
    }
}

async fn delete_conversation(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<String>,
) -> Response {
    delete_conversation_response(&state.memory, &conversation_id)
}

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let providers = state.cfg.providers.iter().map(|p| {
        serde_json::json!({ "name": p.name, "enabled": p.enabled, "priority": p.priority, "models": p.models })
    }).collect::<Vec<_>>();
    Json(serde_json::json!({
        "status": "ok",
        "engine": "obscura-native (embedded)",
        "sessions_loaded": "sessions/<provider>.json (restored at warm-up)",
        "providers": providers,
    }))
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

async fn models(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let cache = state.models_cache.read().await;
    Json(build_model_list(&state.cfg, &cache))
}

/// Conversation IDs are echoed into response IDs and tab affinity. Restrict
/// them to bounded ASCII so formatting and byte slicing cannot panic on a
/// malicious multibyte header value.
fn valid_conversation_id(value: &str) -> bool {
    (8..=128).contains(&value.len())
        && value.is_ascii()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let req = match parse_and_validate_chat_req(&body) {
        Ok(req) => req,
        Err(response) => return response,
    };

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

    // Assemble the provider prompt from the unified memory window. The
    // canonical transcript must be passed to the provider so follow-up
    // turns and provider switches retain conversation context.
    let (context_prompt, windowed) = state
        .memory
        .build_context(conversation_id.as_deref(), &req.messages);
    let has_history = conversation_id
        .as_deref()
        .map(|cid| state.memory.has_history(cid))
        .unwrap_or(false);
    let prompt = context_prompt;
    if prompt.is_empty() {
        return api_error(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "empty_prompt",
            "prompt must not be empty",
        );
    }

    let candidates = state
        .router
        .resolve(&req.model, conversation_id.as_deref(), &state.memory);
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

    let mut last_err: Option<String> = None;
    for provider_name in candidates {
        let permit = match state.pool.acquire(&provider_name).await {
            Ok(p) => p,
            Err(e) => {
                last_err = Some(e.to_string());
                continue;
            }
        };
        tracing::debug!(provider = %provider_name, queue_ms = permit.queue_ms, "acquired provider slot");

        let mut rx = match state
            .pool
            .send_stream(
                &provider_name,
                prompt.clone(),
                provider_model.clone(),
                has_history,
            )
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                last_err = Some(e.to_string());
                continue;
            }
        };

        let new_conv_id = conversation_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        if req.stream {
            let state_c = state.clone();
            let windowed_c = windowed.clone();
            let provider_c = provider_name.clone();
            let model_c = req.model.clone();
            let conv_stream = new_conv_id.clone();
            let sse_stream = async_stream::stream! {
                let _permit = permit;
                let mut full = String::new();
                let mut stream_error: Option<String> = None;
                while let Some(item) = rx.recv().await {
                    match item {
                        Ok(c) => {
                            if c.done { break; }
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
                            stream_error = Some("provider stream failed; partial response was not committed".to_string());
                            break;
                        }
                    }
                }

                if let Some(message) = stream_error {
                    let error = serde_json::json!({
                        "error": { "message": message, "type": "provider_error", "code": "stream_failed" }
                    });
                    yield Ok(Event::default().data(error.to_string()));
                } else {
                    if let Err(error) = state_c.memory.store_turn(
                        conv_stream.clone(),
                        windowed_c.clone(),
                        ChatMessage { role: "assistant".to_string(), content: full.clone() },
                        provider_c.clone(),
                        format!("tab-{}", &conv_stream[..8]),
                    ) {
                        tracing::error!("conversation persistence failed after stream: {error}");
                        let error = serde_json::json!({
                            "error": { "message": "conversation could not be persisted", "type": "server_error", "code": "memory_persistence_failed" }
                        });
                        yield Ok(Event::default().data(error.to_string()));
                        yield Ok(Event::default().data("[DONE]"));
                        return;
                    }
                    let finish = serde_json::json!({
                        "id": format!("chatcmpl-{}", &conv_stream[..8]),
                        "object": "chat.completion.chunk",
                        "created": chrono::Utc::now().timestamp(),
                        "model": model_c,
                        "choices": [{ "delta": {}, "index": 0, "finish_reason": "stop" }],
                    });
                    yield Ok(Event::default().data(finish.to_string()));
                }
                yield Ok(Event::default().data("[DONE]"));
            };
            let mut resp_headers = axum::http::HeaderMap::new();
            resp_headers.insert("X-Conversation-Id", new_conv_id.parse().unwrap());
            resp_headers.insert("Content-Type", "text/event-stream".parse().unwrap());
            return (resp_headers, Sse::new(sse_stream)).into_response();
        }

        // Non-streaming: retry another provider only if no response bytes were
        // emitted. A partial response is an error, never a successful turn.
        let mut full = String::new();
        let mut failed: Option<String> = None;
        while let Some(item) = rx.recv().await {
            match item {
                Ok(c) => {
                    if c.done {
                        break;
                    }
                    full.push_str(&c.content);
                }
                Err(e) => {
                    failed = Some(e.to_string());
                    break;
                }
            }
        }
        if let Some(e) = failed {
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

        if let Err(error) = state.memory.store_turn(
            new_conv_id.clone(),
            windowed.clone(),
            ChatMessage {
                role: "assistant".to_string(),
                content: full.clone(),
            },
            provider_name.clone(),
            format!("tab-{}", &new_conv_id[..8]),
        ) {
            tracing::error!("conversation persistence failed: {error}");
            return api_error(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_persistence_failed",
                "conversation could not be persisted",
            );
        }

        let body = serde_json::json!({
            "id": format!("chatcmpl-{}", &new_conv_id[..8]),
            "object": "chat.completion",
            "created": chrono::Utc::now().timestamp(),
            "model": req.model,
            "choices": [{ "message": { "role": "assistant", "content": full }, "finish_reason": "stop", "index": 0 }],
            "conversation_id": new_conv_id,
        });
        let mut resp_headers = axum::http::HeaderMap::new();
        resp_headers.insert("X-Conversation-Id", new_conv_id.parse().unwrap());
        return (resp_headers, Json(body)).into_response();
    }

    api_error(
        axum::http::StatusCode::BAD_GATEWAY,
        "provider_error",
        "provider_unavailable",
        format!("all providers failed: {:?}", last_err),
    )
}

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
            session_pool_size: 1,
            models: models.iter().map(|model| (*model).to_owned()).collect(),
        }
    }

    #[test]
    fn excludes_configured_and_discovered_models_for_disabled_providers() {
        let cfg = AppConfig {
            server: config::ServerConfig::default(),
            memory: config::MemoryConfig::default(),
            obscura: config::ObscuraConfig::default(),
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
    use std::{path::PathBuf, sync::Arc};
    use tower::ServiceExt;

    async fn validation_endpoint(body: axum::body::Bytes) -> Response {
        match parse_and_validate_chat_req(&body) {
            Ok(request) => Json(serde_json::json!({
                "accepted": true,
                "model": request.model,
            }))
            .into_response(),
            Err(response) => response,
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
            .route("/ready", get(|| async { readiness_response(1) }))
            .route("/v1/chat/completions", post(validation_endpoint))
            .route(
                "/v1/conversations/:conversation_id",
                delete(delete_endpoint),
            )
            .with_state(memory)
            .layer(middleware::from_fn(attach_request_id))
    }

    async fn json_body(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
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
        assert!(!restored.has_history("conversation_http_1"));
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
