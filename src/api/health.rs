use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

use crate::{provider_guard, provider_state, AppState};

pub(crate) async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let providers = state
        .cfg
        .providers
        .iter()
        .map(|p| {
            let session = provider_state::status(&p.name);
            let challenge_disabled_reason = provider_guard::provider_disabled_reason(&p.name);
            serde_json::json!({
                "name": p.name,
                "enabled": p.enabled,
                "priority": p.priority,
                "models": p.models,
                "session_state": session.state.as_str(),
                "last_session_error": session.last_error,
                "challenge_disabled": challenge_disabled_reason.is_some(),
                "challenge_disabled_reason": challenge_disabled_reason,
            })
        })
        .collect::<Vec<_>>();
    Json(serde_json::json!({
        "status": "ok",
        "engine": "obscura-native (embedded)",
        "sessions_loaded": "sessions/<provider>.json (restored at warm-up)",
        "providers": providers,
    }))
}

pub(crate) fn readiness_response(enabled: usize, worker_alive: bool) -> Response {
    if !worker_alive {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "not_ready",
                "reason": "browser_worker_unavailable",
            })),
        )
            .into_response();
    }
    if enabled == 0 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "not_ready",
                "reason": "no_enabled_provider",
            })),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ready",
            "enabled_providers": enabled,
        })),
    )
        .into_response()
}

pub(crate) async fn readiness(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if !state.pool.worker_alive() {
        return readiness_response(0, false);
    }
    let routable = state
        .cfg
        .providers
        .iter()
        .filter(|p| {
            p.enabled
                && provider_state::is_routable(&p.name)
                && provider_guard::provider_disabled_reason(&p.name).is_none()
        })
        .count();
    readiness_response(routable, true)
}

#[cfg(test)]
mod readiness_contract_tests {
    use super::*;

    #[test]
    fn readiness_requires_at_least_one_routable_provider() {
        let not_ready = readiness_response(0, true);
        assert_eq!(not_ready.status(), StatusCode::SERVICE_UNAVAILABLE);

        let ready = readiness_response(1, true);
        assert_eq!(ready.status(), StatusCode::OK);

        let worker_dead = readiness_response(1, false);
        assert_eq!(worker_dead.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
