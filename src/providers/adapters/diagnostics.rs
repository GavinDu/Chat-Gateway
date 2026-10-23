use crate::config::AppConfig;
use crate::browser::pool::BrowserPool;
use crate::session::SessionStore;

pub async fn test_adapter(cfg: &AppConfig, provider: &str) -> anyhow::Result<()> {
    println!("🔍 Testing adapter '{}' with Obscura...", provider);
    println!("   Endpoint: {}", cfg.server.obscura_endpoint);

    // This is diagnostic and may send a test prompt per original Go README
    let pool = BrowserPool::connect(&cfg.server.obscura_endpoint, cfg).await?;
    let sessions = SessionStore::load_all(cfg).await?;

    let registry = super::build_registry(cfg);
    let adapter = registry.get(provider).ok_or_else(|| anyhow::anyhow!("provider {} not found", provider))?.clone();

    let permit = pool.acquire(provider).await?;
    let page = permit.browser.new_page("about:blank").await?;
    sessions.apply_to_page(provider, &page).await.ok();

    println!("→ Discovering models without sending message...");
    match adapter.discover_models(&page).await {
        Ok(models) => println!("✅ Discovered models: {:?}", models),
        Err(e) => println!("❌ Discovery failed: {}", e),
    }

    println!("→ Checking selectors: {:?}", adapter.selectors());
    Ok(())
}
