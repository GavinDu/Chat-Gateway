//! Provider thread reopening and safe landing-page navigation guards.
//!
//! Compare canonical provider URLs and require a loaded composer before
//! reusing a persisted thread or accepting a fresh provider landing route.
use super::pool_policy;
use crate::providers;

/// A saved thread URL is not enough to prove that its composer has loaded.
/// In particular, an SPA can publish the new address before replacing the
/// previous page. Wait for both the exact saved thread identity and the
/// provider's input before allowing a delta-only follow-up.
pub(super) fn provider_thread_navigation_ready(
    actual: &str,
    target: &str,
    page: &obscura_core::Page,
    adapter: &dyn providers::SiteAdapter,
) -> bool {
    let composer_ready = providers::common::visible_composer_exists(
        page,
        &adapter.effective_selectors().input_refs(),
    );
    thread_navigation_snapshot_ready(actual, target, composer_ready)
}

pub(super) fn thread_navigation_snapshot_ready(
    actual: &str,
    target: &str,
    composer_ready: bool,
) -> bool {
    composer_ready && pool_policy::provider_thread_navigation_matches(actual, target)
}

/// Landing pages can canonicalize the provider root to an app route. Accept
/// that redirect only on the same origin and only after the adapter composer
/// is present; thread URLs use the stricter path comparison above.
pub(super) fn provider_landing_navigation_ready(
    actual: &str,
    target: &str,
    page: &obscura_core::Page,
    adapter: &dyn providers::SiteAdapter,
) -> bool {
    let composer_ready = providers::common::visible_composer_exists(
        page,
        &adapter.effective_selectors().input_refs(),
    );
    landing_navigation_snapshot_ready(actual, target, adapter.name(), composer_ready)
}

/// The landing readiness decision is intentionally pure so the real browser
/// path uses the same safe-origin and fresh-thread rules exercised by CI.
pub(super) fn landing_navigation_snapshot_ready(
    actual: &str,
    target: &str,
    provider: &str,
    composer_ready: bool,
) -> bool {
    composer_ready
        && pool_policy::provider_thread_url_is_same_origin(actual, target)
        && pool_policy::landing_is_not_previous_thread(provider, actual)
}

#[cfg(test)]
mod provider_reopen_composer_tests {
    use super::{landing_navigation_snapshot_ready, thread_navigation_snapshot_ready};

    #[tokio::test]
    async fn navigation_uses_configured_visible_editor_not_hidden_builtin_match() {
        let cfg: crate::config::AppConfig = serde_yaml::from_str(
            r#"
providers:
  - name: chatgpt
    selectors:
      input:
        - "textarea#custom-ready"
"#,
        )
        .unwrap();
        let registry = crate::providers::build_registry(&cfg);
        let adapter = registry.get(&crate::providers::Provider::ChatGPT).unwrap();
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <div id="prompt-textarea" contenteditable="true" style="display:none"></div>
            <textarea id="custom-ready">ready</textarea>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        assert!(super::provider_landing_navigation_ready(
            "https://chatgpt.com/",
            "https://chatgpt.com/",
            &page,
            adapter.as_ref()
        ));
        assert!(super::provider_thread_navigation_ready(
            "https://chatgpt.com/c/saved",
            "https://chatgpt.com/c/saved",
            &page,
            adapter.as_ref()
        ));

        // A stale built-in editor cannot bypass a disabled configured input.
        page.evaluate("document.querySelector('#prompt-textarea').style.display = 'block'");
        page.evaluate("document.querySelector('#custom-ready').disabled = true");
        assert!(!super::provider_landing_navigation_ready(
            "https://chatgpt.com/",
            "https://chatgpt.com/",
            &page,
            adapter.as_ref()
        ));
        assert!(!super::provider_thread_navigation_ready(
            "https://chatgpt.com/c/saved",
            "https://chatgpt.com/c/saved",
            &page,
            adapter.as_ref()
        ));
    }

    #[test]
    fn saved_thread_reopen_needs_matching_path_and_ready_composer() {
        assert!(!thread_navigation_snapshot_ready(
            "https://chatgpt.com/c/saved",
            "https://chatgpt.com/c/saved",
            false,
        ));
        assert!(!thread_navigation_snapshot_ready(
            "https://chatgpt.com/c/old",
            "https://chatgpt.com/c/saved",
            true,
        ));
        assert!(thread_navigation_snapshot_ready(
            "https://chatgpt.com/c/saved/?source=redirect",
            "https://chatgpt.com/c/saved",
            true,
        ));
    }

    #[test]
    fn fresh_landing_requires_verified_https_origin_and_fresh_thread() {
        assert!(landing_navigation_snapshot_ready(
            "https://chatgpt.com/?utm_source=home",
            "https://chatgpt.com/",
            "chatgpt",
            true,
        ));
        for untrusted in [
            "http://chatgpt.com/",
            "https://chatgpt.com@evil.example/",
            "https://chatgpt%2ecom/",
            "https://chatgpt.com/c/existing",
            "https://chatgpt.com/?conversation_id=existing",
        ] {
            assert!(!landing_navigation_snapshot_ready(
                untrusted,
                "https://chatgpt.com/",
                "chatgpt",
                true,
            ));
        }
        assert!(!landing_navigation_snapshot_ready(
            "https://chatgpt.com/",
            "https://chatgpt.com/",
            "chatgpt",
            false,
        ));
    }
}

/// Unlike assigning window.location through evaluate, goto actually consumes
/// navigation work in the pinned browser and creates a fresh document.
pub(super) async fn navigate_provider_document(
    page: &mut obscura_core::Page,
    target: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    tokio::time::timeout(timeout, page.goto(target))
        .await
        .map_err(|_| anyhow::anyhow!("provider document navigation timed out"))??;
    Ok(())
}

#[cfg(test)]
mod real_navigation_tests {
    #[tokio::test]
    async fn same_address_navigation_replaces_the_previous_document() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let url = "data:text/html,<textarea id='composer'></textarea>";
        super::navigate_provider_document(&mut page, url, std::time::Duration::from_secs(2))
            .await
            .unwrap();
        page.evaluate("globalThis.oldDocumentMarker = true");
        super::navigate_provider_document(&mut page, url, std::time::Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(
            page.evaluate("typeof oldDocumentMarker"),
            serde_json::json!("undefined")
        );
        assert!(page.query_selector("#composer").is_some());
    }
}
