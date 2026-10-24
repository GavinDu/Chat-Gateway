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
};

#[async_trait(?Send)]
impl ChatProvider for ClaudeAdapter {
    fn name(&self) -> &str { "claude" }
    fn aliases(&self) -> &[String] { &self.cfg.models }
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
}
