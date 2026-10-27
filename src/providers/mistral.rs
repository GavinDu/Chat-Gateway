use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, SiteAdapter};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;

/// Mistral (Le Chat) - chat.mistral.ai. Model picker top; Large/Small/Codestral/Pixtral.
pub struct MistralAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://chat.mistral.ai",
    picker: &["button.model-selector", "text:div=Mistral Large", "select"],
    input: &[
        "textarea[placeholder*=\"Ask\"]",
        "div[contenteditable=\"true\"]",
        "textarea.chat-input",
    ],
    send: &["button[aria-label=\"Send\"]", "button.submit"],
    response: &["div.markdown", "div.assistant-message", "div.prose"],
    model_keywords: "mistral|codestral|pixtral|ministral",
    model_pairs: &[],
};

#[async_trait(?Send)]
impl SiteAdapter for MistralAdapter {
    fn name(&self) -> &str {
        "mistral"
    }
    fn aliases(&self) -> &[String] {
        &self.cfg.models
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
