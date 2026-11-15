use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, SiteAdapter};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;

/// Grok - grok.com (also x.com/i/grok). Model picker shows Grok 3 vs Mini; thinking traces.
pub struct GrokAdapter {
    pub cfg: ProviderConfig,
}

// Keep explicit send controls only; generic submit/SVG fallbacks can hit unrelated forms.
pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://grok.com",
    picker: &[
        "text:button=Grok",
        "select.model-selector",
        "div.model-picker",
    ],
    input: &[
        // Exact existing chat identifiers beat broad Ask/search candidates.
        // Never derive new production selectors from synthetic fixtures.
        "textarea[aria-label=\"Ask Grok anything\"]",
        "textarea[name=\"message\"]",
        "div[contenteditable=\"true\"][role=\"textbox\"]",
        "textarea[placeholder*=\"Ask\"]",
    ],
    send: &["button[aria-label=\"Send\"]", "button.send-button"],
    response: &[
        "div[data-testid=\"grok-response\"]",
        "div.markdown",
        "div.response-content",
    ],
    model_keywords: "grok",
    model_pairs: &[],
};

#[async_trait(?Send)]
impl SiteAdapter for GrokAdapter {
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
        common::discover_models(page, &SELECTORS, &self.cfg.models, &self.cfg.selectors).await
    }

    async fn send_prompt(&self, page: &mut Page, prompt: &str) -> Result<()> {
        common::require_submission_provider_route("grok", page)?;
        common::send_prompt(
            self.name(),
            page,
            &SELECTORS.effective(&self.cfg.selectors),
            prompt,
        )
        .await
    }

    fn response_text(&self, page: &Page) -> Option<String> {
        common::response_text(
            page,
            &SELECTORS.effective(&self.cfg.selectors).response_refs(),
        )
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

#[cfg(test)]
mod labelled_composer_priority_tests {
    use super::*;

    #[tokio::test]
    async fn labelled_chat_editor_wins_over_search_and_clicks_its_own_send() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <form id="search">
              <textarea id="search-input" placeholder="Ask about docs"></textarea>
              <textarea aria-label="Ask Grok anything" hidden></textarea>
              <button id="search-send" type="button" aria-label="Send"
                onclick="window.searchClicks=(window.searchClicks||0)+1">Search</button>
            </form>
            <form id="chat">
              <textarea id="chat-input" name="message" aria-label="Ask Grok anything"></textarea>
              <button id="chat-send" type="button" aria-label="Send"
                onclick="window.chatClicks=(window.chatClicks||0)+1">Chat</button>
            </form>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let effective = SELECTORS.effective(&Default::default());
        let inputs = effective.input_refs();
        common::human_type(&page, &inputs, "fixture request")
            .await
            .unwrap();
        assert_eq!(
            page.evaluate("document.querySelector('#chat-input').value").as_str(),
            Some("fixture request")
        );
        assert_eq!(
            page.evaluate("document.querySelector('#search-input').value").as_str(),
            Some("")
        );
        assert!(common::click_send_or_enter(
            &mut page,
            &effective.send_refs(),
            inputs[0],
            500,
        ).await.unwrap());
        assert_eq!(page.evaluate("window.chatClicks || 0").as_f64(), Some(1.0));
        assert_eq!(page.evaluate("window.searchClicks || 0").as_f64(), Some(0.0));
    }

    #[test]
    fn configured_input_overrides_still_take_priority_over_built_in_order() {
        let overrides = crate::config::ProviderSelectorOverrides {
            input: Some(vec!["#owner-specified-input".to_owned()]),
            ..Default::default()
        };
        let effective = SELECTORS.effective(&overrides);
        assert_eq!(effective.input_refs(), vec!["#owner-specified-input"]);
    }
}
