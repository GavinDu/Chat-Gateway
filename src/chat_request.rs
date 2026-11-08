//! Fail-closed HTTP request validation shared by streaming and JSON chat.
//! Authentication owner identity and conversation IDs are resolved before
//! memory reads, routing, or backend dispatch.
use crate::{
    api::{api_error, validate_chat_req, ChatReq},
    app_state::AuthKeyContext,
};
use axum::{
    extract::Extension,
    http::{HeaderMap, StatusCode},
    response::Response,
};

/// Only syntactically and semantically valid requests proceed to memory or
/// provider access. This same boundary applies to JSON and SSE requests.
pub(crate) struct PreparedChatRequest {
    pub(crate) req: ChatReq,
    pub(crate) owner_key_id: String,
    pub(crate) conversation_id: Option<String>,
}

pub(crate) fn prepare_chat_request(
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

pub(crate) fn valid_conversation_id(value: &str) -> bool {
    (8..=128).contains(&value.len())
        && value.is_ascii()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub(crate) fn oversized_chat_body_error() -> Response {
    api_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "invalid_request_error",
        "request_too_large",
        "request body exceeds the 1 MiB limit",
    )
}

// Returning a ready-to-send Axum response keeps validation errors consistent at the handler boundary.
#[allow(clippy::result_large_err)]
fn parse_and_validate_chat_req(body: &[u8]) -> Result<ChatReq, Response> {
    let req = serde_json::from_slice::<ChatReq>(body).map_err(|error| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_json",
            format!("invalid or unsupported request field: {error}"),
        )
    })?;
    validate_chat_req(&req).map_err(|(code, message)| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            code,
            message,
        )
    })?;
    Ok(req)
}

