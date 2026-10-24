use async_trait::async_trait;
use obscura_core::Page;
use anyhow::Result;
use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, ChatProvider};

/// Mistral (Le Chat) - chat.mistral.ai. Model picker top; Large/Small/Codestral/Pixtral.
pub struct MistralAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://chat.mistral.ai",
    picker: &["button.model-selector", "div:has-text(\"Mistral Large\")", "select"],
    input: &["textarea[placeholder*=\"Ask\"]", "div[contenteditable=\"true\"]", "textarea.chat-input"],
    send: &["button[aria-label=\"Send\"]", "button[type=\"submit\"]", "button.submit"],
    response: &["div.markdown", "div.assistant-message", "div.prose"],
    model_keywords: "mistral|sonar|grok|mistral|codestral|claude|gpt|gemini|qwen|llama|pixtral",
};

#[async_trait(?Send)]
impl ChatProvider for MistralAdapter {
    fn name(&self) -> &str { "mistral" }
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
