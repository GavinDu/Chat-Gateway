use super::*;
use crate::chat_backend::{
    test_support::{ScriptedBackend, ScriptedSend},
    ChatEvent, GatewayError,
};
use axum::body::to_bytes;
use tower::ServiceExt;

fn configured_provider(name: &str, priority: u32) -> config::ProviderConfig {
    config::ProviderConfig {
        transport: crate::config::ProviderTransport::Browser,
        oauth_model: None,
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
