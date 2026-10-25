use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, ChatProvider};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;

/// ChatGPT - chatgpt.com. Contenteditable #prompt-textarea; markdown response divs.
pub struct ChatGPTAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://chatgpt.com",
    picker: &["button[data-testid=\"model-switcher\"]", "text:button=ChatGPT", "div#model-switcher"],
    input: &["#prompt-textarea", "div#prompt-textarea[contenteditable=\"true\"]", "textarea[data-id=\"root\"]"],
    send: &["button[data-testid=\"send-button\"]", "button[aria-label=\"Send prompt\"]", "button:has(svg)"],
    response: &["div[data-message-author-role=\"assistant\"] div.markdown", "article div.markdown", "div[data-message-id] div.markdown"],
    model_keywords: "chatgpt|sonar|grok|mistral|claude|gpt|gemini|qwen|llama",
    model_pairs: &[("button[data-testid='model-switcher-dropdown-button'], button[aria-label*='model' i], [data-testid='model-switcher-dropdown-button']", "[role='menuitem'], [role='option'], [data-testid*='model' i]")],
};

#[async_trait(?Send)]
impl ChatProvider for ChatGPTAdapter {
    fn name(&self) -> &str {
        "chatgpt"
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
