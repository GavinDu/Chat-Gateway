use async_trait::async_trait;
use obscura_core::Page;
use anyhow::Result;
use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, ChatProvider};

/// Qwen - chat.qwen.ai. Model dropdown top-left; discovered labels like qwen3-235b.
pub struct QwenAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://chat.qwen.ai",
    picker: &["button:has-text(\"Qwen\")", "div.model-selector", "[data-testid=\"model-picker\"]"],
    input: &["textarea[placeholder*=\"Ask\"]", "textarea[placeholder*=\"提问\"]", "textarea[data-testid=\"chat-input\"]", "div[contenteditable=\"true\"][role=\"textbox\"]", "textarea"],
    send: &["button[type=\"submit\"]", "button[aria-label*=\"Send\"]", "button[data-testid=\"send-button\"]"],
    response: &["div.chat-message:last-child div.markdown-body", "div[data-role=\"assistant\"]", "div.message-content", "div.markdown-body"],
    model_keywords: "qwen|sonar|grok|mistral|claude|gpt|gemini|qwen|llama",
};

#[async_trait(?Send)]
impl ChatProvider for QwenAdapter {
    fn name(&self) -> &str { "qwen" }
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

impl QwenAdapter {
    /// v2 helper kept: the first visible input element's selector from
    /// the i18n-robust fallback list, if one is on the page.
    pub fn find_input(page: &Page) -> Option<&'static str> {
        SELECTORS.input.iter().copied().find(|sel| {
            page.query_selector(sel)
                .map(|el| el.attribute("hidden").is_none())
                .unwrap_or(false)
        })
    }
}
