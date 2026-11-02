use super::*;
struct Revoking(String);
impl Drop for Revoking {
    fn drop(&mut self) {
        if let Ok(mut registry) = registry().lock() {
            registry.revoking.remove(&self.0);
        }
    }
}
pub async fn revoke(
    State(app): State<Arc<crate::AppState>>,
    headers: HeaderMap,
    Path(provider): Path<String>,
) -> Response {
    if !same_origin(&headers) {
        return error(StatusCode::FORBIDDEN, "same_origin_required");
    }
    let allowed = crate::authorize_api_request(
        &app.cfg.auth,
        headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok()),
        "/login/api/browser",
        &Method::DELETE,
    )
    .ok()
    .flatten()
    .is_some_and(|key| {
        key.provider_allowlist
            .is_none_or(|allowed| allowed.contains(&provider))
    });
    if !allowed {
        return error(StatusCode::FORBIDDEN, "owner_authorization_required");
    }
    if crate::providers::Provider::from_name(&provider).is_none() {
        return error(StatusCode::BAD_REQUEST, "unknown_provider");
    }
    if require_vault().is_err() {
        return error(StatusCode::SERVICE_UNAVAILABLE, "encrypted_vault_required");
    }
    let guard;
    {
        let Ok(mut registry) = registry().lock() else {
            return error(StatusCode::SERVICE_UNAVAILABLE, "browser_unavailable");
        };
        if !registry.revoking.insert(provider.clone()) {
            return error(StatusCode::CONFLICT, "session_revoking");
        }
        guard = Revoking(provider.clone());
        registry.tickets.retain(|_, (bound, _)| bound != &provider);
        if let Some(cancel) = registry.controls.get(&provider) {
            cancel.send_replace(true);
        }
    }
    // Save/cleanup must finish before deleting: a late snapshot must never
    // recreate a session after the endpoint has confirmed revocation.
    let stopped = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if registry()
                .lock()
                .map(|r| !r.active.contains(&provider))
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .is_ok();
    if !stopped {
        return error(StatusCode::SERVICE_UNAVAILABLE, "session_revoke_failed");
    }
    let removed = if app.cfg.provider(&provider).is_some_and(|p| p.enabled) {
        // Existing implementation clears warmed Obscura storage before deletion.
        app.pool.unlink_session(&provider).await
    } else {
        // Disabled providers have no warmed engine context. Never navigate to
        // an external provider merely to delete its saved session.
        crate::session::delete_session_snapshot(&provider)
    };
    let response = match removed {
        Ok(removed) => {
            app.provider_sessions.record_unlinked(&provider);
            (
                [(header::CACHE_CONTROL, "no-store")],
                Json(json!({"ok":true,"removed":removed})),
            )
                .into_response()
        }
        Err(_) => error(StatusCode::SERVICE_UNAVAILABLE, "session_revoke_failed"),
    };
    drop(guard);
    response
}
