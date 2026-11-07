use crate::{app_state::AppState, browser::pool::BrowserWorkerUnavailableError, embedded_login};
use std::{sync::Arc, time::Duration};

/// Stop accepting new connections on SIGINT/SIGTERM and let Axum drain
/// in-flight requests before the process exits.
pub(crate) async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut terminate =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    tracing::warn!(%error, "failed to listen for Ctrl-C");
                }
            }
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "failed to listen for Ctrl-C");
        }
    }

    embedded_login::shutdown_all().await;
    tracing::info!("shutdown signal received; draining in-flight HTTP requests");
}

pub(crate) async fn refresh_models(state: &Arc<AppState>) {
    for provider in &state.cfg.providers {
        if !provider.enabled {
            continue;
        }
        match state.pool.discover(&provider.name).await {
            Ok(models) => {
                state
                    .models_cache
                    .write()
                    .await
                    .insert(provider.name.clone(), models);
            }
            Err(error) => {
                tracing::warn!(provider = %provider.name, "model discovery failed: {error}")
            }
        }
    }
}

/// Start the process-owned background jobs after application state is ready.
/// These tasks are deliberately centralized so startup wiring stays in main.
pub(crate) fn start_background_workers(state: &Arc<AppState>) {
    // Model discovery reads provider pickers without sending messages.
    let state_for_models = state.clone();
    tokio::spawn(async move {
        refresh_models(&state_for_models).await;
    });

    // Verify idle provider sessions on the browser worker. Each probe is
    // read-only and serialized by the provider page lock; stagger providers
    // to avoid synchronized browser work after startup.
    for provider in state
        .cfg
        .providers
        .iter()
        .filter(|provider| provider.enabled)
    {
        let pool = state.pool.clone();
        let provider_sessions = state.provider_sessions.clone();
        let provider_name = provider.name.clone();
        let jitter_seconds = provider_session_probe_jitter_seconds(&provider_name);
        tokio::spawn(async move {
            // Probe once immediately at startup; subsequent checks remain staggered.
            loop {
                // Providers that have not been linked via the relay should not
                // generate recurring authentication warnings. A future import
                // is picked up by the next scheduled verification.
                match crate::session::load_session_file(&provider_name) {
                    Ok(Some(_)) => {},
                    Ok(None) => {
                        tokio::time::sleep(Duration::from_secs(60 + jitter_seconds)).await;
                        continue;
                    }
                    Err(_) => {
                        tracing::warn!(
                            provider = %provider_name,
                            "periodic session verification skipped: stored session unavailable"
                        );
                        tokio::time::sleep(Duration::from_secs(60 + jitter_seconds)).await;
                        continue;
                    }
                }
                match pool.verify_session(&provider_name).await {
                    Ok(true) => provider_sessions.record_success(&provider_name),
                    // A false result means the page is not warmed yet, not that
                    // the provider session is invalid. Leave routing state unchanged.
                    Ok(false) => {}
                    Err(error)
                        if error
                            .downcast_ref::<crate::browser::pool::ProviderSessionExpiredError>()
                            .is_some() =>
                    {
                        provider_sessions.record_needs_login(&provider_name);
                        tracing::warn!(
                            provider = %provider_name,
                            "periodic provider session verification found login required"
                        );
                    }
                    Err(error)
                        if error
                            .downcast_ref::<BrowserWorkerUnavailableError>()
                            .is_some_and(|error| error.is_retryable()) =>
                    {
                        // A dead/restarting worker is not evidence of an expired provider
                        // session. Leave provider eligibility unchanged until the worker recovers.
                        tracing::debug!(
                            provider = %provider_name,
                            "browser worker unavailable during session verification"
                        );
                    }
                    Err(_error) => {
                        tracing::warn!(
                            provider = %provider_name,
                            "periodic provider session verification failed"
                        );
                        provider_sessions
                            .record_error(&provider_name, "periodic session verification failed");
                    }
                }
                tokio::time::sleep(Duration::from_secs(60 + jitter_seconds)).await;
            }
        });
    }

    // Retention is opt-in: a zero-day setting leaves conversation history untouched.
    let retention_days = state.cfg.memory.retention_days;
    if retention_days > 0 {
        let memory = state.memory.clone();
        let retention_seconds = i64::from(retention_days) * 86_400;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .min(i64::MAX as u64) as i64;
                let cutoff = now.saturating_sub(retention_seconds);
                match memory
                    .clone()
                    .purge_expired_conversations_blocking(cutoff)
                    .await
                {
                    Ok(deleted) if deleted > 0 => {
                        tracing::info!(deleted, retention_days, "purged expired conversations");
                    }
                    Ok(_) => {}
                    Err(error) => tracing::error!(%error, "conversation retention purge failed"),
                }
                interval.tick().await;
            }
        });
    }
}

fn provider_session_probe_jitter_seconds(provider_name: &str) -> u64 {
    provider_name
        .bytes()
        .fold(0u64, |sum, byte| sum + u64::from(byte))
        % 15
}

#[cfg(test)]
mod session_probe_jitter_tests {
    use super::provider_session_probe_jitter_seconds;

    #[test]
    fn provider_probe_jitter_is_deterministic_and_bounded() {
        for provider in ["chatgpt", "claude", "qwen", "metaai"] {
            let first = provider_session_probe_jitter_seconds(provider);
            assert_eq!(first, provider_session_probe_jitter_seconds(provider));
            assert!(first < 15);
        }
    }
}
