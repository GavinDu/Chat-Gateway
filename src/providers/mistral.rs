use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, SiteAdapter};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;

/// Mistral (Le Chat) - chat.mistral.ai. Model picker top; Large/Small/Codestral/Pixtral.
pub struct MistralAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://chat.mistral.ai",
    picker: &["button.model-selector", "text:div=Mistral Large", "select"],
    input: &[
        "textarea[placeholder*=\"Ask\"]",
        "div[contenteditable=\"true\"]",
        "textarea.chat-input",
    ],
    send: &["button[aria-label=\"Send\"]", "button.submit"],
    response: &["div.markdown", "div.assistant-message", "div.prose"],
    model_keywords: "mistral|codestral|pixtral|ministral",
    model_pairs: &[],
};

#[async_trait(?Send)]
impl SiteAdapter for MistralAdapter {
    fn name(&self) -> &str {
        "mistral"
    }
    fn aliases(&self) -> &[String] {
        &self.cfg.models
    }
    fn strip_citations(&self) -> bool {
        false
    }

    async fn discover_models(&self, page: &mut Page) -> Result<Vec<String>> {
        common::discover_models(page, &SELECTORS, &self.cfg.models, &self.cfg.selectors).await
    }

    async fn send_prompt(&self, page: &mut Page, prompt: &str) -> Result<()> {
        let effective = SELECTORS.effective(&self.cfg.selectors);
        let inputs = effective.input_refs();
        if let Err(error) = common::wait_for_selector(page, &inputs, 15_000).await {
            let diagnostics = mistral_readiness_diagnostics(page);
            anyhow::bail!(
                "Mistral composer unavailable: {error}; {diagnostics}. Check provider JavaScript asset loading, session and network access; selector overrides cannot repair an HTTP 403"
            );
        }
        common::send_prompt(page, &effective, prompt).await
    }

    fn response_text(&self, page: &Page) -> Option<String> {
        common::response_text(
            page,
            &SELECTORS.effective(&self.cfg.selectors).response_refs(),
        )
    }

    fn selectors(&self) -> AdapterSelectors {
        SELECTORS.effective(&self.cfg.selectors).adapter_selectors()
    }
    fn selector_set(&self) -> &common::SelectorSet {
        &SELECTORS
    }
    fn effective_selectors(&self) -> common::EffectiveSelectorSet {
        SELECTORS.effective(&self.cfg.selectors)
    }
}

/// Collect non-sensitive readiness evidence without logging asset URLs,
/// cookie values, page content or authentication tokens. Resource timing
/// does not expose HTTP response status; never infer a 403 from this probe.
fn mistral_readiness_diagnostics(page: &Page) -> String {
    let evidence = page.evaluate(
        r#"(() => ({
            readyState: document.readyState,
            scriptCount: document.scripts.length,
            loadedResources: performance.getEntriesByType('resource').filter(e => e.initiatorType === 'script').length,
            challenge: !!document.querySelector('[id*="challenge"], [class*="challenge"], iframe[src*="challenge"]')
        }))()"#,
    );
    let state = evidence
        .get("readyState")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let scripts = evidence
        .get("scriptCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let resources = evidence
        .get("loadedResources")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let challenge = evidence
        .get("challenge")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    format!(
        "document_ready={state}, script_elements={scripts}, script_resource_entries={resources}, challenge_marker={challenge}; HTTP asset status not observable from DOM"
    )
}

#[cfg(test)]
mod readiness_tests {
    #[tokio::test]
    async fn missing_composer_returns_actionable_readiness_error() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body><script>window.ready=true</script></body></html>")
            .await
            .unwrap();
        let diagnostics = super::mistral_readiness_diagnostics(&page);
        assert!(diagnostics.contains("script_elements="));
        assert!(diagnostics.contains("HTTP asset status not observable from DOM"));
        assert!(!diagnostics.contains("data:text/html"));
    }
}
