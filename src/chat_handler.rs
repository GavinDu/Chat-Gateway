//! HTTP chat orchestration: provider routing, ownership, egress policy,
//! canonical response consumption and transactional turn persistence.
//!
//! This module retains the production implementation while progressively
//! moving backend-neutral decisions into `chat_service`. The public Axum
//! extractor stays in `main.rs` and is exercised by the existing real-router
//! scripted-backend tests.
use super::*;
use crate::chat_render::{completion_response_headers, json_completion_body, sse_completion_chunk};
use crate::chat_request::{prepare_chat_request, PreparedChatRequest};

mod json;
mod streaming;
use json::json_provider_response;
use streaming::streamed_provider_response;

/// Owned SSE attempt state. Moving it outside the routing loop ensures the
/// provider permit is held by the stream and released on client cancellation.
struct StreamingAttempt {
    state: Arc<AppState>,
    owner_key_id: String,
    windowed: Vec<ChatMessage>,
    provider: String,
    model: String,
    provider_model: String,
    conversation_id: String,
    request_id: String,
    request_started: Instant,
    provider_started: Instant,
    completion_timeout: std::time::Duration,
    queue_ms: u64,
    prompt_tokens: u64,
    permit: browser::pool::ObscuraPermit,
    receiver:
        tokio::sync::mpsc::Receiver<Result<chat_backend::ChatEvent, chat_backend::GatewayError>>,
    first_chunk: chat_backend::ChatEvent,
}

/// One non-streaming provider attempt. A failure before any content may be
/// retried on another candidate; a partial response is never committed.
struct JsonAttempt<'a> {
    state: &'a AppState,
    owner_key_id: &'a str,
    windowed: &'a [ChatMessage],
    provider: &'a str,
    model: &'a str,
    provider_model: &'a str,
    conversation_id: &'a str,
    request_id: &'a str,
    request_started: Instant,
    provider_started: Instant,
    completion_timeout: std::time::Duration,
    queue_ms: u64,
    prompt_tokens: u64,
    receiver:
        tokio::sync::mpsc::Receiver<Result<chat_backend::ChatEvent, chat_backend::GatewayError>>,
}

pub(super) async fn run(
    state: Arc<AppState>,
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
    let PreparedChatRequest {
        req,
        owner_key_id,
        conversation_id,
    } = match prepare_chat_request(&headers, auth_context.as_ref(), body) {
        Ok(prepared) => prepared,
        Err(response) => return *response,
    };

    let chat_service::PreparedChatContext {
        memory_egress_policy,
        personal_memory_items,
        context_token_budget,
        prompt,
        windowed,
        has_history,
        stored_messages,
    } = match chat_service::prepare_chat_context(
        &state,
        &owner_key_id,
        conversation_id.as_deref(),
        &req.messages,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(response) => return *response,
    };

    let (candidates, provider_model) = match chat_service::resolve_chat_candidates(
        &state,
        &owner_key_id,
        &req.model,
        conversation_id.as_deref(),
        auth_context.as_ref(),
    )
    .await
    {
        Ok(result) => result,
        Err(response) => return *response,
    };

    let candidate_count = candidates.len();
    let mut busy_count = 0usize;
    let mut last_err: Option<String> = None;
    for provider_name in candidates {
        let chat_service::CandidateAttempt {
            permit,
            queue_ms,
            prompt: provider_prompt,
            thread_url,
            timeout: provider_timeout,
        } = match chat_service::prepare_candidate_attempt(chat_service::CandidateAttemptRequest {
            state: &state,
            provider: &provider_name,
            owner_key_id: &owner_key_id,
            conversation_id: conversation_id.as_deref(),
            model: &req.model,
            stored_messages: &stored_messages,
            incoming_messages: &req.messages,
            token_budget: context_token_budget,
            replay_prompt: &prompt,
            policy: &memory_egress_policy,
            personal_memory_items: &personal_memory_items,
        })
        .await
        {
            Ok(attempt) => attempt,
            Err(chat_service::CandidateAttemptError::Skip { message, busy }) => {
                if busy {
                    busy_count += 1;
                }
                last_err = Some(message);
                continue;
            }
            Err(chat_service::CandidateAttemptError::Fatal(response)) => return *response,
        };

        let provider_started = Instant::now();
        let mut rx = match state
            .chat_service
            .stream_with_timeout(
                BackendRequest {
                    provider: match ProviderId::new(provider_name.clone()) {
                        Some(provider) => provider,
                        None => {
                            last_err = Some(
                                "provider router returned an invalid provider identifier"
                                    .to_owned(),
                            );
                            continue;
                        }
                    },
                    prompt: provider_prompt.clone(),
                    model: provider_model.clone(),
                    has_history: has_history && thread_url.is_some(),
                    thread_url,
                },
                provider_timeout,
            )
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                chat_service::record_backend_admission_error(
                    &state.provider_sessions,
                    &state.metrics,
                    &provider_name,
                    &e,
                    queue_ms,
                    provider_started,
                    request_started,
                );
                last_err = Some(e.to_string());
                continue;
            }
        };

        let mut first_stream_chunk: Option<chat_backend::ChatEvent> = None;
        if req.stream {
            let first_chunk_timeout = provider_timeout.saturating_sub(provider_started.elapsed());
            let first = match chat_service::classify_initial_stream_event(
                first_event(&mut rx, first_chunk_timeout).await,
            ) {
                Ok(chunk) => chunk,
                Err((message, gateway_error)) => {
                    record_attempt_error(
                        &state.provider_sessions,
                        &state.metrics,
                        &provider_name,
                        gateway_error.as_ref(),
                        queue_ms,
                        provider_started,
                        request_started,
                    );
                    last_err = Some(message);
                    continue;
                }
            };
            first_stream_chunk = Some(first);
        }

        let new_conv_id = conversation_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        if req.stream {
            return streamed_provider_response(StreamingAttempt {
                state: state.clone(),
                owner_key_id: owner_key_id.clone(),
                windowed: windowed.clone(),
                provider: provider_name.clone(),
                model: req.model.clone(),
                provider_model: provider_model.clone(),
                conversation_id: new_conv_id,
                request_id: request_id.clone(),
                request_started,
                provider_started,
                completion_timeout: provider_timeout,
                queue_ms,
                prompt_tokens: estimate_tokens(&provider_prompt),
                permit,
                receiver: rx,
                first_chunk: first_stream_chunk.expect("streaming path prefetches its first chunk"),
            });
        }

        // Keep the exclusive provider permit until the JSON attempt has fully
        // finished. Only an error before the first content can use failover.
        let response = json_provider_response(JsonAttempt {
            state: &state,
            owner_key_id: &owner_key_id,
            windowed: &windowed,
            provider: &provider_name,
            model: &req.model,
            provider_model: &provider_model,
            conversation_id: &new_conv_id,
            request_id: &request_id,
            request_started,
            provider_started,
            completion_timeout: provider_timeout,
            queue_ms,
            prompt_tokens: estimate_tokens(&provider_prompt),
            receiver: rx,
        })
        .await;
        match response {
            Ok(response) => return response,
            Err(message) => {
                last_err = Some(message);
                continue;
            }
        }
    }

    if candidate_count > 0 && busy_count == candidate_count {
        // The typed backend boundary preserves the historical OpenAI error
        // code, message, and Retry-After without duplicating HTTP rendering.
        return chat_backend::GatewayError::provider_busy("provider slots exhausted")
            .into_response();
    }

    // The final attempt error may originate from a provider HTML page, a
    // browser driver, or a transient transport failure. Never echo it back
    // to the API client, since it can contain account/session metadata or
    // credentials. The request ID and per-provider outcome metrics provide
    // safe correlation without exposing raw upstream diagnostics.
    tracing::warn!(
        request_id = %request_id,
        had_provider_error = last_err.is_some(),
        "all eligible provider attempts exhausted"
    );
    unavailable_provider_response(last_err.as_deref())
}

#[cfg(test)]
mod conversation_header_tests {
    use super::*;

    #[tokio::test]
    async fn duplicate_conversation_ids_fail_closed_before_provider_dispatch() {
        for (first, second) in [
            ("router_dup_001", "router_dup_002"),
            ("router_dup_001", "router_dup_001"),
        ] {
            let mut headers = HeaderMap::new();
            headers.append(
                "X-Conversation-Id",
                axum::http::HeaderValue::from_str(first).unwrap(),
            );
            headers.append(
                "X-Conversation-Id",
                axum::http::HeaderValue::from_str(second).unwrap(),
            );
            let body = axum::body::Bytes::from_static(
                br#"{"model":"auto","messages":[{"role":"user","content":"synthetic prompt"}]}"#,
            );
            let Err(response) = prepare_chat_request(&headers, None, Ok(body)) else {
                panic!("ambiguous conversation IDs must be rejected before routing");
            };
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let bytes = axum::body::to_bytes((*response).into_body(), 65_536)
                .await
                .unwrap();
            let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(error["error"]["code"], "invalid_conversation_id");
        }
    }
}

#[cfg(test)]
mod first_stream_admission_tests {
    use super::*;

    #[test]
    fn first_stream_classifier_preserves_success_and_typed_errors() {
        let chunk = chat_backend::ChatEvent {
            content: "hello".to_owned(),
            done: false,
        };
        assert_eq!(
            chat_service::classify_initial_stream_event(FirstEvent::Chunk(chunk.clone())).unwrap(),
            chunk,
        );
        let timeout = chat_backend::GatewayError::timeout("provider timed out");
        let (message, typed) =
            chat_service::classify_initial_stream_event(FirstEvent::Error(timeout.clone()))
                .unwrap_err();
        assert_eq!(message, "provider timed out");
        assert_eq!(typed, Some(timeout));
    }

    #[test]
    fn first_stream_classifier_keeps_stable_failure_reasons() {
        for (event, expected) in [
            (
                FirstEvent::EmptyCompletion,
                "provider completed without output before stream started",
            ),
            (
                FirstEvent::Closed,
                "provider stream closed before first output",
            ),
            (
                FirstEvent::Timeout,
                "provider timed out before first output",
            ),
        ] {
            let (message, typed) = chat_service::classify_initial_stream_event(event).unwrap_err();
            assert_eq!(message, expected);
            assert!(typed.is_none());
        }
    }
}

#[cfg(test)]
mod provider_send_planner_tests {
    use super::*;

    fn m(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_owned(),
            content: content.to_owned(),
        }
    }

    fn saved_thread(provider: &str, model: &str, url: &str, marker: &str) -> ProviderThread {
        ProviderThread {
            conversation_id: "conversation-123".to_owned(),
            provider: provider.to_owned(),
            thread_url: url.to_owned(),
            synced_through_message_id: Some(marker.to_owned()),
            model: Some(model.to_owned()),
            updated_at: 0,
        }
    }

    #[test]
    fn synchronized_chatgpt_followup_uses_only_new_message_on_saved_thread() {
        let history = vec![m("user", "old question"), m("assistant", "old answer")];
        let incoming = vec![
            m("user", "old question"),
            m("assistant", "old answer"),
            m("user", "new question"),
        ];
        let marker = transcript_marker(&history);
        let (prompt, thread_url) = chat_service::plan_provider_send(
            "chatgpt",
            "chatgpt/test",
            &history,
            &incoming,
            Some(saved_thread(
                "chatgpt",
                "chatgpt/test",
                "https://chatgpt.com/c/thread_123",
                &marker,
            )),
            1000,
            "bounded canonical transcript",
        );
        assert_eq!(
            thread_url.as_deref(),
            Some("https://chatgpt.com/c/thread_123")
        );
        assert!(prompt.contains("new question"));
        assert!(!prompt.contains("old answer"));
    }

    #[test]
    fn stale_marker_changed_model_or_invalid_thread_replays_without_reopen() {
        let history = vec![m("user", "old question"), m("assistant", "old answer")];
        let incoming = vec![
            m("user", "old question"),
            m("assistant", "old answer"),
            m("user", "new question"),
        ];
        let marker = transcript_marker(&history);
        let inputs = [
            saved_thread(
                "chatgpt",
                "chatgpt/test",
                "https://chatgpt.com/c/thread_123",
                "stale",
            ),
            saved_thread(
                "chatgpt",
                "chatgpt/other",
                "https://chatgpt.com/c/thread_123",
                &marker,
            ),
            saved_thread(
                "chatgpt",
                "chatgpt/test",
                "https://evil.example/c/thread_123",
                &marker,
            ),
            saved_thread(
                "claude",
                "chatgpt/test",
                "https://chatgpt.com/c/thread_123",
                &marker,
            ),
        ];
        for thread in inputs {
            let (prompt, thread_url) = chat_service::plan_provider_send(
                "chatgpt",
                "chatgpt/test",
                &history,
                &incoming,
                Some(thread),
                1000,
                "bounded canonical transcript",
            );
            assert!(
                thread_url.is_none(),
                "replay must never reuse a stale remote thread"
            );
            assert!(prompt.contains("CONVERSATION_CONTEXT_BEGIN"));
            assert!(prompt.contains("bounded canonical transcript"));
        }
    }
}

#[cfg(test)]
mod completion_response_builder_tests {
    use super::*;

    #[test]
    fn sse_role_content_and_terminal_chunks_share_identity_and_openai_shape() {
        let conversation = "fixture-conversation-123";
        let model = "fixture-model";
        let role = sse_completion_chunk(
            conversation,
            model,
            serde_json::json!({ "role": "assistant" }),
            None,
        );
        let content = sse_completion_chunk(
            conversation,
            model,
            serde_json::json!({ "content": "fixture output" }),
            None,
        );
        let finish = sse_completion_chunk(conversation, model, serde_json::json!({}), Some("stop"));

        for chunk in [&role, &content, &finish] {
            assert_eq!(chunk["id"], "chatcmpl-fixture-");
            assert_eq!(chunk["model"], model);
            assert_eq!(chunk["object"], "chat.completion.chunk");
            assert_eq!(chunk["choices"][0]["index"], 0);
            assert!(chunk["created"].as_i64().is_some());
        }
        assert_eq!(role["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(content["choices"][0]["delta"]["content"], "fixture output");
        assert_eq!(role["choices"][0]["finish_reason"], serde_json::Value::Null);
        assert_eq!(
            content["choices"][0]["finish_reason"],
            serde_json::Value::Null
        );
        assert_eq!(finish["choices"][0]["delta"], serde_json::json!({}));
        assert_eq!(finish["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn json_response_preserves_usage_and_conversation_identity() {
        let conversation_id = "fixture-conversation-123";
        let content = "Hello from a fixture";
        let body = json_completion_body(conversation_id, "fixture-model", content, 11);
        assert_eq!(body["id"], "chatcmpl-fixture-");
        assert_eq!(body["conversation_id"], conversation_id);
        assert_eq!(body["model"], "fixture-model");
        assert_eq!(body["choices"][0]["message"]["content"], content);
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert_eq!(body["usage"]["prompt_tokens"], 11);
        assert_eq!(body["usage"]["completion_tokens"], estimate_tokens(content));
        assert_eq!(body["usage"]["total_tokens"], 11 + estimate_tokens(content));
        assert_eq!(body["x_usage_estimated"], true);
    }

    #[test]
    fn both_transports_keep_identity_and_only_sse_has_streaming_headers() {
        let json = completion_response_headers(
            "fixture-conversation-123",
            "chatgpt",
            "fixture-model",
            false,
        );
        let sse = completion_response_headers(
            "fixture-conversation-123",
            "chatgpt",
            "fixture-model",
            true,
        );
        assert_eq!(json.get("X-Conversation-Id"), sse.get("X-Conversation-Id"));
        assert!(json.get("X-Accel-Buffering").is_none());
        assert!(json.get("Cache-Control").is_none());
        assert_eq!(sse.get("Content-Type").unwrap(), "text/event-stream");
        assert_eq!(sse.get("Cache-Control").unwrap(), "no-cache");
        assert_eq!(sse.get("X-Accel-Buffering").unwrap(), "no");
    }
}
