//! Wire-format rendering of OpenAI-compatible JSON/SSE completions.
//!
//! Both transports share identity, provider headers and usage math; the
//! request handler remains responsible for success and persistence semantics.
use axum::http::HeaderMap;

use crate::api::{add_gateway_headers, estimate_tokens};

/// Keep JSON and SSE response headers in one place so the two transports
/// cannot silently diverge on conversation identity and provider metadata.
pub(crate) fn completion_response_headers(
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
pub(crate) fn sse_completion_chunk(
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
pub(crate) fn json_completion_body(
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
