use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, SiteAdapter};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;

/// Grok - grok.com (also x.com/i/grok). Model picker shows Grok 3 vs Mini; thinking traces.
pub struct GrokAdapter {
    pub cfg: ProviderConfig,
}

// Keep explicit send controls only; generic submit/SVG fallbacks can hit unrelated forms.
pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://grok.com",
    picker: &[
        "text:button=Grok",
        "select.model-selector",
        "div.model-picker",
    ],
    input: &[
        "textarea[placeholder*=\"Ask\"]",
        "div[contenteditable=\"true\"][role=\"textbox\"]",
        "textarea[name=\"message\"]",
        "textarea[aria-label=\"Ask Grok anything\"]",
    ],
    send: &["button[aria-label=\"Send\"]", "button.send-button"],
    response: &[
        "div[data-testid=\"grok-response\"]",
        "div.markdown",
        "div.response-content",
    ],
    model_keywords: "grok",
    model_pairs: &[],
};

#[async_trait(?Send)]
impl SiteAdapter for GrokAdapter {
    fn name(&self) -> &str {
        "grok"
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
        common::send_prompt(page, &SELECTORS.effective(&self.cfg.selectors), prompt).await
    }

    fn response_text(&self, page: &Page) -> Option<String> {
        common::response_text(page, &SELECTORS.effective(&self.cfg.selectors).response_refs())
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
