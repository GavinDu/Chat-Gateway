//! Owner-scoped long-term memory HTTP handlers.

use axum::{
    extract::{Extension, Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

use crate::{
    api::api_error,
    app_state::{AppState, AuthKeyContext},
    memory::{self, LongTermMemoryStore},
};

#[derive(Debug, serde::Deserialize)]
pub(crate) struct LongTermMemorySettingsRequest {
    enabled: bool,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct LongTermMemoryItemRequest {
    memory_key: String,
    value: String,
    source_conversation_id: Option<String>,
    confidence: Option<f64>,
    expires_at: Option<i64>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct DeleteAllLongTermMemoryRequest {
    confirm: bool,
}

fn memory_owner(auth_context: &Option<Extension<AuthKeyContext>>) -> &str {
    auth_context
        .as_ref()
        .map(|Extension(context)| context.key_id.as_str())
        .unwrap_or("local")
}

pub(crate) async fn get_long_term_memory_settings(
    State(state): State<Arc<AppState>>,
    auth_context: Option<Extension<AuthKeyContext>>,
) -> Response {
    let owner = memory_owner(&auth_context).to_owned();
    match state
        .memory
        .clone()
        .long_term_memory_enabled_async(owner)
        .await
    {
        Ok(enabled) => Json(serde_json::json!({ "enabled": enabled })).into_response(),
        _ => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "memory_read_failed",
            "memory settings could not be read safely",
        ),
    }
}

pub(crate) async fn put_long_term_memory_settings(
    State(state): State<Arc<AppState>>,
    auth_context: Option<Extension<AuthKeyContext>>,
    Json(request): Json<LongTermMemorySettingsRequest>,
) -> Response {
    let owner = memory_owner(&auth_context).to_owned();
    let enabled = request.enabled;
    match state
        .memory
        .clone()
        .set_long_term_memory_enabled_async(owner, enabled)
        .await
    {
        Ok(()) => Json(serde_json::json!({ "enabled": enabled })).into_response(),
        _ => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "memory_settings_write_failed",
            "memory settings could not be saved safely",
        ),
    }
}

pub(crate) async fn list_long_term_memory_items(
    State(state): State<Arc<AppState>>,
    auth_context: Option<Extension<AuthKeyContext>>,
) -> Response {
    let owner = memory_owner(&auth_context).to_owned();
    match state
        .memory
        .clone()
        .long_term_memory_snapshot_async(owner)
        .await
    {
        Ok((enabled, items)) => Json(serde_json::json!({
            "enabled": enabled,
            "data": items,
        }))
        .into_response(),
        _ => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "memory_read_failed",
            "memory items could not be listed safely",
        ),
    }
}

pub(crate) async fn export_long_term_memory_items(
    State(state): State<Arc<AppState>>,
    auth_context: Option<Extension<AuthKeyContext>>,
) -> Response {
    let owner = memory_owner(&auth_context).to_owned();
    match state
        .memory
        .clone()
        .long_term_memory_snapshot_async(owner)
        .await
    {
        Ok((enabled, items)) => super::long_term_memory_export_response(enabled, items),
        _ => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "memory_export_failed",
            "memory items could not be exported safely",
        ),
    }
}

pub(crate) async fn save_long_term_memory_item(
    State(state): State<Arc<AppState>>,
    auth_context: Option<Extension<AuthKeyContext>>,
    Json(request): Json<LongTermMemoryItemRequest>,
) -> Response {
    let owner = memory_owner(&auth_context).to_owned();
    let memory_key = request.memory_key;
    let value = request.value;
    let source_conversation_id = request.source_conversation_id;
    let confidence = request.confidence.unwrap_or(1.0);
    let expires_at = request.expires_at;
    match state
        .memory
        .clone()
        .save_long_term_memory_item_async(
            owner,
            memory_key,
            value,
            source_conversation_id,
            confidence,
            expires_at,
        )
        .await
    {
        Ok(item) => (StatusCode::OK, Json(item)).into_response(),
        Err(error) if error.to_string().contains("disabled") => api_error(
            StatusCode::CONFLICT,
            "invalid_request_error",
            "long_term_memory_disabled",
            "enable long-term memory before saving items",
        ),
        Err(error) if error.to_string().contains("invalid long-term memory item") => api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_memory_item",
            "memory key, value, or confidence is invalid",
        ),
        Err(error) if error.to_string().contains("source conversation") => api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_memory_provenance",
            "source conversation must belong to this API key",
        ),
        Err(error) => {
            tracing::error!(%error, "long-term memory item save failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_write_failed",
                "memory item could not be saved safely",
            )
        }
    }
}

pub(crate) async fn delete_long_term_memory_item(
    State(state): State<Arc<AppState>>,
    Path(item_id): Path<String>,
    auth_context: Option<Extension<AuthKeyContext>>,
) -> Response {
    if item_id.is_empty() || item_id.len() > 64 {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_memory_item_id",
            "memory item ID is invalid",
        );
    }
    let owner = memory_owner(&auth_context).to_owned();
    match state
        .memory
        .clone()
        .delete_long_term_memory_item_async(owner, item_id)
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "memory_item_not_found",
            "memory item was not found",
        ),
        _ => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "memory_delete_failed",
            "memory item could not be deleted safely",
        ),
    }
}

pub(crate) async fn delete_all_long_term_memory_items(
    State(state): State<Arc<AppState>>,
    auth_context: Option<Extension<AuthKeyContext>>,
    Json(request): Json<DeleteAllLongTermMemoryRequest>,
) -> Response {
    if !request.confirm {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "confirmation_required",
            "set confirm=true to delete all of your long-term memory items",
        );
    }
    let owner = memory_owner(&auth_context).to_owned();
    match state
        .memory
        .clone()
        .delete_all_long_term_memory_items_async(owner)
        .await
    {
        Ok(deleted) => Json(serde_json::json!({ "deleted": deleted })).into_response(),
        Err(error) => {
            tracing::error!(%error, "long-term memory bulk deletion failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_delete_failed",
                "memory items could not be deleted safely",
            )
        }
    }
}
