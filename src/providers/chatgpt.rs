use async_trait::async_trait;
use obscura_core::Page;
use anyhow::Result;
use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, ChatProvider};

/// ChatGPT - chatgpt.com. Contenteditable #prompt-textarea; markdown response divs.
pub struct ChatGPTAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://chatgpt.com",
    picker: &["button[data-testid=\"model-switcher\"]", "button:has-text(\"ChatGPT\")", "div#model-switcher"],
    input: &["#prompt-textarea", "div#prompt-textarea[contenteditable=\"true\"]", "textarea[data-id=\"root\"]"],
    send: &["button[data-testid=\"send-button\"]", "button[aria-label=\"Send prompt\"]", "button:has(svg)"],
    response: &["div[data-message-author-role=\"assistant\"] div.markdown", "article div.markdown", "div[data-message-id] div.markdown"],
    model_keywords: "chatgpt|sonar|grok|mistral|claude|gpt|gemini|qwen|llama",
};

#[async_trait(?Send)]
impl ChatProvider for ChatGPTAdapter {
    fn name(&self) -> &str { "chatgpt" }
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
