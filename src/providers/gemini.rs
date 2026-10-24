use async_trait::async_trait;
use obscura_core::Page;
use anyhow::Result;
use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, ChatProvider};

/// Gemini - gemini.google.com. rich-textarea custom element; <model-response> answers.
pub struct GeminiAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://gemini.google.com",
    picker: &["button.model-picker", "mat-select", "button:has-text(\"Gemini\")"],
    input: &["rich-textarea[aria-label]", "div[contenteditable=\"true\"][role=\"textbox\"]", "div[contenteditable=\"true\"]"],
    send: &["button[aria-label=\"Send message\"]", "button.send-button", "button:has(mat-icon)"],
    response: &["model-response", "div.response-container", "div.model-response-text"],
    model_keywords: "gemini|sonar|grok|mistral|claude|gpt|gemini|qwen|llama",
};

#[async_trait(?Send)]
impl ChatProvider for GeminiAdapter {
    fn name(&self) -> &str { "gemini" }
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
