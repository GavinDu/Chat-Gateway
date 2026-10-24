use async_trait::async_trait;
use obscura_core::Page;
use anyhow::Result;
use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, ChatProvider};

/// Claude - claude.ai. ProseMirror input; streaming via data-is-streaming.
pub struct ClaudeAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://claude.ai",
    picker: &["button:has-text(\"Claude\")", "div[data-testid=\"model-selector\"]", "button.model-selector"],
    input: &["div[contenteditable=\"true\"][data-placeholder]", "div.ProseMirror", "div[contenteditable=\"true\"]"],
    send: &["button[aria-label=\"Send Message\"]", "button:has(svg)[type=\"button\"]", "button[type=\"button\"]"],
    response: &["div[data-testid=\"assistant-message\"]", "div.font-claude-response", "div[data-is-streaming] .prose"],
    model_keywords: "claude|sonar|grok|mistral|claude|gpt|gemini|qwen|llama",
    model_pairs: &[("button[aria-haspopup='menu'][data-testid*='model' i], button[aria-label*='model' i], [data-testid='model-selector-dropdown']", "[role='menuitem'], [role='option'], [data-testid*='model-option' i]")],
};

#[async_trait(?Send)]
impl ChatProvider for ClaudeAdapter {
    fn name(&self) -> &str { "claude" }
    fn aliases(&self) -> &[String] { &self.cfg.models }
    fn model_pairs(&self) -> &[(&str, &str)] { SELECTORS.model_pairs }

    async fn select_model(&self, page: &Page, model: &str) -> Result<()> {
        common::select_model(page, SELECTORS.model_pairs, model, &self.cfg.models).await
    }
    fn strip_citations(&self) -> bool { false }

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
