//! Embedded browser construction with existing storage and stealth defaults.
use crate::config::AppConfig;
use anyhow::Result;

pub(super) fn build_browser(cfg: &AppConfig) -> Result<obscura_core::Browser> {
    let mut builder = obscura_core::Browser::builder().stealth(cfg.obscura.stealth);
    if let Some(user_agent) = cfg
        .obscura
        .user_agent
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        builder = builder.user_agent(user_agent.to_owned());
    }
    let storage = cfg
        .obscura
        .storage_dir
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if cfg.server.chrome_profile_dir.is_empty() {
                None
            } else {
                Some(cfg.server.chrome_profile_dir.clone())
            }
        });
    if let Some(dir) = storage {
        builder = builder.storage_dir(dir);
    }
    Ok(builder.build()?)
}

/// Apply page-level compatibility before any provider script executes.
pub(super) async fn new_page(browser: &obscura_core::Browser) -> Result<obscura_core::Page> {
    let mut page = browser.new_page().await?;
    page.add_preload_script(include_str!("iframe_sandbox.js"));
    page.add_preload_script(include_str!("window_compat.js"));
    Ok(page)
}

#[cfg(test)]
mod compatibility_tests {

    #[tokio::test]
    async fn provider_scripts_can_assign_iframe_sandbox_before_and_after_navigation() {
        let html = r#"<!doctype html><script>
        'use strict';
        const frame = document.createElement('iframe');
        const original = frame.sandbox;
        try {
          frame.sandbox = 'allow-scripts';
          globalThis.sandboxContract = original === frame.sandbox &&
            frame.getAttribute('sandbox') === 'allow-scripts' &&
            frame.sandbox.contains('allow-scripts') &&
            !frame.sandbox.contains('allow-same-origin');
        } catch { globalThis.sandboxContract = false; }
        </script>"#;
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        let url = format!("data:text/html,{encoded}");
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = super::new_page(&browser).await.unwrap();
        for _ in 0..2 {
            page.goto(&url).await.unwrap();
            assert_eq!(
                page.evaluate("globalThis.sandboxContract"),
                serde_json::json!(true)
            );
        }
    }

    #[tokio::test]
    async fn window_origin_is_available_before_first_script_and_after_navigation() {
        let html = r#"<!doctype html><script>
        'use strict';
        globalThis.originCompat = typeof window.origin === 'string' &&
          window.origin === new URL(window.location.href).origin &&
          Object.getOwnPropertyDescriptor(window, 'origin') !== undefined;
        </script>"#;
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        let url = format!("data:text/html,{encoded}");
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = super::new_page(&browser).await.unwrap();
        for _ in 0..2 {
            page.goto(&url).await.unwrap();
            assert_eq!(
                page.evaluate("globalThis.originCompat"),
                serde_json::json!(true),
                "the page must expose window.origin before app scripts execute"
            );
        }
    }
}
