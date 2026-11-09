use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, SiteAdapter};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;

/// Perplexity - www.perplexity.ai. Focus modes + Sonar selector; answers carry citations (stripped).
pub struct PerplexityAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://www.perplexity.ai",
    picker: &[
        "text:button=Sonar",
        "div.model-selector",
        "button[aria-label*=\"model\"]",
    ],
    input: &[
        "textarea[placeholder*=\"Ask\"]",
        "div[contenteditable=\"true\"][role=\"textbox\"]",
        "textarea[name=\"q\"]",
    ],
    send: &[
        "button[aria-label=\"Submit\"]",
        "button:has(svg.lucide-arrow-up)",
    ],
    response: &[
        "div[data-testid=\"answer\"]",
        "div.prose",
        "div.answer-container",
    ],
    model_keywords: "perplexity|sonar",
    model_pairs: &[],
};

#[async_trait(?Send)]
impl SiteAdapter for PerplexityAdapter {
    fn name(&self) -> &str {
        "perplexity"
    }
    fn aliases(&self) -> &[String] {
        &self.cfg.models
    }
    fn strip_citations(&self) -> bool {
        true
    }

    async fn discover_models(&self, page: &mut Page) -> Result<Vec<String>> {
        common::discover_models(page, &SELECTORS, &self.cfg.models, &self.cfg.selectors).await
    }

    async fn send_prompt(&self, page: &mut Page, prompt: &str) -> Result<()> {
        common::send_prompt(page, &SELECTORS.effective(&self.cfg.selectors), prompt).await
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
