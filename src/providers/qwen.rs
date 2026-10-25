use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, ChatProvider};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;

/// Qwen - chat.qwen.ai. Model dropdown top-left; discovered labels like qwen3-235b.
pub struct QwenAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://chat.qwen.ai",
    picker: &["button:has-text(\"Qwen\")", "div.model-selector", "[data-testid=\"model-picker\"]"],
    input: &["textarea[placeholder*=\"Ask\"]", "textarea[data-testid=\"chat-input\"]", "div[contenteditable=\"true\"][role=\"textbox\"]"],
    send: &["button[type=\"submit\"]", "button[aria-label*=\"Send\"]", "button[data-testid=\"send-button\"]"],
    response: &["div.markdown-body", "div[data-role=\"assistant\"]", "div.message-content"],
    model_keywords: "qwen|sonar|grok|mistral|claude|gpt|gemini|qwen|llama",
    model_pairs: &[("[role='button'][aria-label*='select model' i], button[aria-label*='model' i], .model-selector button, [data-model-selector]", "[role='listbox'] [role='option'], [role='menuitem'], [role='option'], [data-model-option], .ant-select-item"), (".ant-select-selector", ".ant-select-item, [role='option']")],
};

#[async_trait(?Send)]
impl ChatProvider for QwenAdapter {
    fn name(&self) -> &str {
        "qwen"
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
