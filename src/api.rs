use crate::{context, memory::ChatMessage};
use axum::{response::IntoResponse, Json};
use std::collections::HashMap;

#[derive(serde::Deserialize)]
pub(crate) struct ChatReq {
    pub(crate) model: String,
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
        if !matches!(
            message.role.as_str(),
            "system" | "developer" | "user" | "assistant" | "tool"
        ) {
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

pub(crate) fn add_gateway_headers(headers: &mut axum::http::HeaderMap, provider: &str, model: &str) {
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
