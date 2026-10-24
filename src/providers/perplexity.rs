use async_trait::async_trait;
use obscura_core::Page;
use anyhow::Result;
use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, ChatProvider};

/// Perplexity - www.perplexity.ai. Focus modes + Sonar selector; answers carry citations (stripped).
pub struct PerplexityAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://www.perplexity.ai",
    picker: &["button:has-text(\"Sonar\")", "div.model-selector", "button[aria-label*=\"model\"]"],
    input: &["textarea[placeholder*=\"Ask\"]", "div[contenteditable=\"true\"][role=\"textbox\"]", "textarea[name=\"q\"]"],
    send: &["button[aria-label=\"Submit\"]", "button[type=\"submit\"]", "button:has(svg.lucide-arrow-up)"],
    response: &["div[data-testid=\"answer\"]", "div.prose", "div.answer-container"],
    model_keywords: "perplexity|sonar|grok|mistral|codestral|claude|gpt|gemini|qwen|llama|pixtral",
    model_pairs: &[],
};

#[async_trait(?Send)]
impl ChatProvider for PerplexityAdapter {
    fn name(&self) -> &str { "perplexity" }
    fn aliases(&self) -> &[String] { &self.cfg.models }
    fn strip_citations(&self) -> bool { true }

    async fn discover_models(&self, page: &mut Page) -> Result<Vec<String>> {
        common::discover_models(page, &SELECTORS, &self.cfg.models).await
    }

    async fn send_prompt(&self, page: &Page, prompt: &str) -> Result<()> {
        common::send_prompt(page, &SELECTORS, prompt).await
    }

    fn response_text(&self, page: &Page) -> Option<String> {
        common::response_text(page, &SELECTORS)
    }

    fn selectors(&self) -> AdapterSelectors { SELECTORS.adapter_selectors() }
    fn selector_set(&self) -> &common::SelectorSet { &SELECTORS }
}
