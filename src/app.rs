use std::sync::Arc;

use crate::{app_state::AppState, embedded_login};

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
