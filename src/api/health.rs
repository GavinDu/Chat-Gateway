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

pub(crate) fn readiness_response(
    enabled: usize,
    worker_alive: bool,
    worker_generation: u64,
) -> Response {
    if !worker_alive {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "not_ready",
                "reason": "browser_worker_unavailable",
                "worker_generation": worker_generation,
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
                "worker_generation": worker_generation,
            })),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ready",
            "enabled_providers": enabled,
            "worker_generation": worker_generation,
        })),
    )
        .into_response()
}

pub(crate) async fn readiness(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if !state.pool.worker_alive() {
        return readiness_response(0, false, state.pool.worker_generation());
    }
    let routable = state
        .cfg
        .providers
        .iter()
        .filter(|p| {
            p.enabled
                && provider_state::is_available(&p.name)
                && provider_guard::provider_disabled_reason(&p.name).is_none()
        })
        .count();
    readiness_response(routable, true, state.pool.worker_generation())
}

#[cfg(test)]
mod readiness_contract_tests {
    use super::*;

    #[test]
    fn readiness_requires_at_least_one_routable_provider() {
        let not_ready = readiness_response(0, true, 0);
        assert_eq!(not_ready.status(), StatusCode::SERVICE_UNAVAILABLE);

        let ready = readiness_response(1, true, 0);
        assert_eq!(ready.status(), StatusCode::OK);

        let worker_dead = readiness_response(1, false, 3);
        assert_eq!(worker_dead.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn readiness_reports_the_last_successfully_warmed_worker_generation() {
        let response = readiness_response(1, true, 7);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("readiness body should be readable");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("readiness body should be JSON");
        assert_eq!(body["worker_generation"], 7);
    }

    #[tokio::test]
    async fn not_ready_reports_generation_while_worker_is_down() {
        let response = readiness_response(0, false, 4);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("readiness body should be readable");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("readiness body should be JSON");
        assert_eq!(body["reason"], "browser_worker_unavailable");
        assert_eq!(body["worker_generation"], 4);
    }
}
