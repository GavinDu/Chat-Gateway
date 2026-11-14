use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, SiteAdapter};
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
    input: &["#prompt-textarea", "div#prompt-textarea[contenteditable=\"true\"]", "textarea[data-id=\"root\"]", "textarea#mobile-composer-prompt[name=\"prompt\"]", "form[data-chatgpt-composer] div.ProseMirror[contenteditable=\"true\"][role=\"textbox\"]"],
    send: &["form[data-chatgpt-composer] button[aria-label=\"Send\"]", "button[data-testid=\"send-button\"]", "button[aria-label=\"Send prompt\"]", "form:has(textarea#mobile-composer-prompt[name=\"prompt\"]) button[aria-label=\"Send message\"]"],
    response: &["[class*=\"DilResponseRoot-\"]", "div[data-message-author-role=\"assistant\"] div.markdown", "article div.markdown", "div[data-message-id] div.markdown"],
    model_keywords: "chatgpt|gpt|o1|o3|o4",
    model_pairs: &[("button[data-testid='model-switcher-dropdown-button'], button[aria-label*='model' i], [data-testid='model-switcher-dropdown-button']", "[role='menuitem'], [role='option'], [data-testid*='model' i]")],
};

/// A redesign fallback must not target a second form or a dialog editor.
fn require_unambiguous_form_composer(page: &Page) -> Result<()> {
    let safe = page.evaluate(r#"(() => {
        const candidates = Array.from(document.querySelectorAll('form[data-chatgpt-composer] div.ProseMirror[contenteditable="true"][role="textbox"]')).filter(el =>
            el.getClientRects().length > 0 && getComputedStyle(el).visibility !== 'hidden' &&
            !el.closest('[hidden], [inert], [aria-hidden="true"], [aria-disabled="true"], [aria-readonly="true"]'));
        return candidates.length === 0 || (candidates.length === 1 &&
            !candidates[0].closest('dialog, [role="dialog"], [role="search"]'));
    })()"#);
    anyhow::ensure!(
        safe.as_bool() == Some(true),
        "ChatGPT composer is ambiguous or belongs to a dialog"
    );
    Ok(())
}

#[async_trait(?Send)]
impl SiteAdapter for ChatGPTAdapter {
    fn name(&self) -> &str {
        "chatgpt"
    }
    fn aliases(&self) -> &[String] {
        &self.cfg.models
    }
    fn model_pairs(&self) -> &[(&str, &str)] {
        SELECTORS.model_pairs
    }

    async fn select_model(&self, page: &mut Page, model: &str) -> Result<()> {
        common::select_model(page, SELECTORS.model_pairs, model, &self.cfg.models).await
    }
    fn strip_citations(&self) -> bool {
        false
    }

    async fn discover_models(&self, page: &mut Page) -> Result<Vec<String>> {
        common::discover_models(page, &SELECTORS, &self.cfg.models, &self.cfg.selectors).await
    }

    async fn send_prompt(&self, page: &mut Page, prompt: &str) -> Result<()> {
        common::require_submission_provider_route("chatgpt", page)?;
        require_unambiguous_form_composer(page)?;
        common::send_prompt(page, &SELECTORS.effective(&self.cfg.selectors), prompt).await
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
mod composer_tests {
    #[tokio::test]
    async fn current_response_root_preserves_all_blocks_and_excludes_user_and_actions() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><main>
            <div class="group/user-message"><div class="MarkdownRoot-user">user echo</div></div></main>
            <div class="DilRenderer-new DilResponseRoot-current">
                <div class="MarkdownRoot-first"><p>first block</p></div>
                <div class="MarkdownRoot-second"><p>second block</p></div>
                <div class="MarkdownRoot-hidden" hidden>hidden block</div>
                <button>Reload response</button>
            </div>
            <button>Copy reply</button>
            <div class="DilResponseRoot-hidden" hidden>hidden stale answer</div>
            <div class="DilResponseRoot-pending"></div>
            </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let text =
            crate::providers::common::response_text(&page, super::SELECTORS.response).unwrap();
        assert_eq!(text, "first block\n\nsecond block");
        assert!(
            !text.contains("user echo") && !text.contains("Copy reply") && !text.contains("stale")
        );
        let probe =
            crate::providers::common::response_probe(&page, super::SELECTORS.response, false);
        assert_eq!(probe.text.as_deref(), Some(text.as_str()));
        assert_eq!(probe.count, 2);
    }

    #[tokio::test]
    async fn scoped_editor_rejects_dialogs_and_multiple_forms() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <input type="search"><div contenteditable="true" role="textbox">search</div>
            <form data-chatgpt-composer><div class="ProseMirror" contenteditable="true" role="textbox"></div><button aria-label="Send">Send</button></form>
            </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        assert!(super::require_unambiguous_form_composer(&page).is_ok());
        let inputs = super::SELECTORS.input;
        assert!(crate::providers::common::visible_composer_exists(
            &page, inputs
        ));
        page.evaluate("document.body.insertAdjacentHTML('beforeend', document.querySelector('form').outerHTML)");
        assert!(super::require_unambiguous_form_composer(&page).is_err());
        page.evaluate(
            "document.querySelector('form').remove(); document.body.setAttribute('role', 'dialog')",
        );
        assert!(super::require_unambiguous_form_composer(&page).is_err());
    }
}
