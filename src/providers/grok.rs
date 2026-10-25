use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, ChatProvider};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;

/// Grok - grok.com (also x.com/i/grok). Model picker shows Grok 3 vs Mini; thinking traces.
pub struct GrokAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://grok.com",
    picker: &[
        "button:has-text(\"Grok\")",
        "select.model-selector",
        "div.model-picker",
    ],
    input: &[
        "textarea[placeholder*=\"Ask\"]",
        "div[contenteditable=\"true\"][role=\"textbox\"]",
        "textarea[name=\"message\"]",
    ],
    send: &[
        "button[aria-label=\"Send\"]",
        "button[type=\"submit\"]:has(svg)",
        "button.send-button",
    ],
    response: &[
        "div[data-testid=\"grok-response\"]",
        "div.markdown",
        "div.response-content",
    ],
    model_keywords: "grok|sonar|grok|mistral|codestral|claude|gpt|gemini|qwen|llama|pixtral",
    model_pairs: &[],
};

#[async_trait(?Send)]
impl ChatProvider for GrokAdapter {
    fn name(&self) -> &str {
        "grok"
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
