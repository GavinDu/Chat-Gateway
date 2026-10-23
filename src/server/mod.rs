pub mod state;
pub mod routes;
pub mod dashboard;

use axum::{routing::get, Router};
use std::sync::Arc;

pub async fn run(state: state::AppState) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/health", get(routes::health))
        .route("/v1/models", get(routes::list_models))
        .route("/v1/chat/completions", axum::routing::post(routes::chat_completions))
        .route("/", get(dashboard::dashboard))
        .route("/api/traces", get(dashboard::traces_api))
        .route("/api/traces/:n", get(dashboard::traces_latest))
        .route("/api/login/:provider/stream", get(dashboard::login_stream))
        .with_state(Arc::new(state));

    let port_str = std::env::var("PORT").ok();
    let port: u16 = if let Some(p) = port_str { p.parse().unwrap_or(8080) } else { 8080 };

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    tracing::info!("listening on {} - dashboard at http://localhost:{}/", listener.local_addr()?, port);
    axum::serve(listener, app).await?;
    Ok(())
}
