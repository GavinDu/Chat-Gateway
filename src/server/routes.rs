use axum::{extract::State, http::HeaderMap, response::{IntoResponse, sse::{Sse, Event}}, Json};
use std::sync::Arc;
use crate::server::state::{AppState, PerformanceTrace, GenerationBreakdown};
use serde::Deserialize;
use tokio_stream::StreamExt;
use std::time::Instant;

#[derive(Deserialize)]
pub struct ChatReq {
    pub model: String,
    pub messages: Vec<crate::memory::ChatMessage>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f32>,
}

pub async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let providers = state.config.providers.iter().map(|p| {
        serde_json::json!({ "name": p.name, "enabled": p.enabled, "priority": p.priority, "models": p.models })
    }).collect::<Vec<_>>();
    Json(serde_json::json!({ "status": "ok", "providers": providers, "obscura": state.config.server.obscura_endpoint, "sessions_loaded": state.session_store }))
}

pub async fn list_models(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let cache = state.model_cache.list().await;
    let mut data = vec![];
    for p in &state.config.providers {
        for m in &p.models {
            data.push(serde_json::json!({ "id": m, "object": "model", "owned_by": p.name }));
        }
    }
    for (provider, models) in cache {
        for m in models {
            if !data.iter().any(|d| d["id"] == m) {
                let id = if data.iter().any(|d| d["id"] == m) { format!("{provider}:{m}") } else { m.clone() };
                data.push(serde_json::json!({ "id": id, "object": "model", "owned_by": provider, "discovered": true }));
            }
        }
    }
    Json(serde_json::json!({ "object": "list", "data": data }))
}

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let request_id = uuid::Uuid::new_v4().to_string();
    let total_start = Instant::now();
    let mut trace = PerformanceTrace::new(request_id.clone(), "unknown".to_string());

    // Phase: parse
    let parse_start = Instant::now();
    let req: ChatReq = match serde_json::from_slice::<ChatReq>(&body) {
        Ok(r) => r,
        Err(e) => {
            trace.error = Some(format!("parse error: {e}"));
            state.traces.push(trace).await;
            return (axum::http::StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": { "message": e.to_string() } }))).into_response();
        }
    };
    trace.parse_ms = parse_start.elapsed().as_millis();
    trace.model = req.model.clone();

    let conversation_id = headers.get("X-Conversation-Id").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    trace.conversation_id = conversation_id.clone();

    // Phase: memory + context
    let mem_start = Instant::now();
    let (prompt, windowed) = state.memory.build_context(conversation_id.as_deref(), &req.messages);
    trace.memory_ms = mem_start.elapsed().as_millis();
    trace.context_ms = 0; // part of memory in Rust impl

    // Phase: routing
    let routing_start = Instant::now();
    let router = crate::providers::router::ProviderRouter::new(state.config.clone());
    let candidates = router.resolve(&req.model, conversation_id.as_deref(), &state.memory);
    trace.routing_ms = routing_start.elapsed().as_millis();
    trace.provider_attempts = candidates.clone();

    let tab_hint = conversation_id.as_deref().and_then(|cid| state.memory.get_tab(cid));

    tracing::info!(request_id=%request_id, model=%req.model, ?candidates, conv=?conversation_id, prompt_len=%prompt.len(), "routing request");

    // Try providers in priority order with full waterfall
    let mut last_err: Option<String> = None;
    for provider_name in candidates {
        trace.provider = provider_name.clone();

        // Phase: browser acquisition + queue
        let acq_start = Instant::now();
        let permit = match state.browser_pool.acquire(&provider_name).await {
            Ok(p) => p,
            Err(e) => { last_err = Some(e.to_string()); continue; }
        };
        trace.browser_acquisition_ms = acq_start.elapsed().as_millis();
        trace.provider_queue_ms = permit.queue_ms;

        // Phase: page readiness (new page + session apply + wait for input)
        let readiness_start = Instant::now();
        let browser = permit.browser.clone();
        let page = match browser.new_page("about:blank").await {
            Ok(p) => p,
            Err(e) => { last_err = Some(e.to_string()); continue; }
        };

        // Navigate to provider origin first to allow cookie + localStorage injection (required for auth)
        let provider_url = match provider_name.as_str() {
            "qwen" => "https://chat.qwen.ai",
            "chatgpt" => "https://chatgpt.com",
            "gemini" => "https://gemini.google.com",
            "metaai" => "https://www.meta.ai",
            "claude" => "https://claude.ai",
            "grok" => "https://grok.com",
            "mistral" => "https://chat.mistral.ai",
            "perplexity" => "https://www.perplexity.ai",
            _ => "about:blank",
        };

        if let Err(e) = page.goto(provider_url).await {
            tracing::warn!("initial goto {} failed: {}", provider_url, e);
        }

        if let Err(e) = state.session_store.apply_to_page(&provider_name, &page).await {
            tracing::warn!("session apply failed for {}: {}", provider_name, e);
        }

        // Reload after session injection to become authenticated
        let _ = page.reload().await;
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        // Get adapter for readiness check
        let registry = crate::providers::adapters::build_registry(&state.config);
        let adapter = match registry.get(&provider_name) {
            Some(a) => a.clone(),
            None => { last_err = Some(format!("adapter {} not found", provider_name)); continue; }
        };

        // Wait for input selector (page readiness)
        let input_sels = [adapter.selectors().input_selector.clone()];
        let input_refs: Vec<&str> = input_sels.iter().map(|s| s.as_str()).collect();
        if let Err(e) = crate::providers::adapters::common::wait_for_selector(&page, &input_refs, 15000).await {
            tracing::warn!(provider=%provider_name, "page readiness failed: {e}");
            last_err = Some(format!("readiness failed: {e}"));
            continue;
        }
        trace.page_readiness_ms = readiness_start.elapsed().as_millis();

        // Phase: send (input + send click) - measured inside adapter, but we track overall
        let send_start = Instant::now();

        let preq = crate::providers::traits::ProviderRequest {
            conversation_id: conversation_id.clone(),
            messages: windowed.clone(),
            model: req.model.clone(),
            stream: req.stream,
            tab_id: tab_hint.clone(),
        };

        match adapter.send(&page, preq).await {
            Ok(stream) => {
                let new_conv_id = conversation_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let state_clone = state.clone();
                let mut trace_clone = trace.clone();
                let windowed_clone = windowed.clone();
                let provider_clone = provider_name.clone();
                let model_clone = req.model.clone();
                let total_start_clone = total_start;

                if req.stream {
                    let sse_stream = async_stream::stream! {
                        let mut full = String::new();
                        let mut update_count = 0usize;
                        let mut ttft: Option<Instant> = None;
                        let mut first_update: Option<u128> = None;
                        let gen_start = Instant::now();
                        let mut stream = stream;
                        let mut last_len = 0usize;

                        while let Some(chunk) = stream.next().await {
                            match chunk {
                                Ok(c) => {
                                    if ttft.is_none() && !c.content.is_empty() {
                                        ttft = Some(Instant::now());
                                        first_update = Some(gen_start.elapsed().as_millis());
                                    }
                                    if !c.content.is_empty() {
                                        full.push_str(&c.content);
                                        update_count += 1;
                                    }
                                    let data = serde_json::json!({
                                        "id": format!("chatcmpl-{}", &new_conv_id[..8]),
                                        "object": "chat.completion.chunk",
                                        "created": chrono::Utc::now().timestamp(),
                                        "model": model_clone,
                                        "choices": [{"delta": {"content": c.content}, "index": 0, "finish_reason": if c.done { Some("stop") } else { None }}]
                                    });
                                    yield Ok::<_, anyhow::Error>(Event::default().data(data.to_string()));
                                    if c.done { break; }
                                }
                                Err(e) => { tracing::error!("stream err: {e}"); break; }
                            }
                        }

                        // Finalize trace
                        let total_ms = total_start_clone.elapsed().as_millis();
                        let ttft_ms = ttft.map(|t| t.elapsed().as_millis()).unwrap_or(0);
                        // Actually TTFT should be from send start
                        let generation_ms = gen_start.elapsed().as_millis();

                        // Store turn in unified memory (no prompts/responses logged in trace)
                        state_clone.memory.store_turn(
                            new_conv_id.clone(),
                            windowed_clone.clone(),
                            crate::memory::ChatMessage { role: "assistant".to_string(), content: full.clone() },
                            provider_clone.clone(),
                            format!("tab-{}", &new_conv_id[..8])
                        );

                        // Push performance trace
                        trace_clone.total_ms = total_ms;
                        trace_clone.ttft_ms = ttft_ms;
                        trace_clone.generation_ms = generation_ms;
                        trace_clone.generation_breakdown = GenerationBreakdown {
                            first_update_latency_ms: first_update.unwrap_or(0),
                            active_growth_ms: generation_ms.saturating_sub(first_update.unwrap_or(0)),
                            settle_wait_ms: 0,
                            delivery_ms: 0,
                        };
                        trace_clone.response_update_count = update_count;
                        trace_clone.response_len = full.len();
                        trace_clone.success = true;
                        trace_clone.send_ms = send_start.elapsed().as_millis();
                        state_clone.traces.push(trace_clone).await;

                        yield Ok(Event::default().data("[DONE]"));
                    };
                    let mut resp_headers = axum::http::HeaderMap::new();
                    resp_headers.insert("X-Conversation-Id", new_conv_id.parse().unwrap());
                    resp_headers.insert("Content-Type", "text/event-stream".parse().unwrap());
                    return (resp_headers, Sse::new(sse_stream)).into_response();
                } else {
                    // Non-stream
                    let mut full = String::new();
                    let mut update_count = 0usize;
                    let gen_start = Instant::now();
                    let mut ttft: Option<Instant> = None;
                    let mut stream = stream;
                    while let Some(chunk) = stream.next().await {
                        if let Ok(c) = chunk {
                            if ttft.is_none() && !c.content.is_empty() { ttft = Some(Instant::now()); }
                            full.push_str(&c.content);
                            update_count += 1;
                            if c.done { break; }
                        }
                    }
                    let new_conv_id2 = new_conv_id.clone();
                    state.memory.store_turn(
                        new_conv_id2.clone(),
                        windowed.clone(),
                        crate::memory::ChatMessage { role: "assistant".to_string(), content: full.clone() },
                        provider_name.clone(),
                        format!("tab-{}", &new_conv_id[..8])
                    );

                    // Finalize trace for non-stream
                    trace.total_ms = total_start.elapsed().as_millis();
                    trace.ttft_ms = ttft.map(|t| t.elapsed().as_millis()).unwrap_or(0);
                    trace.generation_ms = gen_start.elapsed().as_millis();
                    trace.response_update_count = update_count;
                    trace.response_len = full.len();
                    trace.success = true;
                    trace.send_ms = send_start.elapsed().as_millis();
                    state.traces.push(trace.clone()).await;

                    let body = serde_json::json!({
                        "id": format!("chatcmpl-{}", &new_conv_id[..8]),
                        "object": "chat.completion",
                        "created": chrono::Utc::now().timestamp(),
                        "model": req.model,
                        "choices": [{"message": {"role": "assistant", "content": full}, "finish_reason": "stop", "index": 0}],
                        "conversation_id": new_conv_id
                    });
                    let mut resp_headers = axum::http::HeaderMap::new();
                    resp_headers.insert("X-Conversation-Id", new_conv_id.parse().unwrap());
                    return (resp_headers, Json(body)).into_response();
                }
            }
            Err(e) => {
                tracing::warn!(provider=%provider_name, request_id=%request_id, "provider failed: {e}");
                last_err = Some(e.to_string());
                trace.error = Some(e.to_string());
                continue;
            }
        }
    }

    trace.total_ms = total_start.elapsed().as_millis();
    trace.success = false;
    state.traces.push(trace).await;

    let err_body = serde_json::json!({ "error": { "message": format!("all providers failed: {:?}", last_err), "type": "provider_error" } });
    (axum::http::StatusCode::BAD_GATEWAY, Json(err_body)).into_response()
}
