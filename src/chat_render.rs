//! Wire-format rendering of OpenAI-compatible JSON/SSE completions.
//!
//! Both transports share identity, provider headers and usage math; the
//! request handler remains responsible for success and persistence semantics.
use axum::http::HeaderMap;

use crate::api::{add_gateway_headers, estimate_tokens};

/// One completion owns one identity, independently of its conversation.
/// Reuse this value for every event in a streamed response.
pub(crate) struct CompletionIdentity {
    id: String,
    created: i64,
}

impl CompletionIdentity {
    pub(crate) fn new() -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            created: chrono::Utc::now().timestamp(),
        }
    }
}

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
    identity: &CompletionIdentity,
    model: &str,
    delta: serde_json::Value,
    finish_reason: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "id": identity.id,
        "object": "chat.completion.chunk",
        "created": identity.created,
        "model": model,
        "choices": [{ "delta": delta, "index": 0, "finish_reason": finish_reason }],
    })
}

/// Pure response construction: the live handler controls persistence and
/// success metrics, while this helper only renders the OpenAI JSON envelope.
pub(crate) fn json_completion_body(
    identity: &CompletionIdentity,
    conversation_id: &str,
    model: &str,
    content: &str,
    prompt_tokens: u64,
) -> serde_json::Value {
    let completion_tokens = estimate_tokens(content);
    serde_json::json!({
        "id": identity.id,
        "object": "chat.completion",
        "created": identity.created,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transports_preserve_the_completion_timestamp_instead_of_sampling_each_event() {
        let identity = CompletionIdentity {
            id: "chatcmpl-synthetic".to_owned(),
            created: 1,
        };
        let json = json_completion_body(&identity, "conversation_a", "model", "answer", 1);
        let sse = sse_completion_chunk(&identity, "model", serde_json::json!({}), Some("stop"));
        assert_eq!(json["created"], 1);
        assert_eq!(sse["created"], 1);
        assert_eq!(json["id"], sse["id"]);
    }
}
