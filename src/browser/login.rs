use crate::config::AppConfig;

/// Interactive login flow with full cookie + localStorage capture
/// - Spawns headed Obscura instance (obscura serve --port 9223 --headed --stealth)
/// - Dashboard proxies screencast + input relay without exposing CDP/DOM
/// - On save, captures cookies via CDP Network.getAllCookies + localStorage via JS
/// - Saves to sessions/<provider>.json
pub async fn interactive_login(cfg: &AppConfig, provider: &str) -> anyhow::Result<()> {
    tracing::info!("Starting interactive login for {provider}...");

    let valid = ["qwen","metaai","claude","chatgpt","gemini","grok","mistral","perplexity"];
    if !valid.contains(&provider) {
        anyhow::bail!("invalid provider {}, valid: {:?}", provider, valid);
    }

    println!("Dashboard login recommended: run without --login and open http://localhost:8080/");
    println!("Provider: {}", provider);
    println!("The dashboard at / starts an isolated manual-login browser and streams its page into the login modal.");
    println!("Clicks, scrolling, keyboard controls, and text are relayed without exposing CDP or provider DOM.");
    println!("Login capture saves cookies and localStorage; authentication input and screenshots are not logged or persisted.");

    // For CLI path, we would:
    // 1. Connect to Obscura pool
    // 2. Create new BrowserContext isolated
    // 3. Navigate to provider URL
    // 4. Wait for user to complete login (headed mode)
    // 5. Call SessionStore::capture_from_page to save cookies + localStorage

    anyhow::bail!("CLI login deferred to dashboard - run without --login and use / dashboard for secure capture")
}
