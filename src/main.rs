use std::{path::PathBuf, sync::Arc};
use clap::Parser;
use tracing_subscriber::EnvFilter;

mod config;
mod browser;
mod session;
mod memory;
mod providers;
mod server;
mod models;

#[derive(Parser, Debug)]
#[command(name = "chat-gateway-rs", about = "Rust + Obscura Chat Gateway - unified memory over 8 providers")]
struct Cli {
    #[arg(long, default_value = "config.yaml")]
    config: PathBuf,

    #[arg(long, help = "Interactive login for provider (qwen|metaai|claude|chatgpt|gemini|grok|mistral|perplexity)")]
    login: Option<String>,

    #[arg(long, help = "Test adapter selectors live")]
    test_adapter: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let cli = Cli::parse();
    let cfg = config::AppConfig::load(&cli.config)?;

    if let Some(provider) = cli.login {
        return browser::login::interactive_login(&cfg, &provider).await;
    }
    if let Some(provider) = cli.test_adapter {
        return providers::adapters::diagnostics::test_adapter(&cfg, &provider).await;
    }

    let supervisor = Arc::new(browser::ObscuraSupervisor::start(&cfg.obscura).await?);
    let pool = browser::pool::BrowserPool::connect(&cfg.server.obscura_endpoint, &cfg).await?;
    let session_store = Arc::new(session::SessionStore::load_all(&cfg).await?);

    // Spawn file watcher for sessions/*.json hot-reload
    session_store.clone().spawn_watcher();

    let memory = Arc::new(memory::UnifiedMemory::new(cfg.memory.clone()));
    let model_cache = Arc::new(models::cache::ModelCache::load(&cfg).await);

    let cache_clone = model_cache.clone();
    let cfg_clone = cfg.clone();
    let pool_clone = pool.clone();
    let sessions_clone = session_store.clone();
    tokio::spawn(async move {
        if let Err(e) = cache_clone.refresh_all(&cfg_clone, &pool_clone, &sessions_clone).await {
            tracing::warn!("initial model discovery failed: {e}");
        }
    });

    let state = server::state::AppState {
        config: Arc::new(cfg.clone()),
        browser_pool: pool,
        session_store,
        memory,
        model_cache,
        traces: Arc::new(server::state::TraceBuffer::new(500)),
        _supervisor: supervisor,
    };

    server::run(state).await?;
    Ok(())
}
