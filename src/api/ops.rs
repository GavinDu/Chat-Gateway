//! Operator observability and policy status endpoints.
//!
//! These handlers retain the existing metrics text contract and fail-closed
//! memory egress policy envelope; routes remain protected by the same middleware.
use super::api_error;
use crate::{app_state::AppState, memory_egress};
use axum::{extract::State, http::StatusCode, response::{IntoResponse, Response}, Json};
use std::sync::Arc;

pub(crate) async fn memory_egress_policy_status() -> Response {
    match memory_egress::MemoryEgressPolicy::from_env() {
        Ok(policy) => Json(serde_json::json!({
            "personal_long_term_memory": {
                "default": "deny",
                "allowed_providers": policy.allowed_personal_memory_providers(),
            },
            "current_turn": "allowed",
            "conversation_context": "allowed",
            "personal_memory_retrieval_wired": true,
        }))
        .into_response(),
        Err(_) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "memory_egress_policy_invalid",
            "provider memory egress policy is invalid",
        ),
    }
}

pub(crate) async fn metrics_endpoint(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let provider_states = state
        .cfg
        .providers
        .iter()
        .map(|provider| {
            (
                provider.name.clone(),
                state
                    .provider_sessions
                    .status(&provider.name)
                    .state
                    .as_str()
                    .to_owned(),
            )
        })
        .collect::<Vec<_>>();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    (
        headers,
        state.metrics.render_with_provider_states(&provider_states),
    )
}
