use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::{sse::{Event, Sse}, Html, IntoResponse},
    routing::{get, post},
    middleware::{self, Next},
    http::{Request, StatusCode},
    response::Response,
    body::Body,
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
#[command(name = "chat-gateway-rs", about = "Chat Gateway v2 - Obscura native, unified memory over 8 providers")]
struct Cli {
    #[arg(long, default_value = "config.yaml")]
    config: std::path::PathBuf,

    #[arg(long, help = "Test one adapter live: discover models + show selectors, then exit")]
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
#[serde(deny_unknown_fields)]
struct ChatReq {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    temperature: Option<f32>,
}

fn api_error(status: axum::http::StatusCode, error_type: &str, code: &str, message: impl Into<String>) -> axum::response::Response {
    (status, Json(serde_json::json!({
        "error": {
            "message": message.into(),
            "type": error_type,
            "code": code,
        }
    }))).into_response()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();
    let cfg = Arc::new(AppConfig::load(&cli.config)?);

    let pool = ObscuraPool::new(cfg.clone()).await?;
    pool.warm_all().await?;

    if let Some(provider) = cli.test_adapter {
        let permit = pool.acquire(&provider).await?;
        println!("Testing adapter '{}' with embedded Obscura (queue {}ms)...", provider, permit.queue_ms);
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
                if rep.errors.is_empty() && rep.session_ok && rep.input_found && rep.response_token {
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
        memory: Arc::new(UnifiedMemory::from_env(cfg.memory.max_tokens)),
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
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/login", get(login_page))
        .route("/login/api/status", get(login_status))
        .route("/login/api/:provider/start", post(login_start))
        .route("/login/api/:provider/cancel", post(login_cancel))
        .route("/login/api/:provider/complete", post(login_complete))
        .route("/login/api/:provider/view", get(login_view))
        .route("/login/api/:provider/act", post(login_act))
        .with_state(state)
        .layer(middleware::from_fn(protect_login_api));

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
                state.models_cache.write().await.insert(p.name.clone(), models);
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
    if !req.uri().path().starts_with("/login/api/") {
        return next.run(req).await;
    }
    let headers = req.headers();
    let fetch_site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());
    if fetch_site == Some("cross-site") || !login_origin_is_same_authority(headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": {
                    "message": "cross-origin browser-control request rejected",
                    "type": "security_error",
                    "code": "cross_origin_request"
                }
            })),
        ).into_response();
    }
    next.run(req).await
}

fn login_origin_is_same_authority(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        // CLI and local automation clients commonly omit Origin. The
        // Sec-Fetch-Site check above still rejects explicitly cross-site calls.
        return true;
    };
    let Ok(origin) = origin.to_str() else { return false; };
    let Some((scheme, authority)) = origin.split_once("://") else { return false; };
    if scheme != "http" && scheme != "https" { return false; }
    let Some(host) = headers.get(axum::http::header::HOST).and_then(|v| v.to_str().ok()) else {
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
        let Some((host, rest)) = authority.split_once(']') else { return false; };
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
    matches!(host.to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "[::1]")
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
            let enabled = state
                .cfg
                .provider(name)
                .map(|c| c.enabled)
                .unwrap_or(false);
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
        Ok(url) => Json(serde_json::json!({ "ok": true, "provider": provider, "url": url })).into_response(),
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
    match state.pool.login_act(&provider, action).await {
        // Deliberately no echo of the request: the detail strings never
        // contain relayed text.
        Ok(detail) => Json(serde_json::json!({ "ok": true, "detail": detail })).into_response(),
        Err(e) => login_err(e.to_string()),
    }
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

async fn models(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let cache = state.models_cache.read().await;
    let mut data = vec![];
    for p in &state.cfg.providers {
        for m in &p.models {
            data.push(serde_json::json!({ "id": m, "object": "model", "owned_by": p.name }));
        }
    }
    for (provider, models) in cache.iter() {
        for m in models {
            if !data.iter().any(|d| d["id"] == *m) {
                data.push(serde_json::json!({ "id": m, "object": "model", "owned_by": provider, "discovered": true }));
            }
        }
    }
    Json(serde_json::json!({ "object": "list", "data": data }))
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let req: ChatReq = match serde_json::from_slice::<ChatReq>(&body) {
        Ok(r) => r,
        Err(e) => {
            return api_error(
                axum::http::StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_json",
                format!("invalid or unsupported request field: {e}"),
            );
        }
    };

    if req.model.trim().is_empty() {
        return api_error(axum::http::StatusCode::BAD_REQUEST, "invalid_request_error", "model_required", "model must not be empty");
    }
    if req.messages.is_empty() {
        return api_error(axum::http::StatusCode::BAD_REQUEST, "invalid_request_error", "messages_required", "messages must contain at least one message");
    }
    for (index, message) in req.messages.iter().enumerate() {
        if !matches!(message.role.as_str(), "system" | "developer" | "user" | "assistant" | "tool") {
            return api_error(
                axum::http::StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_message_role",
                format!("messages[{index}].role '{}' is not supported", message.role),
            );
        }
    }
    if req.temperature.is_some() {
        return api_error(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "unsupported_parameter",
            "temperature is not supported by the current browser-backed adapters",
        );
    }

    let conversation_id = headers.get("X-Conversation-Id")
        .and_then(|v| v.to_str().ok()).map(|s| s.to_string());

    // Assemble the provider prompt from the unified memory window. The
    // canonical transcript must be passed to the provider so follow-up
    // turns and provider switches retain conversation context.
    let (context_prompt, windowed) = state.memory.build_context(conversation_id.as_deref(), &req.messages);
    let has_history = conversation_id.as_deref().map(|cid| state.memory.has_history(cid)).unwrap_or(false);
    let prompt = context_prompt;
    if prompt.is_empty() {
        return (axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": { "message": "empty prompt" } }))).into_response();
    }

    let candidates = state.router.resolve(&req.model, conversation_id.as_deref(), &state.memory);
    let provider_model = req.model.split_once('/').map(|(_, model)| model).unwrap_or(req.model.as_str()).to_string();
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

        let mut rx = match state.pool.send_stream(&provider_name, prompt.clone(), provider_model.clone(), has_history).await {
            Ok(rx) => rx,
            Err(e) => {
                last_err = Some(e.to_string());
                continue;
            }
        };

        let new_conv_id = conversation_id.clone()
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
                    state_c.memory.store_turn(
                        conv_stream.clone(),
                        windowed_c.clone(),
                        ChatMessage { role: "assistant".to_string(), content: full.clone() },
                        provider_c.clone(),
                        format!("tab-{}", &conv_stream[..8]),
                    );
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
                    if c.done { break; }
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

        state.memory.store_turn(
            new_conv_id.clone(),
            windowed.clone(),
            ChatMessage { role: "assistant".to_string(), content: full.clone() },
            provider_name.clone(),
            format!("tab-{}", &new_conv_id[..8]),
        );

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

    let err_body = serde_json::json!({
        "error": { "message": format!("all providers failed: {:?}", last_err), "type": "provider_error" }
    });
    (axum::http::StatusCode::BAD_GATEWAY, Json(err_body)).into_response()
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
    fn accepts_minimal_openai_chat_request() {
        let parsed: ChatReq = serde_json::from_value(serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "hello"}]
        })).expect("minimal request should deserialize");
        assert_eq!(parsed.model, "auto");
        assert_eq!(parsed.messages.len(), 1);
        assert!(!parsed.stream);
        assert!(parsed.temperature.is_none());
    }

    #[test]
    fn rejects_unknown_request_fields_instead_of_ignoring_them() {
        let result = serde_json::from_value::<ChatReq>(serde_json::json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": []
        }));
        assert!(result.is_err());
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
