pub(crate) mod health;

use crate::{chat_backend::Role, context, memory::ChatMessage};
use axum::{response::IntoResponse, Json};
use std::collections::HashMap;

#[derive(serde::Deserialize)]
pub(crate) struct ChatReq {
    pub(crate) model: String,
    #[serde(deserialize_with = "deserialize_chat_messages")]
    pub(crate) messages: Vec<ChatMessage>,
    #[serde(default)]
    pub(crate) stream: bool,
    #[serde(default)]
    pub(crate) temperature: Option<f32>,
    // Capture unknown OpenAI-style fields so validation can return a clear
    // unsupported_parameter error instead of misreporting them as malformed JSON.
    #[serde(flatten)]
    pub(crate) unsupported_fields: HashMap<String, serde_json::Value>,
}

/// Only canonical text messages are supported by browser adapters. Parsing
/// directly into the persistence type would silently discard unrecognized
/// OpenAI message fields such as tool_calls, tool_call_id, name, or refusal.
/// Reject them at the wire boundary rather than claim a successful send after
/// dropping their semantics. The stored ChatMessage schema remains unchanged.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTextMessage {
    role: String,
    content: String,
}

fn deserialize_chat_messages<'de, D>(deserializer: D) -> Result<Vec<ChatMessage>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let messages = <Vec<WireTextMessage> as serde::Deserialize>::deserialize(deserializer)?;
    Ok(messages
        .into_iter()
        .map(|message| ChatMessage {
            role: message.role,
            content: message.content,
        })
        .collect())
}

/// Validate request semantics separately from JSON decoding so the contract
/// can be exercised without initializing browser-backed providers.
pub(crate) fn validate_chat_req(req: &ChatReq) -> Result<(), (&'static str, String)> {
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
        if Role::from_wire(&message.role).is_none() {
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

pub(crate) fn estimate_tokens(text: &str) -> u64 {
    // Share the same conservative estimator used by context budgeting.
    context::estimate_message_tokens(text) as u64
}

pub(crate) fn add_gateway_headers(
    headers: &mut axum::http::HeaderMap,
    provider: &str,
    model: &str,
) {
    if let Ok(value) = axum::http::HeaderValue::from_bytes(provider.as_bytes()) {
        headers.insert("X-Gateway-Provider", value);
    }
    if let Ok(value) = axum::http::HeaderValue::from_bytes(model.as_bytes()) {
        headers.insert("X-Gateway-Model", value);
    }
}

pub(crate) fn api_error(
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

#[cfg(test)]
mod wire_chat_message_contract_tests {
    use super::ChatReq;

    #[test]
    fn canonical_text_messages_across_supported_roles_still_deserialize() {
        let request: ChatReq = serde_json::from_value(serde_json::json!({
            "model": "auto",
            "messages": [
                {"role": "system", "content": "instructions"},
                {"role": "developer", "content": "guidance"},
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"},
                {"role": "tool", "content": "plain text result"}
            ]
        }))
        .unwrap();
        assert_eq!(request.messages.len(), 5);
        assert_eq!(request.messages[2].content, "hello");
    }

    #[test]
    fn rejects_message_metadata_that_cannot_be_preserved_by_browser_backend() {
        for (field, value) in [
            ("tool_calls", serde_json::json!([])),
            ("tool_call_id", serde_json::json!("call_123")),
            ("name", serde_json::json!("tool_name")),
            ("refusal", serde_json::json!("no")),
            (
                "function_call",
                serde_json::json!({"name": "go", "arguments": "{}"}),
            ),
            ("audio", serde_json::json!({"id": "a"})),
        ] {
            let mut request = serde_json::json!({
                "model": "auto",
                "messages": [{"role": "assistant", "content": "text"}]
            });
            request["messages"][0]
                .as_object_mut()
                .unwrap()
                .insert(field.to_owned(), value);
            let error = serde_json::from_value::<ChatReq>(request)
                .err()
                .expect("unsupported message attributes must not vanish");
            assert!(
                error.to_string().contains(field),
                "missing unsupported field name {field}"
            );
        }
    }

    #[test]
    fn rejects_multimodal_content_instead_of_flattening_it_into_text() {
        let request = serde_json::json!({
            "model": "auto",
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "hello"}]
            }]
        });
        assert!(serde_json::from_value::<ChatReq>(request).is_err());
    }

    #[test]
    fn typed_roles_accept_only_exact_supported_wire_values() {
        for role in ["system", "developer", "user", "assistant", "tool"] {
            let request: ChatReq = serde_json::from_value(serde_json::json!({
                "model": "auto",
                "messages": [{"role": role, "content": "hello"}]
            }))
            .unwrap();
            assert!(super::validate_chat_req(&request).is_ok(), "{role}");
        }
        for role in ["User", "assistant ", "function", ""] {
            let request: ChatReq = serde_json::from_value(serde_json::json!({
                "model": "auto",
                "messages": [{"role": role, "content": "hello"}]
            }))
            .unwrap();
            let error = super::validate_chat_req(&request).unwrap_err();
            assert_eq!(error.0, "invalid_message_role", "{role}");
        }
    }
}
