use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, SiteAdapter};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;

/// Manus web app. DOM selectors are conservative candidates until verified
/// against a signed-in account; no provider API key is required.
pub struct ManusAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://manus.im/app",
    picker: &[
        "button[aria-label*='model' i]",
        "[data-testid*='model-selector' i]",
    ],
    input: &[
        "textarea[placeholder*='Ask' i]",
        "textarea[placeholder*='message' i]",
        "[contenteditable='true'][role='textbox']",
        "div.ProseMirror[contenteditable='true']",
        "textarea",
    ],
    send: &[
        "button[aria-label='Send']",
        "button[aria-label*='Send' i]",
        "button[type='submit']",
    ],
    response: &[
        "[data-message-role='assistant']",
        "[data-role='assistant']",
        "[data-testid='assistant-message']",
        "[data-testid*='assistant-response' i]",
        "div.markdown-body",
        "div.prose",
    ],
    model_keywords: "manus|lite|max",
    model_pairs: &[],
};

#[async_trait(?Send)]
impl SiteAdapter for ManusAdapter {
    fn name(&self) -> &str {
        "manus"
    }

    fn aliases(&self) -> &[String] {
        &self.cfg.models
    }

    async fn discover_models(&self, page: &mut Page) -> Result<Vec<String>> {
        common::discover_models(page, &SELECTORS, &self.cfg.models, &self.cfg.selectors).await
    }

    async fn send_prompt(&self, page: &mut Page, prompt: &str) -> Result<()> {
        common::send_prompt(page, &SELECTORS.effective(&self.cfg.selectors), prompt).await
    }

    fn response_text(&self, page: &Page) -> Option<String> {
        common::response_text(page, &SELECTORS.effective(&self.cfg.selectors).response_refs())
    }

    fn selectors(&self) -> AdapterSelectors {
        SELECTORS.effective(&self.cfg.selectors).adapter_selectors()
    }

    fn selector_set(&self) -> &common::SelectorSet {
        &SELECTORS
    }
    fn effective_selectors(&self) -> common::EffectiveSelectorSet {
        SELECTORS.effective(&self.cfg.selectors)
    }
}
