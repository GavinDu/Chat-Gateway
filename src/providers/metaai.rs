use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, ChatProvider};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;

/// Meta AI - www.meta.ai. Model switcher shows Llama 4 variants.
pub struct MetaAIAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://www.meta.ai",
    picker: &["text:button=Llama", "div.model-switcher", "button[aria-label*=\"model\"]"],
    input: &["div[contenteditable=\"true\"][role=\"textbox\"]", "textarea[placeholder*=\"Ask\"]", "div[contenteditable=\"true\"]"],
    send: &["button[aria-label=\"Send\"]", "button:has(svg[data-testid=\"send\"])"],
    response: &["div[data-testid=\"bot-message\"]", "div.message.assistant", "div.prose"],
    model_keywords: "metaai|meta|llama",
    model_pairs: &[("button[aria-label*='model' i], button[aria-label*='llama' i], [data-testid*='model' i], [data-model-selector]", "[role='menuitem'], [role='option'], [data-model-option]"), ("[role='button'][aria-haspopup='menu']", "[role='menuitem'], [role='option']")],
};

#[async_trait(?Send)]
impl ChatProvider for MetaAIAdapter {
    fn name(&self) -> &str {
        "metaai"
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
