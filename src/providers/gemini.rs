use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, ChatProvider};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;

/// Gemini - gemini.google.com. rich-textarea custom element; <model-response> answers.
pub struct GeminiAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://gemini.google.com",
    picker: &["button.model-picker", "mat-select", "text:button=Gemini"],
    input: &[
        "rich-textarea[aria-label]",
        "div[contenteditable=\"true\"][role=\"textbox\"]",
        "div[contenteditable=\"true\"]",
    ],
    send: &[
        "button[aria-label=\"Send message\"]",
        "button.send-button",
        "button:has(mat-icon)",
    ],
    response: &[
        "model-response",
        "div.response-container",
        "div.model-response-text",
    ],
    model_keywords: "gemini|sonar|grok|mistral|claude|gpt|gemini|qwen|llama",
    model_pairs: &[(
        "button[aria-label*='model' i], [data-test-id*='model' i], .bard-mode-switcher",
        "[role='menuitem'], [role='option'], mat-option, [data-test-id*='model-option' i]",
    )],
};

#[async_trait(?Send)]
impl ChatProvider for GeminiAdapter {
    fn name(&self) -> &str {
        "gemini"
    }
    fn aliases(&self) -> &[String] {
        &self.cfg.models
    }
    fn model_pairs(&self) -> &[(&str, &str)] {
        SELECTORS.model_pairs
    }

    async fn select_model(&self, page: &Page, model: &str) -> Result<()> {
        common::select_model(page, SELECTORS.model_pairs, model, &self.cfg.models).await
    }
    fn strip_citations(&self) -> bool {
        false
    }

    async fn discover_models(&self, page: &mut Page) -> Result<Vec<String>> {
        common::discover_models(page, &SELECTORS, &self.cfg.models).await
    }

    async fn send_prompt(&self, page: &Page, prompt: &str) -> Result<()> {
        common::send_prompt(page, &SELECTORS, prompt).await
    }

    fn response_text(&self, page: &Page) -> Option<String> {
        common::response_text(page, &SELECTORS)
    }

    fn selectors(&self) -> AdapterSelectors {
        SELECTORS.adapter_selectors()
    }
    fn selector_set(&self) -> &common::SelectorSet {
        &SELECTORS
    }
}
