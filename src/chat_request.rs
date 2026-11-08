//! Fail-closed HTTP request validation shared by streaming and JSON chat.
//! Authentication owner identity and conversation IDs are resolved before
//! memory reads, routing, or backend dispatch.
use axum::{extract::Extension, http::{HeaderMap, StatusCode}, response::Response};
use crate::{api::{api_error, ChatReq}, app_state::AuthKeyContext, oversized_chat_body_error, parse_and_validate_chat_req, valid_conversation_id};

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
