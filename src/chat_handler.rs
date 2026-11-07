//! HTTP chat orchestration: provider routing, ownership, egress policy,
//! canonical response consumption and transactional turn persistence.
//!
//! This module retains the production implementation while progressively
//! moving backend-neutral decisions into `chat_service`. The public Axum
//! extractor stays in `main.rs` and is exercised by the existing real-router
//! scripted-backend tests.
use super::*;

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

    let memory_egress_policy = match memory_egress::MemoryEgressPolicy::from_env() {
        Ok(policy) => policy,
        Err(_) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_egress_policy_invalid",
                "provider memory egress policy is invalid",
            )
        }
    };

    // The default-deny policy should not reduce transcript context when no
    // provider is authorized to receive personal memory.
    let personal_memory_egress_enabled = !memory_egress_policy
        .allowed_personal_memory_providers()
        .is_empty();

    // Retrieve personal memory only when the owner opted in. Read errors fail
    // closed rather than silently changing the prompt. Reserve a fixed portion
    // of the context budget before assembling transcript history.
    let retrieved_memory = if personal_memory_egress_enabled {
        match state
            .memory
            .clone()
            .retrieve_long_term_memory_items_async(owner_key_id.clone(), 20)
            .await
        {
            Ok(items) => items,
            _ => {
                return api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    "memory_read_failed",
                    "long-term memory could not be read safely",
                );
            }
        }
    } else {
        Vec::new()
    };
    let (personal_memory_items, memory_tokens) =
        budget_personal_memory_items(retrieved_memory, 512);
    let context_token_budget = 8000usize.saturating_sub(memory_tokens);

    // Read all context-dependent memory before any provider request. A read
    // failure must not degrade into an empty prompt or an unauthenticated turn.
    let (context_prompt, windowed, has_history, stored_messages) =
        match read_conversation_context_with_transcript_for_owner(
            state.memory.clone(),
            owner_key_id.clone(),
            conversation_id.clone(),
            req.messages.clone(),
            context_token_budget,
        )
        .await
        {
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

    let router = state.router.clone();
    let routing_memory = state.memory.clone();
    let routing_owner = owner_key_id.clone();
    let routing_model = req.model.clone();
    let routing_conversation_id = conversation_id.clone();
    let mut candidates = match router
        .resolve_for_owner_async(
            &routing_owner,
            &routing_model,
            routing_conversation_id.as_deref(),
            routing_memory,
        )
        .await
    {
        Ok(candidates) => candidates,
        Err(_) => {
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
        if !state.provider_sessions.is_routable(&provider_name) {
            last_err = Some(format!("provider '{provider_name}' needs login"));
            continue;
        }
        if let Some(reason) = state
            .provider_guard
            .provider_disabled_reason(&provider_name)
        {
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
        // Fail closed on provider data policy before charging pacing quota or
        // taking its exclusive browser permit. Thread lookup and context
        // planning intentionally remain UNDER the permit to serialize
        // concurrent sends to the same remote provider thread.
        if memory_egress_policy
            .authorize(
                &provider_name,
                memory_egress::ProviderDataClass::CurrentTurn,
            )
            .is_err()
            || memory_egress_policy
                .authorize(
                    &provider_name,
                    memory_egress::ProviderDataClass::ConversationContext,
                )
                .is_err()
        {
            last_err = Some("provider data-handling policy denied request".to_owned());
            continue;
        }

        // Reserve/wait for the provider's pacing slot before taking its
        // exclusive browser permit. Otherwise a long interval blocks every
        // other caller behind a permit that is not doing useful work.
        if let Err(error) = state
            .provider_guard
            .wait_for_request_slot(
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
                    .clone()
                    .get_provider_thread_async(
                        owner_key_id.clone(),
                        id.to_owned(),
                        provider_name.clone(),
                    )
                    .await
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
            context_token_budget,
        );
        let (mut provider_prompt, thread_url) = match send_plan {
            ContextPlan::Delta { messages } => (
                context::render_prompt(&messages),
                saved_thread.map(|thread| thread.thread_url),
            ),
            ContextPlan::Replay { preamble, .. } => (format!("{preamble}\n{prompt}"), None),
        };

        // Enforce data-handling policy at the last gateway boundary before
        // provider-specific submission. Current-turn content and local
        // transcript context are distinct from optional personal long-term
        // memory. The typed egress policy filters retrieved items for each
        // candidate before rendering them into that provider's prompt.
        if memory_egress_policy
            .append_personal_memory_to_prompt(
                &provider_name,
                &mut provider_prompt,
                &personal_memory_items,
            )
            .is_err()
        {
            last_err = Some("provider memory egress policy denied request".to_owned());
            continue;
        }

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
                std::time::Duration::from_secs(provider_cfg.timeout_seconds),
            )
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                let retryable_worker_error = e.is_retryable();
                tracing::debug!(
                    provider = %provider_name,
                    retryable = retryable_worker_error,
                    "provider send failed"
                );
                // Worker loss is an infrastructure failure, not evidence that
                // the provider's authentication session has expired.
                if !retryable_worker_error {
                    state
                        .provider_sessions
                        .record_gateway_error(&provider_name, &e);
                }
                state.metrics.record_provider_timings(
                    &provider_name,
                    "error",
                    queue_ms,
                    None,
                    provider_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    request_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                );
                last_err = Some(e.to_string());
                continue;
            }
        };

        let mut first_stream_chunk: Option<chat_backend::ChatEvent> = None;
        if req.stream {
            let first_chunk_timeout = std::time::Duration::from_secs(provider_cfg.timeout_seconds)
                .saturating_sub(provider_started.elapsed());
            let first = match first_event(&mut rx, first_chunk_timeout).await {
                FirstEvent::Chunk(chunk) => chunk,
                FirstEvent::EmptyCompletion => {
                    let error = "provider completed without output before stream started";
                    record_attempt_error(
                        &state.provider_sessions,
                        &state.metrics,
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
                        &state.provider_sessions,
                        &state.metrics,
                        &provider_name,
                        &message,
                        queue_ms,
                        provider_started,
                        request_started,
                    );
                    state
                        .provider_sessions
                        .record_gateway_error(&provider_name, &error);
                    last_err = Some(message);
                    continue;
                }
                FirstEvent::Closed => {
                    let error = "provider stream closed before first output";
                    record_attempt_error(
                        &state.provider_sessions,
                        &state.metrics,
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
                    state.provider_sessions.record_error(&provider_name, error);
                    state.metrics.record_provider_timings(
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
            let completion_timeout_c = std::time::Duration::from_secs(provider_cfg.timeout_seconds);
            let queue_ms_c = queue_ms;
            let request_id_c = request_id.clone();
            let sse_stream = async_stream::stream! {
                let _permit = permit;
                let mut accumulator = chat_service::ResponseAccumulator::new(provider_started_c);
                let completion_deadline =
                    chat_service::completion_deadline(provider_started_c, completion_timeout_c);
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
                // The same terminal-aware reducer drives both SSE and JSON
                // responses, including TTFT, partial errors, and channel closure.
                let mut pending: Option<Result<chat_backend::ChatEvent, chat_backend::GatewayError>> = Some(Ok(first_chunk));
                while let Some(item) = match pending.take() {
                    Some(item) => Some(item),
                    None => chat_service::recv_with_deadline(&mut rx, completion_deadline).await,
                } {
                    let update = accumulator.push(item);
                    let done_after_delta =
                        matches!(&update, chat_service::StreamUpdate::CompleteWithDelta(_));
                    match update {
                        chat_service::StreamUpdate::Delta(content)
                        | chat_service::StreamUpdate::CompleteWithDelta(content) => {
                            let data = serde_json::json!({
                                "id": format!("chatcmpl-{}", &conv_stream[..8]),
                                "object": "chat.completion.chunk",
                                "created": chrono::Utc::now().timestamp(),
                                "model": model_c,
                                "choices": [{ "delta": { "content": content }, "index": 0, "finish_reason": null }],
                            });
                            yield Ok::<_, anyhow::Error>(Event::default().data(data.to_string()));
                            if done_after_delta {
                                break;
                            }
                        }
                        chat_service::StreamUpdate::Complete => break,
                        chat_service::StreamUpdate::Failed => {
                            tracing::warn!(provider = %provider_c, "provider stream failed; partial response not committed");
                            break;
                        }
                    }
                }
                let collected = accumulator.finish();
                let full = collected.content;
                let ttft_ms = collected
                    .first_event_elapsed
                    .map(|elapsed| elapsed.as_millis().min(u64::MAX as u128) as u64);
                let provider_gateway_error = collected.error;
                let stream_error = provider_gateway_error.as_ref().map(|error| {
                    if error.message == chat_service::INCOMPLETE_PROVIDER_STREAM {
                        "provider stream closed before terminal completion; partial response was not committed".to_owned()
                    } else {
                        "provider stream failed; partial response was not committed".to_owned()
                    }
                });

                let provider_work_ms = provider_started_c.elapsed().as_millis().min(u64::MAX as u128) as u64;
                // Persist before reporting a successful gateway operation.
                // An SSE stream may have emitted content, but no successful
                // terminal completion can be claimed until its turn is durable.
                let persist_error = if stream_error.is_none() {
                    state_c.provider_sessions.record_success(&provider_c);
                    persist_completed_turn(
                        &state_c,
                        &owner_key_id_c,
                        &conv_stream,
                        windowed_c.clone(),
                        full.clone(),
                        &provider_c,
                        &model_c,
                    )
                    .await
                    .err()
                } else {
                    None
                };
                let total_ms = request_started_c.elapsed().as_millis().min(u64::MAX as u128) as u64;
                record_provider_outcome(
                    &state_c.metrics,
                    ProviderOutcome {
                        request_id: &request_id_c,
                        provider: &provider_c,
                        outcome: completed_chat_outcome(stream_error.is_some(), persist_error.is_some()),
                        queue_ms: queue_ms_c,
                        ttft_ms,
                        provider_work_ms,
                        total_ms,
                    },
                );
                if let Some(message) = stream_error {
                    // Only typed provider authentication failures require
                    // re-login. Never infer that from arbitrary upstream text.
                    if let Some(error) = provider_gateway_error.as_ref() {
                        state_c
                            .provider_sessions
                            .record_gateway_error(&provider_c, error);
                    }
                    let error = serde_json::json!({
                        "error": { "message": message, "type": "provider_error", "code": "stream_failed" }
                    });
                    yield Ok(Event::default().data(error.to_string()));
                } else {
                    if let Some(error) = persist_error {
                        tracing::error!(conversation_id = %conv_stream, %error, "conversation persistence failed after stream");
                        let error = serde_json::json!({
                            "error": { "message": "conversation could not be persisted", "type": "server_error", "code": "memory_persistence_failed" }
                        });
                        yield Ok(Event::default().data(error.to_string()));
                        yield Ok(Event::default().data("[DONE]"));
                        return;
                    }
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
        let collected = state
            .chat_service
            .collect_with_timeout(
                rx,
                provider_started,
                std::time::Duration::from_secs(provider_cfg.timeout_seconds),
            )
            .await;
        let full = collected.content;
        let failed = collected.error;
        let ttft_ms = collected
            .first_event_elapsed
            .map(|elapsed| elapsed.as_millis().min(u64::MAX as u128) as u64);
        let provider_work_ms = provider_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let total_ms = request_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        if let Some(e) = failed {
            state
                .provider_sessions
                .record_gateway_error(&provider_name, &e);
            let message = e.to_string();
            record_provider_outcome(
                &state.metrics,
                ProviderOutcome {
                    request_id: &request_id,
                    provider: &provider_name,
                    outcome: "error",
                    queue_ms,
                    ttft_ms,
                    provider_work_ms,
                    total_ms,
                },
            );
            if full.is_empty() {
                tracing::warn!(provider = %provider_name, "provider failed before output; considering fallback");
                last_err = Some(message);
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

        state.provider_sessions.record_success(&provider_name);
        let persist_result = persist_completed_turn(
            &state,
            &owner_key_id,
            &new_conv_id,
            windowed.clone(),
            full.clone(),
            &provider_name,
            &req.model,
        )
        .await;
        // Total gateway latency includes the durable write, not just provider
        // output. Use the same post-commit timing boundary as the SSE path.
        let total_ms = request_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        record_provider_outcome(
            &state.metrics,
            ProviderOutcome {
                request_id: &request_id,
                provider: &provider_name,
                outcome: completed_chat_outcome(false, persist_result.is_err()),
                queue_ms,
                ttft_ms,
                provider_work_ms,
                total_ms,
            },
        );
        if let Err(error) = persist_result {
            tracing::error!(conversation_id = %new_conv_id, %error, "conversation persistence failed");
            return api_error(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_persistence_failed",
                "conversation could not be persisted",
            );
        }
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
