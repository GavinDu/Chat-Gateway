use super::*;
use crate::chat_backend::{
    test_support::{ScriptedBackend, ScriptedSend},
    ChatEvent, GatewayError,
};
use axum::body::to_bytes;
use tower::ServiceExt;

fn configured_provider(name: &str, priority: u32) -> config::ProviderConfig {
    config::ProviderConfig {
        name: name.to_owned(),
        enabled: true,
        priority,
        timeout_seconds: 1,
        completion_idle_ms: 15_000,
        queue_timeout_seconds: 1,
        session_pool_size: 1,
        min_request_interval_ms: 1,
        daily_request_cap: 100,
        models: vec!["fake-model".to_owned()],
        selectors: config::ProviderSelectorOverrides::default(),
    }
}

pub(super) fn fixture(
    providers: &[&str],
    scripted: Vec<ScriptedSend>,
    allowed: Option<Vec<String>>,
) -> (Router, Arc<AppState>) {
    let (app, state, _) = fixture_with_backend(providers, scripted, allowed);
    (app, state)
}

fn fixture_with_backend(
    providers: &[&str],
    scripted: Vec<ScriptedSend>,
    allowed: Option<Vec<String>>,
) -> (Router, Arc<AppState>, Arc<ScriptedBackend>) {
    fixture_with_memory(
        providers,
        scripted,
        allowed,
        Arc::new(UnifiedMemory::new(8_000)),
    )
}

pub(super) fn fixture_with_memory(
    providers: &[&str],
    scripted: Vec<ScriptedSend>,
    allowed: Option<Vec<String>>,
    memory: Arc<UnifiedMemory>,
) -> (Router, Arc<AppState>, Arc<ScriptedBackend>) {
    let cfg = Arc::new(AppConfig {
        server: config::ServerConfig::default(),
        memory: config::MemoryConfig::default(),
        obscura: config::ObscuraConfig::default(),
        auth: config::AuthConfig::default(),
        providers: providers
            .iter()
            .enumerate()
            .map(|(index, provider)| configured_provider(provider, index as u32))
            .collect(),
    });
    let pool = ObscuraPool::for_scripted_backend_tests(cfg.clone());
    // Thread handles are supported by the browser backend itself, but the
    // individual adapter catalog still gates which providers may use them.
    assert!(crate::chat_backend::ChatBackend::capabilities(pool.as_ref()).addressable_threads);
    let (relay, _) = relay::RelayState::new().unwrap();
    let backend = Arc::new(ScriptedBackend::new(scripted));
    let state = Arc::new(AppState {
        cfg: cfg.clone(),
        pool: pool.clone(),
        chat_service: ChatService::new(backend.clone()),
        memory,
        router: ProviderRouter::new(cfg),
        models_cache: Arc::new(RwLock::new(HashMap::new())),
        relay: Arc::new(relay),
        embedded_login: Arc::new(embedded_login::EmbeddedLoginRuntime::default()),
        provider_guard: pool.provider_guard(),
        provider_sessions: pool.provider_sessions(),
        metrics: pool.metrics(),
    });
    // Use the same handler and State extraction as production: no copied
    // validation_endpoint or fake HTTP handler implementation.
    let mut app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state.clone())
        .layer(DefaultBodyLimit::max(1024 * 1024));
    if let Some(provider_allowlist) = allowed {
        app = app.layer(Extension(AuthKeyContext {
            key_id: "local".to_owned(),
            provider_allowlist: Some(provider_allowlist),
        }));
    }
    (app, state, backend)
}

fn response_events(content: &str) -> ScriptedSend {
    ScriptedSend::Events(vec![
        Ok(ChatEvent {
            content: content.to_owned(),
            done: false,
        }),
        Ok(ChatEvent {
            content: String::new(),
            done: true,
        }),
    ])
}

async fn send(app: Router, stream: bool, model: &str, conversation: &str) -> Response {
    send_prompt(app, stream, model, conversation, "synthetic prompt").await
}

async fn send_prompt(
    app: Router,
    stream: bool,
    model: &str,
    conversation: &str,
    prompt: &str,
) -> Response {
    app.oneshot(
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::CONTENT_TYPE, "application/json")
            .header("X-Conversation-Id", conversation)
            .body(Body::from(
                serde_json::json!({
                    "model": model,
                    "stream": stream,
                    "messages": [{"role":"user","content":prompt}]
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await
    .unwrap()
}

async fn memory_request(
    app: &Router,
    owner: &str,
    method: &str,
    uri: &str,
    payload: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .layer(Extension(AuthKeyContext {
            key_id: owner.to_owned(),
            provider_allowlist: None,
        }))
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 65_536).await.unwrap();
    let json = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, json)
}

#[tokio::test]
async fn real_memory_api_owner_scoped_consent_crud_and_export() {
    let (_, state) = fixture(&["chatgpt"], Vec::new(), None);
    let app = Router::new()
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
            "/v1/memory/items/:item_id",
            axum::routing::delete(memory_api::delete_long_term_memory_item),
        )
        .route(
            "/v1/memory/export",
            get(memory_api::export_long_term_memory_items),
        )
        .with_state(state.clone());
    let settings = "/v1/memory/settings";
    let items = "/v1/memory/items";
    let export = "/v1/memory/export";
    let initial = serde_json::json!({"memory_key":"timezone","value":"UTC"});
    let (status, body) =
        memory_request(&app, "owner-a", "GET", settings, serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], false);
    let (status, _) = memory_request(&app, "owner-a", "PUT", items, initial.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT);

    for owner in ["owner-a", "owner-b"] {
        let (status, _) = memory_request(
            &app,
            owner,
            "PUT",
            settings,
            serde_json::json!({"enabled":true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, first) = memory_request(&app, "owner-a", "PUT", items, initial.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let id = first["id"].as_str().unwrap();
    let (status, updated) = memory_request(
        &app,
        "owner-a",
        "PUT",
        items,
        serde_json::json!({"memory_key":"timezone","value":"America/New_York"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["id"], id);
    let (status, foreign) = memory_request(&app, "owner-b", "PUT", items, initial).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(foreign["id"], id);

    let item_uri = format!("{items}/{id}");
    let (status, _) = memory_request(
        &app,
        "owner-b",
        "DELETE",
        &item_uri,
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) =
        memory_request(&app, "owner-a", "GET", export, serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"][0]["id"], id);
    assert_eq!(body["data"][0]["value"], "America/New_York");

    let (status, _) = memory_request(
        &app,
        "owner-a",
        "PUT",
        settings,
        serde_json::json!({"enabled":false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(state
        .memory
        .retrieve_long_term_memory_items("owner-a", 20)
        .unwrap()
        .is_empty());
    let (status, body) =
        memory_request(&app, "owner-a", "GET", items, serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], false);
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    let (status, _) = memory_request(
        &app,
        "owner-a",
        "DELETE",
        items,
        serde_json::json!({"confirm":false}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = memory_request(
        &app,
        "owner-a",
        "DELETE",
        items,
        serde_json::json!({"confirm":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], 1);
    let (_, body) = memory_request(&app, "owner-b", "GET", export, serde_json::Value::Null).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"][0]["id"], foreign["id"]);
}

#[tokio::test]
async fn authenticated_memory_routes_enforce_scope_and_owner_identity() {
    let (_, state) = fixture(&["chatgpt"], Vec::new(), None);
    let auth = config::AuthConfig {
        keys: [
            ("owner-a", "synthetic-admin-a", "admin"),
            ("owner-b", "synthetic-admin-b", "admin"),
            ("chat-only", "synthetic-chat", "chat"),
        ]
        .into_iter()
        .map(|(name, secret, scope)| config::ApiKeyConfig {
            name: name.to_owned(),
            secret_hash: hash_api_key(secret),
            scopes: vec![scope.to_owned()],
            provider_allowlist: None,
        })
        .collect(),
    };
    let app = Router::new()
        .route(
            "/v1/memory/settings",
            get(memory_api::get_long_term_memory_settings)
                .put(memory_api::put_long_term_memory_settings),
        )
        .route(
            "/v1/memory/items",
            axum::routing::put(memory_api::save_long_term_memory_item),
        )
        .route(
            "/v1/memory/items/:item_id",
            axum::routing::delete(memory_api::delete_long_term_memory_item),
        )
        .route(
            "/v1/memory/export",
            get(memory_api::export_long_term_memory_items),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(auth, authenticate_api_key));
    async fn request(
        app: &Router,
        secret: Option<&str>,
        method: &str,
        uri: &str,
        payload: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .header("X-Owner-Key-Id", "key:owner-b");
        if let Some(secret) = secret {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {secret}"));
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::from(payload.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 65_536).await.unwrap();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, json)
    }
    let settings = "/v1/memory/settings";
    let items = "/v1/memory/items";
    let export = "/v1/memory/export";
    for (secret, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some("invalid"), StatusCode::UNAUTHORIZED),
        (Some("synthetic-chat"), StatusCode::FORBIDDEN),
    ] {
        for path in [settings, export] {
            assert_eq!(
                request(&app, secret, "GET", path, serde_json::Value::Null)
                    .await
                    .0,
                expected
            );
        }
        assert_eq!(
            request(
                &app,
                secret,
                "PUT",
                settings,
                serde_json::json!({"enabled":true})
            )
            .await
            .0,
            expected
        );
    }
    for secret in ["synthetic-admin-a", "synthetic-admin-b"] {
        assert_eq!(
            request(
                &app,
                Some(secret),
                "PUT",
                settings,
                serde_json::json!({"enabled":true}),
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let (status, own) = request(
        &app,
        Some("synthetic-admin-a"),
        "PUT",
        items,
        serde_json::json!({"memory_key":"fact","value":"owner-a-value","expires_at":0}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, foreign) = request(
        &app,
        Some("synthetic-admin-b"),
        "PUT",
        items,
        serde_json::json!({"memory_key":"fact","value":"owner-b-value"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(own["id"], foreign["id"]);
    let id = own["id"].as_str().unwrap();
    let item_uri = format!("{items}/{id}");
    assert_eq!(
        request(
            &app,
            Some("synthetic-admin-b"),
            "DELETE",
            &item_uri,
            serde_json::Value::Null,
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, body) = request(
        &app,
        Some("synthetic-admin-a"),
        "GET",
        export,
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"][0]["id"], id);
    assert_eq!(body["data"][0]["value"], "owner-a-value");
    assert!(state
        .memory
        .retrieve_long_term_memory_items("key:owner-a", 20)
        .unwrap()
        .is_empty());
    assert_eq!(
        state
            .memory
            .retrieve_long_term_memory_items("key:owner-b", 20)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        request(
            &app,
            Some("synthetic-admin-a"),
            "DELETE",
            &item_uri,
            serde_json::Value::Null,
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (status, body) = request(
        &app,
        Some("synthetic-admin-b"),
        "GET",
        export,
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"][0]["id"], foreign["id"]);
}

#[tokio::test]
async fn real_router_completion_identity_is_unique_per_turn_for_active_providers() {
    for provider in ["metaai", "chatgpt"] {
        let (app, _) = fixture(
            &[provider],
            (0..4)
                .map(|_| response_events("synthetic answer"))
                .collect(),
            None,
        );
        let mut identities = std::collections::HashSet::new();
        for stream in [false, false, true, true] {
            // The fixture retains production pacing at one millisecond.
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            let response = send(
                app.clone(),
                stream,
                &format!("{provider}/fake-model"),
                "shared_conversation_001",
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()["X-Conversation-Id"],
                "shared_conversation_001"
            );
            let body = to_bytes(response.into_body(), 65_536).await.unwrap();
            let events: Vec<serde_json::Value> = if stream {
                let text = std::str::from_utf8(&body).unwrap();
                assert!(text.contains("data: [DONE]"));
                text.lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter(|data| *data != "[DONE]")
                    .map(|data| serde_json::from_str(data).unwrap())
                    .collect()
            } else {
                vec![serde_json::from_slice(&body).unwrap()]
            };
            assert!(!events.is_empty());
            let first = &events[0];
            let id = first["id"].as_str().unwrap();
            assert!(
                identities.insert(id.to_owned()),
                "separate turns reused a completion ID"
            );
            for event in &events {
                assert_eq!(event["id"], first["id"]);
                assert_eq!(event["created"], first["created"]);
            }
        }
    }
}

#[tokio::test]
async fn real_router_json_success_commits_one_canonical_turn() {
    let (app, state) = fixture(
        &["chatgpt"],
        vec![response_events("synthetic answer")],
        None,
    );
    let response = send(app, false, "chatgpt/fake-model", "router_json_001").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["X-Gateway-Provider"], "chatgpt");
    let body = to_bytes(response.into_body(), 65_536).await.unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        payload["choices"][0]["message"]["content"],
        "synthetic answer"
    );
    let messages = state
        .memory
        .conversation_messages_for_owner("local", "router_json_001")
        .unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content, "synthetic prompt");
    assert_eq!(messages[1].content, "synthetic answer");
}

#[tokio::test]
async fn real_router_terminal_event_with_content_matches_json_and_sse() {
    // A backend may put all response text in its terminal event. Both
    // transports must emit it exactly once and persist the same turn.
    for stream in [false, true] {
        let conversation = if stream {
            "router_terminal_stream_001"
        } else {
            "router_terminal_json_001"
        };
        let (app, state) = fixture(
            &["chatgpt"],
            vec![ScriptedSend::Events(vec![Ok(ChatEvent {
                content: "single terminal answer".to_owned(),
                done: true,
            })])],
            None,
        );
        let response = send(app, stream, "chatgpt/fake-model", conversation).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 65_536).await.unwrap();
        if stream {
            let sse = String::from_utf8(body.to_vec()).unwrap();
            assert_eq!(
                sse.matches(r#""content":"single terminal answer""#).count(),
                1,
                "the terminal event's text must be sent only once"
            );
            assert!(sse.contains("data: [DONE]"));
            assert!(sse.contains(r#""finish_reason":"stop""#));
        } else {
            let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                payload["choices"][0]["message"]["content"],
                "single terminal answer"
            );
        }
        let messages = state
            .memory
            .conversation_messages_for_owner("local", conversation)
            .unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "synthetic prompt");
        assert_eq!(messages[1].content, "single terminal answer");
    }
}

#[tokio::test]
async fn real_router_rejects_duplicate_conversation_id_before_backend_or_storage() {
    let (app, state, backend) = fixture_with_backend(
        &["chatgpt"],
        vec![response_events("this backend must never run")],
        None,
    );
    for (first, second) in [
        ("router_dup_a_001", "router_dup_b_001"),
        ("router_dup_a_001", "router_dup_a_001"),
    ] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "model":"chatgpt/fake-model",
                    "messages":[{"role":"user","content":"synthetic prompt"}]
                })
                .to_string(),
            ))
            .unwrap();
        request
            .headers_mut()
            .append("X-Conversation-Id", first.parse().unwrap());
        request
            .headers_mut()
            .append("X-Conversation-Id", second.parse().unwrap());

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 65_536).await.unwrap();
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"]["code"], "invalid_conversation_id");
    }
    assert!(
        backend.observed_requests().await.is_empty(),
        "ambiguous routing headers must never reach a provider"
    );
    for conversation in ["router_dup_a_001", "router_dup_b_001"] {
        assert!(
            state
                .memory
                .conversation_messages_for_owner("local", conversation)
                .unwrap()
                .is_empty(),
            "ambiguous routing headers must not persist a conversation"
        );
    }
}

#[tokio::test]
async fn real_router_fails_over_on_backend_admission_error_before_output() {
    let (app, state) = fixture(
        &["chatgpt", "claude"],
        vec![
            ScriptedSend::Reject(GatewayError::provider_failure(
                "synthetic backend admission rejection",
            )),
            response_events("answer from second provider"),
        ],
        None,
    );
    let response = send(app, false, "auto", "router_retry_001").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["X-Gateway-Provider"], "claude");
    assert_eq!(
        state
            .memory
            .conversation_messages_for_owner("local", "router_retry_001")
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn real_router_session_health_uses_typed_errors_not_upstream_text() {
    let (app, state) = fixture(
        &["chatgpt"],
        vec![ScriptedSend::Events(vec![Err(
            GatewayError::provider_failure(
                "provider returned the words session expired but did not signal auth expiry",
            ),
        )])],
        None,
    );
    let failed = send(app, false, "chatgpt/fake-model", "typed_failure_001").await;
    assert_eq!(failed.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        state.provider_sessions.status("chatgpt").state,
        provider_state::ProviderSessionState::Degraded
    );

    let (app, state) = fixture(
        &["chatgpt"],
        vec![ScriptedSend::Events(vec![Err(
            GatewayError::session_expired("opaque message without authentication keywords"),
        )])],
        None,
    );
    let failed = send(app, false, "chatgpt/fake-model", "typed_auth_expiry_001").await;
    assert_eq!(failed.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        state.provider_sessions.status("chatgpt").state,
        provider_state::ProviderSessionState::NeedsLogin
    );
}

#[tokio::test]
async fn real_router_fails_over_after_admission_deadline_without_commit() {
    let (app, state, backend) = fixture_with_backend(
        &["chatgpt", "claude"],
        vec![
            ScriptedSend::DelayAdmission(std::time::Duration::from_secs(3), vec![]),
            response_events("synthetic fallback after admission timeout"),
        ],
        None,
    );
    let response = send(app, false, "auto", "router_admission_timeout_001").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["X-Gateway-Provider"], "claude");
    let requests = backend.observed_requests().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].provider.as_str(), "chatgpt");
    assert_eq!(requests[1].provider.as_str(), "claude");
    let persisted = state
        .memory
        .conversation_messages_for_owner("local", "router_admission_timeout_001")
        .unwrap();
    assert_eq!(persisted.len(), 2);
    assert_eq!(
        persisted[1].content,
        "synthetic fallback after admission timeout"
    );
}

#[tokio::test]
async fn real_router_partial_json_error_never_commits_transcript() {
    let (app, state) = fixture(
        &["chatgpt"],
        vec![ScriptedSend::Events(vec![
            Ok(ChatEvent {
                content: "partial private answer".to_owned(),
                done: false,
            }),
            Err(GatewayError::provider_failure("synthetic provider error")),
        ])],
        None,
    );
    let response = send(app, false, "chatgpt/fake-model", "router_partial_json_001").await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = to_bytes(response.into_body(), 65_536).await.unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload["error"]["code"], "partial_response");
    assert!(!String::from_utf8_lossy(&body).contains("partial private answer"));
    assert!(state
        .memory
        .conversation_messages_for_owner("local", "router_partial_json_001")
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn real_router_stream_has_role_delta_done_and_durable_turn() {
    let (app, state) = fixture(&["chatgpt"], vec![response_events("stream answer")], None);
    let response = send(app, true, "chatgpt/fake-model", "router_stream_001").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let body = to_bytes(response.into_body(), 65_536).await.unwrap();
    let content = String::from_utf8(body.to_vec()).unwrap();
    assert!(content.contains(r#""role":"assistant""#));
    assert!(content.contains(r#""content":"stream answer""#));
    assert!(content.contains("data: [DONE]"));
    assert_eq!(
        state
            .memory
            .conversation_messages_for_owner("local", "router_stream_001")
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn real_router_persistence_failure_is_an_error_for_json_and_sse() {
    for stream in [false, true] {
        let memory = Arc::new(UnifiedMemory::new(8_000));
        // No existing conversation is read, so this fixture fails at the
        // completed-turn write *after* the scripted provider returned success.
        memory.drop_conversations_table_for_test().unwrap();
        let (app, _, backend) = fixture_with_memory(
            &["chatgpt"],
            vec![response_events("synthetic upstream completion")],
            None,
            memory,
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "chatgpt/fake-model",
                            "stream": stream,
                            "messages": [{"role": "user", "content": "synthetic prompt"}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(backend.observed_requests().await.len(), 1);
        let expected_status = if stream {
            StatusCode::OK
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        assert_eq!(response.status(), expected_status);
        let body = to_bytes(response.into_body(), 65_536).await.unwrap();
        let content = String::from_utf8(body.to_vec()).unwrap();
        assert!(content.contains("memory_persistence_failed"));
        if stream {
            // Content may have already streamed; no successful terminal
            // completion or durable-turn acknowledgement may follow it.
            assert!(content.contains("data: [DONE]"));
            assert!(!content.contains(r#""finish_reason":"stop""#));
        } else {
            assert!(!content.contains("synthetic upstream completion"));
        }
    }
}

#[tokio::test]
async fn real_router_stream_error_sends_error_event_without_committing_turn() {
    let (app, state) = fixture(
        &["chatgpt"],
        vec![ScriptedSend::Events(vec![
            Ok(ChatEvent {
                content: "partial".to_owned(),
                done: false,
            }),
            Err(GatewayError::provider_failure("provider stopped")),
        ])],
        None,
    );
    let response = send(app, true, "chatgpt/fake-model", "router_stream_error_001").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 65_536).await.unwrap();
    let content = String::from_utf8(body.to_vec()).unwrap();
    assert!(content.contains("stream_failed"));
    assert!(content.contains("data: [DONE]"));
    assert!(state
        .memory
        .conversation_messages_for_owner("local", "router_stream_error_001")
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn real_router_honors_key_provider_allowlist_before_fake_send() {
    let (app, state) = fixture(
        &["chatgpt", "claude"],
        vec![response_events("allowed answer")],
        Some(vec!["claude".to_owned()]),
    );
    let response = send(app, false, "auto", "router_allowlist_001").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["X-Gateway-Provider"], "claude");
    assert_eq!(
        state
            .memory
            .conversation_messages_for_owner("local", "router_allowlist_001")
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn real_router_client_drop_cancels_sse_and_releases_provider_permit() {
    let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
    let (app, state) = fixture(
        &["chatgpt"],
        vec![ScriptedSend::PartialThenHold(
            ChatEvent {
                content: "partial first delta".to_owned(),
                done: false,
            },
            cancelled_tx,
        )],
        None,
    );
    let response = send(app, true, "chatgpt/fake-model", "router_drop_sse_001").await;
    assert_eq!(response.status(), StatusCode::OK);
    // The handler prefetched first content, then returned the real SSE
    // response. Simulate client disconnect before a terminal [DONE].
    drop(response);
    tokio::time::timeout(std::time::Duration::from_secs(2), cancelled_rx)
        .await
        .expect("client drop should close backend receiver")
        .expect("scripted backend should observe cancellation");
    let _reacquired = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.pool.acquire("chatgpt"),
    )
    .await
    .expect("client cancellation must release browser permit")
    .expect("provider permit should be available again");
    assert!(state
        .memory
        .conversation_messages_for_owner("local", "router_drop_sse_001")
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn real_router_dropped_delta_invalidates_sync_and_retries_with_replay() {
    let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
    let (app, state, backend) = fixture_with_backend(
        &["chatgpt"],
        vec![
            ScriptedSend::PartialThenHold(
                ChatEvent {
                    content: "discarded remote partial".to_owned(),
                    done: false,
                },
                cancelled_tx,
            ),
            response_events("recovered answer"),
        ],
        None,
    );
    let conversation = "router_delta_cancel_001";
    let history = vec![
        ChatMessage {
            role: "user".to_owned(),
            content: "SYNTHETIC_PRIOR_TURN".to_owned(),
        },
        ChatMessage {
            role: "assistant".to_owned(),
            content: "prior answer".to_owned(),
        },
    ];
    state
        .memory
        .store_turn_for_owner_with_tab_id(
            "local",
            conversation.to_owned(),
            vec![history[0].clone()],
            history[1].clone(),
            "chatgpt".to_owned(),
            None,
        )
        .unwrap();
    let url = "https://chatgpt.com/c/synthetic-delta-thread";
    state
        .memory
        .upsert_provider_thread_for_owner(
            "local",
            &ProviderThread {
                conversation_id: conversation.to_owned(),
                provider: "chatgpt".to_owned(),
                thread_url: url.to_owned(),
                synced_through_message_id: Some(transcript_marker(&history)),
                model: Some("chatgpt/fake-model".to_owned()),
                updated_at: 1,
            },
        )
        .unwrap();

    let response = send_prompt(
        app.clone(),
        true,
        "chatgpt/fake-model",
        conversation,
        "SYNTHETIC_INTERRUPTED_TURN",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let requests = backend.observed_requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].thread_url.as_deref(), Some(url));
    assert!(!requests[0].prompt.contains("SYNTHETIC_PRIOR_TURN"));
    assert!(state
        .memory
        .get_provider_thread_for_owner("local", conversation, "chatgpt")
        .unwrap()
        .unwrap()
        .synced_through_message_id
        .is_none());

    drop(response);
    tokio::time::timeout(std::time::Duration::from_secs(2), cancelled_rx)
        .await
        .expect("client drop should close backend receiver")
        .expect("scripted backend should observe cancellation");
    assert_eq!(
        state
            .memory
            .conversation_messages_for_owner("local", conversation)
            .unwrap(),
        history
    );

    let recovered = send_prompt(
        app,
        false,
        "chatgpt/fake-model",
        conversation,
        "SYNTHETIC_RECOVERY_TURN",
    )
    .await;
    assert_eq!(recovered.status(), StatusCode::OK);
    let requests = backend.observed_requests().await;
    assert_eq!(requests.len(), 2);
    assert!(requests[1].thread_url.is_none());
    assert!(requests[1].prompt.contains("SYNTHETIC_PRIOR_TURN"));
    assert!(!requests[1].prompt.contains("SYNTHETIC_INTERRUPTED_TURN"));
    assert!(!requests[1].prompt.contains("discarded remote partial"));
    let persisted = state
        .memory
        .conversation_messages_for_owner("local", conversation)
        .unwrap();
    assert_eq!(persisted.len(), 4);
    assert_eq!(persisted[3].content, "recovered answer");
}

#[tokio::test]
async fn imported_thread_marker_cannot_rearm_consumed_delta() {
    let (app, state, backend) = fixture_with_backend(
        &["chatgpt"],
        vec![response_events("recovered answer")],
        None,
    );
    let conversation = "router_imported_thread_001";
    let history = vec![
        ChatMessage {
            role: "user".to_owned(),
            content: "SYNTHETIC_CANONICAL_HISTORY".to_owned(),
        },
        ChatMessage {
            role: "assistant".to_owned(),
            content: "prior answer".to_owned(),
        },
    ];
    state
        .memory
        .store_turn_for_owner_with_tab_id(
            "local",
            conversation.to_owned(),
            vec![history[0].clone()],
            history[1].clone(),
            "chatgpt".to_owned(),
            None,
        )
        .unwrap();
    let mut thread = ProviderThread {
        conversation_id: conversation.to_owned(),
        provider: "chatgpt".to_owned(),
        thread_url: "https://chatgpt.com/c/synthetic-original".to_owned(),
        synced_through_message_id: Some(transcript_marker(&history)),
        model: Some("chatgpt/fake-model".to_owned()),
        updated_at: 1,
    };
    state
        .memory
        .upsert_provider_thread_for_owner("local", &thread)
        .unwrap();
    assert!(state
        .memory
        .clone()
        .claim_provider_thread_sync_async("local".to_owned(), thread.clone())
        .await
        .unwrap());

    let mapping_router = Router::new()
        .route(
            "/v1/conversations/:conversation_id/provider-threads/:provider",
            get(get_provider_thread).put(upsert_provider_thread),
        )
        .with_state(state.clone());
    thread.thread_url = "https://chatgpt.com/c/synthetic-unverified".to_owned();
    let uri = format!("/v1/conversations/{conversation}/provider-threads/chatgpt");
    let request = || {
        Request::builder()
            .method("PUT")
            .uri(uri.as_str())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_string(&thread).unwrap()))
            .unwrap()
    };
    let foreign_response = mapping_router
        .clone()
        .layer(Extension(AuthKeyContext {
            key_id: "foreign-owner".to_owned(),
            provider_allowlist: None,
        }))
        .oneshot(request())
        .await
        .unwrap();
    assert!(!foreign_response.status().is_success());
    let unchanged = state
        .memory
        .get_provider_thread_for_owner("local", conversation, "chatgpt")
        .unwrap()
        .unwrap();
    assert_eq!(
        unchanged.thread_url,
        "https://chatgpt.com/c/synthetic-original"
    );
    assert!(unchanged.synced_through_message_id.is_none());

    let response = mapping_router.oneshot(request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 65_536).await.unwrap();
    let imported: ProviderThread = serde_json::from_slice(&body).unwrap();
    assert_eq!(imported.thread_url, thread.thread_url);
    assert!(imported.synced_through_message_id.is_none());
    let persisted = state
        .memory
        .get_provider_thread_for_owner("local", conversation, "chatgpt")
        .unwrap()
        .unwrap();
    assert_eq!(persisted.thread_url, thread.thread_url);
    assert!(persisted.synced_through_message_id.is_none());

    let recovered = send_prompt(
        app,
        false,
        "chatgpt/fake-model",
        conversation,
        "SYNTHETIC_NEW_TURN",
    )
    .await;
    assert_eq!(recovered.status(), StatusCode::OK);
    let requests = backend.observed_requests().await;
    assert_eq!(requests.len(), 1);
    assert!(requests[0].thread_url.is_none());
    assert!(requests[0].prompt.contains("SYNTHETIC_CANONICAL_HISTORY"));
}

#[tokio::test]
async fn real_router_cancelled_absent_admission_allows_explicit_retry() {
    let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
    let (app, state, _) = fixture_with_backend(
        &["chatgpt"],
        vec![
            ScriptedSend::PartialThenHold(
                ChatEvent {
                    content: "discarded first answer".to_owned(),
                    done: false,
                },
                cancelled_tx,
            ),
            response_events("fresh answer"),
        ],
        None,
    );
    let conversation = "router_absent_cancel_001";
    let response = send_prompt(
        app.clone(),
        true,
        "chatgpt/fake-model",
        conversation,
        "INTERRUPTED_FIRST_TURN",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    tokio::time::timeout(std::time::Duration::from_secs(2), cancelled_rx)
        .await
        .expect("client drop should close backend receiver")
        .expect("scripted backend should observe cancellation");
    assert!(state
        .memory
        .conversation_messages_for_owner("local", conversation)
        .unwrap()
        .is_empty());
    assert!(state
        .memory
        .get_provider_thread_for_owner("local", conversation, "chatgpt")
        .unwrap()
        .is_none());

    let retry = send_prompt(
        app,
        false,
        "chatgpt/fake-model",
        conversation,
        "EXPLICIT_RETRY_TURN",
    )
    .await;
    assert_eq!(retry.status(), StatusCode::OK);
    assert_eq!(
        state
            .memory
            .conversation_messages_for_owner("local", conversation)
            .unwrap(),
        vec![
            ChatMessage {
                role: "user".to_owned(),
                content: "EXPLICIT_RETRY_TURN".to_owned(),
            },
            ChatMessage {
                role: "assistant".to_owned(),
                content: "fresh answer".to_owned(),
            },
        ]
    );
}

#[tokio::test]
async fn real_router_absent_admission_cannot_resurrect_deleted_conversation() {
    for stream in [false, true] {
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (app, state, _) = fixture_with_backend(
            &["chatgpt", "claude"],
            vec![
                ScriptedSend::PartialThenRelease(
                    ChatEvent {
                        content: "stale first answer".to_owned(),
                        done: false,
                    },
                    release_rx,
                    Some(started_tx),
                ),
                response_events("intervening answer"),
            ],
            None,
        );
        let app = app.merge(
            Router::new()
                .route(
                    "/v1/conversations/:conversation_id",
                    get(export_conversation).delete(delete_conversation),
                )
                .with_state(state.clone()),
        );
        let conversation = "router_initially_absent_001";
        let pending = send_prompt(
            app.clone(),
            stream,
            "chatgpt/fake-model",
            conversation,
            "STALE_FIRST_TURN",
        );
        tokio::pin!(pending);
        let mut early_response = None;
        tokio::select! {
            response = &mut pending => {
                assert!(stream);
                early_response = Some(response);
            }
            started = started_rx => started.unwrap(),
        }
        let created = send_prompt(
            app.clone(),
            false,
            "claude/fake-model",
            conversation,
            "INTERVENING_FIRST_TURN",
        )
        .await;
        assert_eq!(created.status(), StatusCode::OK);
        let deleted = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/v1/conversations/{conversation}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        release_tx.send(()).unwrap();
        let response = match early_response {
            Some(response) => response,
            None => pending.await,
        };
        assert_eq!(
            response.status(),
            if stream {
                StatusCode::OK
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        );
        let body = to_bytes(response.into_body(), 65_536).await.unwrap();
        let content = String::from_utf8(body.to_vec()).unwrap();
        assert!(content.contains("memory_persistence_failed"));
        assert!(!content.contains(r#""finish_reason":"stop""#));
        assert!(state
            .memory
            .conversation_messages_for_owner("local", conversation)
            .unwrap()
            .is_empty());
        assert!(state
            .memory
            .get_provider_thread_for_owner("local", conversation, "chatgpt")
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn real_router_stale_json_cannot_resurrect_or_replace_deleted_conversation() {
    for recreate in [false, true] {
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (app, state, _) = fixture_with_backend(
            &["chatgpt", "claude"],
            vec![
                response_events("initial answer"),
                ScriptedSend::PartialThenRelease(
                    ChatEvent {
                        content: "stale JSON answer".to_owned(),
                        done: false,
                    },
                    release_rx,
                    Some(started_tx),
                ),
                response_events("initial answer"),
            ],
            None,
        );
        let app = app.merge(
            Router::new()
                .route(
                    "/v1/conversations/:conversation_id",
                    get(export_conversation).delete(delete_conversation),
                )
                .with_state(state.clone()),
        );
        let conversation = "router_json_incarnation_001";
        let initial = send_prompt(
            app.clone(),
            false,
            "claude/fake-model",
            conversation,
            "INITIAL_CANONICAL_HISTORY",
        )
        .await;
        assert_eq!(initial.status(), StatusCode::OK);
        let original = state
            .memory
            .conversation_messages_for_owner("local", conversation)
            .unwrap();
        let stale = send_prompt(
            app.clone(),
            false,
            "chatgpt/fake-model",
            conversation,
            "STALE_JSON_TURN",
        );
        tokio::pin!(stale);
        // Poll the actual JSON handler until the backend acknowledges admission.
        // The terminal event stays held until after the deletion/recreation.
        tokio::select! {
            _ = &mut stale => panic!("JSON request completed before release"),
            started = started_rx => started.unwrap(),
        }
        let deleted = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/v1/conversations/{conversation}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        if recreate {
            let response = send_prompt(
                app,
                false,
                "claude/fake-model",
                conversation,
                "INITIAL_CANONICAL_HISTORY",
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        release_tx.send(()).unwrap();
        let response = stale.await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), 65_536).await.unwrap();
        let content = String::from_utf8(body.to_vec()).unwrap();
        assert!(content.contains("memory_persistence_failed"));
        assert!(!content.contains("stale JSON answer"));
        let history = state
            .memory
            .conversation_messages_for_owner("local", conversation)
            .unwrap();
        if recreate {
            assert_eq!(history, original);
        } else {
            assert!(history.is_empty());
        }
        assert!(state
            .memory
            .get_provider_thread_for_owner("local", conversation, "chatgpt")
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn real_router_external_import_during_sse_keeps_mapping_unverified() {
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (app, state, _) = fixture_with_backend(
        &["chatgpt", "claude"],
        vec![
            response_events("initial answer"),
            ScriptedSend::PartialThenRelease(
                ChatEvent {
                    content: "completed answer".to_owned(),
                    done: false,
                },
                release_rx,
                None,
            ),
        ],
        None,
    );
    let app = app.merge(
        Router::new()
            .route(
                "/v1/conversations/:conversation_id/provider-threads/:provider",
                get(get_provider_thread).put(upsert_provider_thread),
            )
            .with_state(state.clone()),
    );
    let conversation = "router_pending_import_001";
    let initial = send_prompt(
        app.clone(),
        false,
        "claude/fake-model",
        conversation,
        "INITIAL_CANONICAL_HISTORY",
    )
    .await;
    assert_eq!(initial.status(), StatusCode::OK);
    let pending = send_prompt(
        app.clone(),
        true,
        "chatgpt/fake-model",
        conversation,
        "PENDING_IMPORT_TURN",
    )
    .await;
    assert_eq!(pending.status(), StatusCode::OK);
    let thread = ProviderThread {
        conversation_id: conversation.to_owned(),
        provider: "chatgpt".to_owned(),
        thread_url: "https://chatgpt.com/c/imported-pending".to_owned(),
        synced_through_message_id: Some("forged-marker".to_owned()),
        model: Some("chatgpt/fake-model".to_owned()),
        updated_at: 1,
    };
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!(
                    "/v1/conversations/{conversation}/provider-threads/chatgpt"
                ))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_string(&thread).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 65_536).await.unwrap();
    let imported: ProviderThread = serde_json::from_slice(&body).unwrap();
    assert!(imported.synced_through_message_id.is_none());

    release_tx.send(()).unwrap();
    let body = to_bytes(pending.into_body(), 65_536).await.unwrap();
    let content = String::from_utf8(body.to_vec()).unwrap();
    assert!(content.contains("completed answer"));
    assert!(!content.contains("memory_persistence_failed"));
    assert!(content.contains(r#""finish_reason":"stop""#));
    let stored = state
        .memory
        .get_provider_thread_for_owner("local", conversation, "chatgpt")
        .unwrap()
        .unwrap();
    assert_eq!(stored, imported);
    let history = state
        .memory
        .conversation_messages_for_owner("local", conversation)
        .unwrap();
    assert_eq!(history.len(), 4);
    assert_eq!(history[3].content, "completed answer");
}

#[tokio::test]
async fn real_router_stale_sse_cannot_resurrect_or_replace_deleted_conversation() {
    for recreate in [false, true] {
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (app, state, backend) = fixture_with_backend(
            &["chatgpt", "claude"],
            vec![
                response_events("initial answer"),
                ScriptedSend::PartialThenRelease(
                    ChatEvent {
                        content: "stale answer".to_owned(),
                        done: false,
                    },
                    release_rx,
                    None,
                ),
                response_events("initial answer"),
            ],
            None,
        );
        let app = app.merge(
            Router::new()
                .route(
                    "/v1/conversations/:conversation_id",
                    get(export_conversation).delete(delete_conversation),
                )
                .with_state(state.clone()),
        );
        let conversation = "router_incarnation_aba_001";
        let initial = send_prompt(
            app.clone(),
            false,
            "claude/fake-model",
            conversation,
            "INITIAL_CANONICAL_HISTORY",
        )
        .await;
        assert_eq!(initial.status(), StatusCode::OK);
        let original = state
            .memory
            .conversation_messages_for_owner("local", conversation)
            .unwrap();
        let stale = send_prompt(
            app.clone(),
            true,
            "chatgpt/fake-model",
            conversation,
            "STALE_IN_FLIGHT_TURN",
        )
        .await;
        assert_eq!(stale.status(), StatusCode::OK);
        let deleted = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/v1/conversations/{conversation}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        if recreate {
            let response = send_prompt(
                app.clone(),
                false,
                "claude/fake-model",
                conversation,
                "INITIAL_CANONICAL_HISTORY",
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        let before = state
            .memory
            .conversation_messages_for_owner("local", conversation)
            .unwrap();
        if recreate {
            assert_eq!(before, original);
        } else {
            assert!(before.is_empty());
        }

        release_tx.send(()).unwrap();
        let body = to_bytes(stale.into_body(), 65_536).await.unwrap();
        let content = String::from_utf8(body.to_vec()).unwrap();
        assert!(content.contains("memory_persistence_failed"));
        assert!(content.contains("data: [DONE]"));
        assert!(!content.contains(r#""finish_reason":"stop""#));
        assert_eq!(
            state
                .memory
                .conversation_messages_for_owner("local", conversation)
                .unwrap(),
            before
        );
        assert!(state
            .memory
            .get_provider_thread_for_owner("local", conversation, "chatgpt")
            .unwrap()
            .is_none());
        assert_eq!(
            backend.observed_requests().await.len(),
            if recreate { 3 } else { 2 }
        );
    }
}

#[tokio::test]
async fn real_router_stale_sse_completion_invalidates_mapping_after_json_handoff() {
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (app, state, backend) = fixture_with_backend(
        &["chatgpt", "claude"],
        vec![
            response_events("initial answer"),
            ScriptedSend::PartialThenRelease(
                ChatEvent {
                    content: "delayed SSE answer".to_owned(),
                    done: false,
                },
                release_rx,
                None,
            ),
            response_events("earlier JSON answer"),
        ],
        None,
    );
    let conversation = "router_cross_provider_snapshot_001";
    let initial = send_prompt(
        app.clone(),
        false,
        "claude/fake-model",
        conversation,
        "INITIAL_CANONICAL_HISTORY",
    )
    .await;
    assert_eq!(initial.status(), StatusCode::OK);
    let stale = send_prompt(
        app.clone(),
        true,
        "chatgpt/fake-model",
        conversation,
        "DELAYED_SSE_TURN",
    )
    .await;
    assert_eq!(stale.status(), StatusCode::OK);

    // The SSE response holds its provider permit and has captured history,
    // but its terminal event has not been released. Complete another provider.
    let earlier = send_prompt(
        app,
        false,
        "claude/fake-model",
        conversation,
        "EARLIER_JSON_TURN",
    )
    .await;
    assert_eq!(earlier.status(), StatusCode::OK);
    let committed = state
        .memory
        .conversation_messages_for_owner("local", conversation)
        .unwrap();
    assert_eq!(committed.len(), 4);
    let marker = transcript_marker(&committed);
    let mapping = ProviderThread {
        conversation_id: conversation.to_owned(),
        provider: "chatgpt".to_owned(),
        thread_url: "https://chatgpt.com/c/synthetic-concurrent".to_owned(),
        synced_through_message_id: Some(marker),
        model: Some("chatgpt/fake-model".to_owned()),
        updated_at: 1,
    };
    state
        .memory
        .upsert_provider_thread_for_owner("local", &mapping)
        .unwrap();

    release_tx.send(()).unwrap();
    let body = to_bytes(stale.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("delayed SSE answer"));
    assert!(body.contains("[DONE]"));
    assert!(!body.contains("memory_persistence_failed"));

    let combined = state
        .memory
        .conversation_messages_for_owner("local", conversation)
        .unwrap();
    assert_eq!(combined.len(), 6);
    assert_eq!(&combined[..4], committed.as_slice());
    assert_eq!(combined[5].content, "delayed SSE answer");
    let invalidated = state
        .memory
        .get_provider_thread_for_owner("local", conversation, "chatgpt")
        .unwrap()
        .unwrap();
    assert_eq!(invalidated.thread_url, mapping.thread_url);
    assert!(invalidated.synced_through_message_id.is_none());
    let requests = backend.observed_requests().await;
    assert_eq!(requests.len(), 3);
    assert!(requests[1].prompt.contains("INITIAL_CANONICAL_HISTORY"));
    assert!(!requests[1].prompt.contains("EARLIER_JSON_TURN"));
    assert!(requests[2].prompt.contains("INITIAL_CANONICAL_HISTORY"));
    assert!(!requests[2].prompt.contains("DELAYED_SSE_TURN"));
}

#[tokio::test]
async fn real_router_interleaved_conversations_never_send_other_threads_context() {
    let (app, state, backend) = fixture_with_backend(
        &["chatgpt"],
        vec![
            response_events("answer A1"),
            response_events("answer B1"),
            response_events("answer A2"),
            response_events("answer B2"),
        ],
        None,
    );
    let a = "router_context_a_001";
    let b = "router_context_b_001";
    for (id, prompt) in [
        (a, "SYNTHETIC_PRIVATE_MARKER_A_314"),
        (b, "SYNTHETIC_PRIVATE_MARKER_B_159"),
        (a, "repeat the first A marker"),
        (b, "repeat the first B marker"),
    ] {
        let response = send_prompt(app.clone(), false, "chatgpt/fake-model", id, prompt).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    let requests = backend.observed_requests().await;
    assert_eq!(requests.len(), 4);
    assert!(requests[2]
        .prompt
        .contains("SYNTHETIC_PRIVATE_MARKER_A_314"));
    assert!(!requests[2]
        .prompt
        .contains("SYNTHETIC_PRIVATE_MARKER_B_159"));
    assert!(requests[3]
        .prompt
        .contains("SYNTHETIC_PRIVATE_MARKER_B_159"));
    assert!(!requests[3]
        .prompt
        .contains("SYNTHETIC_PRIVATE_MARKER_A_314"));
    for conversation in [a, b] {
        let messages = state
            .memory
            .conversation_messages_for_owner("local", conversation)
            .unwrap();
        assert_eq!(messages.len(), 4);
    }
}

#[tokio::test]
async fn real_router_provider_switch_replays_bounded_canonical_history() {
    let (app, state, backend) = fixture_with_backend(
        &["chatgpt", "claude"],
        vec![
            response_events("synthetic answer from chatgpt"),
            response_events("synthetic answer from claude"),
        ],
        None,
    );
    let conversation = "router_handoff_001";
    let first = send_prompt(
        app.clone(),
        false,
        "chatgpt/fake-model",
        conversation,
        "SYNTHETIC_HANDOFF_MARKER_123",
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let second = send_prompt(
        app,
        false,
        "claude/fake-model",
        conversation,
        "Please continue after the provider switch",
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);

    let requests = backend.observed_requests().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].provider.as_str(), "chatgpt");
    assert_eq!(requests[1].provider.as_str(), "claude");
    assert!(requests[1].prompt.contains("SYNTHETIC_HANDOFF_MARKER_123"));
    assert!(requests[1].prompt.contains("synthetic answer from chatgpt"));
    assert!(requests[1]
        .prompt
        .contains("Please continue after the provider switch"));
    let persisted = state
        .memory
        .conversation_messages_for_owner("local", conversation)
        .unwrap();
    assert_eq!(persisted.len(), 4);
    assert_eq!(persisted[3].content, "synthetic answer from claude");
}

#[tokio::test]
async fn real_router_failed_handoff_never_advances_canonical_transcript() {
    let (app, state, backend) = fixture_with_backend(
        &["chatgpt", "claude"],
        vec![
            response_events("first committed answer"),
            ScriptedSend::Events(vec![
                Ok(ChatEvent {
                    content: "partial discarded answer".to_owned(),
                    done: false,
                }),
                Err(GatewayError::provider_failure("synthetic interrupted send")),
            ]),
            response_events("recovered answer"),
        ],
        None,
    );
    let conversation = "router_failed_handoff_001";
    let first = send_prompt(
        app.clone(),
        false,
        "chatgpt/fake-model",
        conversation,
        "SYNTHETIC_FIRST_TURN",
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let failed = send_prompt(
        app.clone(),
        false,
        "claude/fake-model",
        conversation,
        "SYNTHETIC_UNCOMMITTED_TURN",
    )
    .await;
    assert_eq!(failed.status(), StatusCode::BAD_GATEWAY);
    let before = state
        .memory
        .conversation_messages_for_owner("local", conversation)
        .unwrap();
    assert_eq!(before.len(), 2, "failed handoff must not commit a turn");

    let recovered = send_prompt(
        app,
        false,
        "chatgpt/fake-model",
        conversation,
        "SYNTHETIC_RECOVERED_TURN",
    )
    .await;
    assert_eq!(recovered.status(), StatusCode::OK);
    let requests = backend.observed_requests().await;
    assert_eq!(requests.len(), 3);
    assert!(!requests[2].prompt.contains("SYNTHETIC_UNCOMMITTED_TURN"));
    assert!(!requests[2].prompt.contains("partial discarded answer"));
    let after = state
        .memory
        .conversation_messages_for_owner("local", conversation)
        .unwrap();
    assert_eq!(after.len(), 4);
    assert_eq!(after[3].content, "recovered answer");
}

#[tokio::test]
async fn real_router_bounds_backend_admission_and_releases_provider_slot() {
    for (stream, conversation) in [
        (false, "router_admission_json_001"),
        (true, "router_admission_sse_001"),
    ] {
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let (app, state, backend) = fixture_with_backend(
            &["chatgpt"],
            vec![ScriptedSend::HangOnAdmission(cancel_tx)],
            None,
        );
        let started = Instant::now();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            send(app, stream, "chatgpt/fake-model", conversation),
        )
        .await
        .expect("provider admission must be bounded");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), cancel_rx,)
                .await
                .expect("pending backend send must be cancelled")
                .is_err()
        );
        let _slot = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            state.pool.acquire("chatgpt"),
        )
        .await
        .expect("admission timeout must release provider permit")
        .expect("provider slot must be available again");
        assert_eq!(backend.observed_requests().await.len(), 1);
        assert!(state
            .memory
            .conversation_messages_for_owner("local", conversation)
            .unwrap()
            .is_empty());
    }
}

#[tokio::test]
async fn real_router_busy_provider_returns_retry_after_and_no_commit() {
    let (app, state) = fixture(&["chatgpt"], vec![], None);
    let _held = state.pool.acquire("chatgpt").await.unwrap();
    let response = send(app, false, "chatgpt/fake-model", "router_busy_001").await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().contains_key(header::RETRY_AFTER));
    assert!(state
        .memory
        .conversation_messages_for_owner("local", "router_busy_001")
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn real_router_telemetry_separates_request_purpose_waits_and_first_content() {
    for (stream, reported_kind, expected_kind, transport) in [
        (true, "primary", "primary", "sse"),
        (false, "private-user-session-id", "unspecified", "json"),
    ] {
        let (app, state) = fixture(&["metaai"], vec![response_events("verified answer")], None);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-chat-gateway-request-kind", reported_kind)
                    .body(Body::from(
                        serde_json::json!({
                            "model":"metaai/fake-model", "stream":stream,
                            "messages":[{"role":"user","content":"PRIVATE_PROMPT_SENTINEL"}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), 65_536).await.unwrap();
        let metrics = state.metrics.render_with_provider_states(&[]);
        assert!(metrics.contains(&format!("gateway_chat_input_requests_total{{kind=\"{expected_kind}\",transport=\"{transport}\"}} 1")));
        assert!(metrics.contains(&format!("gateway_request_ttft_ms_count{{provider=\"metaai\",kind=\"{expected_kind}\",transport=\"{transport}\"}} 1")));
        for phase in [
            "pacing",
            "slot",
            "provider_context",
            "backend_admission",
            "persistence",
        ] {
            assert!(metrics.contains(&format!("gateway_chat_phase_duration_ms_count{{provider=\"metaai\",phase=\"{phase}\"}} 1")), "missing phase {phase}");
        }
        assert!(!metrics.contains("PRIVATE_PROMPT_SENTINEL"));
        assert!(!metrics.contains("private-user-session-id"));
    }
}
