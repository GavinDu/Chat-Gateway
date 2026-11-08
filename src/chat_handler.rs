//! HTTP chat orchestration: provider routing, ownership, egress policy,
//! canonical response consumption and transactional turn persistence.
//!
//! This module retains the production implementation while progressively
//! moving backend-neutral decisions into `chat_service`. The public Axum
//! extractor stays in `main.rs` and is exercised by the existing real-router
//! scripted-backend tests.
use super::*;

/// Only syntactically and semantically valid requests proceed to memory or
/// provider access. This same boundary applies to JSON and SSE requests.
struct PreparedChatRequest {
    req: ChatReq,
    owner_key_id: String,
    conversation_id: Option<String>,
}

fn prepare_chat_request(
    headers: &HeaderMap,
    auth_context: Option<&Extension<AuthKeyContext>>,
    body: Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<PreparedChatRequest, Box<Response>> {
    // Bytes extraction happens before the handler; preserve the structured
    // OpenAI error envelope on oversized payloads.
    let body = body.map_err(|_| Box::new(oversized_chat_body_error()))?;
    let req = parse_and_validate_chat_req(&body).map_err(Box::new)?;
    let owner_key_id = auth_context
        .map(|Extension(context)| context.key_id.as_str())
        .unwrap_or("local")
        .to_owned();
    // Duplicate conversation IDs are ambiguous across reverse proxies and
    // HTTP stacks that disagree about first/last header precedence. Never
    // route or persist a turn using an arbitrarily selected identifier.
    if headers.get_all("X-Conversation-Id").iter().count() > 1 {
        return Err(Box::new(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_conversation_id",
            "X-Conversation-Id must appear at most once",
        )));
    }
    let conversation_id = match headers.get("X-Conversation-Id") {
        None => None,
        Some(value) => {
            let value = value.to_str().map_err(|_| {
                Box::new(api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "invalid_conversation_id",
                    "X-Conversation-Id must be a valid ASCII identifier",
                ))
            })?;
            if !valid_conversation_id(value) {
                return Err(Box::new(api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "invalid_conversation_id",
                    "X-Conversation-Id must be 8-128 ASCII letters, digits, '_' or '-'",
                )));
            }
            Some(value.to_owned())
        }
    };
    Ok(PreparedChatRequest {
        req,
        owner_key_id,
        conversation_id,
    })
}

/// Resolve and authorize the provider candidates before acquiring a worker
/// permit. In particular, a failed affinity read or an empty API-key allowlist
/// must never fall through to a different provider.
async fn resolve_chat_candidates(
    state: &AppState,
    owner_key_id: &str,
    model: &str,
    conversation_id: Option<&str>,
    auth_context: Option<&Extension<AuthKeyContext>>,
) -> Result<(Vec<String>, String), Box<Response>> {
    let mut candidates = state
        .router
        .resolve_for_owner_async(owner_key_id, model, conversation_id, state.memory.clone())
        .await
        .map_err(|_| {
            tracing::error!(conversation_id = ?conversation_id, "provider affinity read failed");
            Box::new(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_read_failed",
                "conversation routing state could not be read safely",
            ))
        })?;
    let original_candidate_count = candidates.len();
    if let Some(Extension(context)) = auth_context {
        if let Some(allowed) = &context.provider_allowlist {
            candidates.retain(|provider| allowed.iter().any(|item| item == provider));
            if candidates.is_empty() && original_candidate_count > 0 {
                return Err(Box::new(api_error(
                    StatusCode::FORBIDDEN,
                    "authentication_error",
                    "provider_not_allowed",
                    "the API key is not allowed to use the requested provider",
                )));
            }
        }
    }
    let provider_model = model
        .split_once('/')
        .map(|(_, model)| model)
        .unwrap_or(model)
        .to_owned();
    tracing::info!(model, ?candidates, conv = ?conversation_id, "routing request");
    if candidates.is_empty() {
        return Err(Box::new(api_error(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "model_not_found",
            format!("model '{model}' is not configured for an enabled provider; use /v1/models or model='auto'"),
        )));
    }
    Ok((candidates, provider_model))
}

/// Keep JSON and SSE response headers in one place so the two transports
/// cannot silently diverge on conversation identity and provider metadata.
fn completion_response_headers(
    conversation_id: &str,
    provider: &str,
    provider_model: &str,
    streaming: bool,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("X-Conversation-Id", conversation_id.parse().unwrap());
    if streaming {
        headers.insert("Content-Type", "text/event-stream".parse().unwrap());
        headers.insert("Cache-Control", "no-cache".parse().unwrap());
        headers.insert("X-Accel-Buffering", "no".parse().unwrap());
    }
    add_gateway_headers(&mut headers, provider, provider_model);
    headers
}

/// Render the common OpenAI SSE chunk envelope once for role, content and
/// terminal deltas. Keeping identity and creation metadata in one place
/// prevents transport-specific edits from changing only one chunk shape.
fn sse_completion_chunk(
    conversation_id: &str,
    model: &str,
    delta: serde_json::Value,
    finish_reason: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "id": format!("chatcmpl-{}", &conversation_id[..8]),
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{ "delta": delta, "index": 0, "finish_reason": finish_reason }],
    })
}

/// Pure response construction: the live handler controls persistence and
/// success metrics, while this helper only renders the OpenAI JSON envelope.
fn json_completion_body(
    conversation_id: &str,
    model: &str,
    content: &str,
    prompt_tokens: u64,
) -> serde_json::Value {
    let completion_tokens = estimate_tokens(content);
    serde_json::json!({
        "id": format!("chatcmpl-{}", &conversation_id[..8]),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{ "message": { "role": "assistant", "content": content }, "finish_reason": "stop", "index": 0 }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens
        },
        "x_usage_estimated": true,
        "conversation_id": conversation_id,
    })
}

struct PreparedChatContext {
    memory_egress_policy: memory_egress::MemoryEgressPolicy,
    personal_memory_items: Vec<memory_egress::PersonalMemoryItem>,
    context_token_budget: usize,
    prompt: String,
    windowed: Vec<ChatMessage>,
    has_history: bool,
    stored_messages: Vec<ChatMessage>,
}

/// Prepare one immutable, owner-scoped context snapshot before provider
/// routing or admission. Both optional personal-memory reads and canonical
/// transcript reads fail closed; no provider is touched on a failed read.
async fn prepare_chat_context(
    state: &AppState,
    owner_key_id: &str,
    conversation_id: Option<&str>,
    incoming_messages: &[ChatMessage],
) -> Result<PreparedChatContext, Box<Response>> {
    let memory_egress_policy = memory_egress::MemoryEgressPolicy::from_env().map_err(|_| {
        Box::new(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "memory_egress_policy_invalid",
            "provider memory egress policy is invalid",
        ))
    })?;

    // Do not reserve memory context tokens when no provider is authorized
    // to receive personal items. Disabled personal memory requires no read.
    let retrieved_memory = if !memory_egress_policy
        .allowed_personal_memory_providers()
        .is_empty()
    {
        state
            .memory
            .clone()
            .retrieve_long_term_memory_items_async(owner_key_id.to_owned(), 20)
            .await
            .map_err(|_| {
                Box::new(api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    "memory_read_failed",
                    "long-term memory could not be read safely",
                ))
            })?
    } else {
        Vec::new()
    };
    let (personal_memory_items, memory_tokens) =
        budget_personal_memory_items(retrieved_memory, 512);
    let context_token_budget = 8000usize.saturating_sub(memory_tokens);

    let (prompt, windowed, has_history, stored_messages) =
        read_conversation_context_with_transcript_for_owner(
            state.memory.clone(),
            owner_key_id.to_owned(),
            conversation_id.map(str::to_owned),
            incoming_messages.to_vec(),
            context_token_budget,
        )
        .await
        .map_err(|_| Box::new(conversation_context_read_error(conversation_id)))?;
    if prompt.is_empty() {
        return Err(Box::new(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "empty_prompt",
            "prompt must not be empty",
        )));
    }

    Ok(PreparedChatContext {
        memory_egress_policy,
        personal_memory_items,
        context_token_budget,
        prompt,
        windowed,
        has_history,
        stored_messages,
    })
}

/// The pure provider-context decision is shared by JSON and SSE attempts.
///
/// A delta is allowed only for a validated, reopenable thread whose exact
/// provider, model and durable transcript marker still match. Otherwise
/// return an isolated bounded replay and never navigate the old thread.
fn plan_provider_send(
    provider: &str,
    model: &str,
    stored_messages: &[ChatMessage],
    incoming_messages: &[ChatMessage],
    saved_thread: Option<ProviderThread>,
    token_budget: usize,
    replay_prompt: &str,
) -> (String, Option<String>) {
    let saved_thread = saved_thread
        .filter(|thread| is_reopenable_provider_thread_url(provider, &thread.thread_url));
    let supports_reopen = providers::ADAPTERS
        .iter()
        .find(|spec| spec.id == provider)
        .is_some_and(|spec| matches!(spec.threads, providers::ThreadSupport::UrlAddressable))
        && saved_thread
            .as_ref()
            .is_some_and(|thread| thread.model.as_deref() == Some(model));
    let last_marker = transcript_marker(stored_messages);
    let thread_state = saved_thread.as_ref().map(|thread| ThreadSyncState {
        provider: thread.provider.clone(),
        thread_url: thread.thread_url.clone(),
        synced_through_message_id: thread.synced_through_message_id.clone(),
        last_stored_message_id: Some(last_marker),
    });
    match ContextBuilder::plan(
        stored_messages,
        incoming_messages,
        provider,
        thread_state.as_ref(),
        supports_reopen,
        token_budget,
    ) {
        ContextPlan::Delta { messages } => (
            context::render_prompt(&messages),
            saved_thread.map(|thread| thread.thread_url),
        ),
        ContextPlan::Replay { preamble, .. } => (format!("{preamble}\n{replay_prompt}"), None),
    }
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

    let PreparedChatContext {
        memory_egress_policy,
        personal_memory_items,
        context_token_budget,
        prompt,
        windowed,
        has_history,
        stored_messages,
    } = match prepare_chat_context(
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

    let (candidates, provider_model) = match resolve_chat_candidates(
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
                    Ok(thread) => thread,
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
        let (mut provider_prompt, thread_url) = plan_provider_send(
            &provider_name,
            &req.model,
            &stored_messages,
            &req.messages,
            saved_thread,
            context_token_budget,
            &prompt,
        );

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
                let role = sse_completion_chunk(
                    &conv_stream,
                    &model_c,
                    serde_json::json!({ "role": "assistant" }),
                    None,
                );
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
                            let data = sse_completion_chunk(
                                &conv_stream,
                                &model_c,
                                serde_json::json!({ "content": content }),
                                None,
                            );
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
                let persist_error = chat_service::finalize_provider_attempt(
                    &state_c,
                    &owner_key_id_c,
                    &conv_stream,
                    &windowed_c,
                    &full,
                    &model_c,
                    chat_service::AttemptTelemetry {
                        request_id: &request_id_c,
                        provider: &provider_c,
                        queue_ms: queue_ms_c,
                        ttft_ms,
                        provider_work_ms,
                        request_started: request_started_c,
                        stream_failed: stream_error.is_some(),
                    },
                )
                .await;
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
                    let mut finish = sse_completion_chunk(
                        &conv_stream,
                        &model_c,
                        serde_json::json!({}),
                        Some("stop"),
                    );
                    finish["usage"] = serde_json::json!({
                        "prompt_tokens": prompt_tokens_c,
                        "completion_tokens": completion_tokens,
                        "total_tokens": prompt_tokens_c + completion_tokens,
                    });
                    finish["x_usage_estimated"] = serde_json::json!(true);
                    yield Ok(Event::default().data(finish.to_string()));
                }
                yield Ok(Event::default().data("[DONE]"));
            };
            let resp_headers =
                completion_response_headers(&new_conv_id, &provider_name, &provider_model_c, true);
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

        let persist_error = chat_service::finalize_provider_attempt(
            &state,
            &owner_key_id,
            &new_conv_id,
            &windowed,
            &full,
            &req.model,
            chat_service::AttemptTelemetry {
                request_id: &request_id,
                provider: &provider_name,
                queue_ms,
                ttft_ms,
                provider_work_ms,
                request_started,
                stream_failed: false,
            },
        )
        .await;
        if let Some(error) = persist_error {
            tracing::error!(conversation_id = %new_conv_id, %error, "conversation persistence failed");
            return api_error(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_persistence_failed",
                "conversation could not be persisted",
            );
        }
        let body = json_completion_body(
            &new_conv_id,
            &req.model,
            &full,
            estimate_tokens(&provider_prompt),
        );
        let resp_headers =
            completion_response_headers(&new_conv_id, &provider_name, &provider_model, false);
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
        let (prompt, thread_url) = plan_provider_send(
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
            let (prompt, thread_url) = plan_provider_send(
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
