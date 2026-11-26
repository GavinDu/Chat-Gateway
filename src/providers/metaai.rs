use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, SiteAdapter};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;

/// An observed first-party Meta AI document HTTP 403 at browser bootstrap.
/// This is not a login-expiry signal and does not reveal page or account data.
#[derive(Debug)]
pub struct MetaAiDocumentDeniedError;

impl std::fmt::Display for MetaAiDocumentDeniedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Meta AI document denied by HTTP 403 before composer readiness")
    }
}

impl std::error::Error for MetaAiDocumentDeniedError {}

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
impl SiteAdapter for MetaAIAdapter {
    fn name(&self) -> &str {
        "metaai"
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
mod native_flow_regressions {
    use crate::providers::common;
    use obscura_core::{Browser, Page};

    async fn fixture(page: &mut Page, body: &str) {
        let html = format!("<html><body>{body}</body></html>");
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
    }

    fn script(source: &str, origin: &str, prompt: &str) -> String {
        source
            .replace("__COMPOSER_TEXT__", include_str!("metaai_composer_text.js"))
            .replace("location.origin", &serde_json::to_string(origin).unwrap())
            .replace("__PROMPT__", &serde_json::to_string(prompt).unwrap())
    }

    #[tokio::test]
    async fn visible_multiline_turn_excludes_draft_assistant_and_hidden_text() {
        let browser = Browser::builder().stealth(false).build().unwrap();
        let mut page = browser.new_page().await.unwrap();
        fixture(
            &mut page,
            r#"<main>
            <div id="turn"><div><span>line one
line two</span><span hidden>hidden secret</span></div></div>
            <div class="prose">line one
line two</div>
            <nav><div>line one
line two</div></nav>
            <div style="opacity:0">line one
line two</div>
            <div contenteditable="true" role="textbox" data-lexical-editor="true"></div>
        </main>"#,
        )
        .await;
        let read = script(
            include_str!("metaai_user_turn.js"),
            "https://www.meta.ai",
            "line one\nline two",
        );
        let value = page.evaluate(&read);
        assert_eq!(value["count"], 1);
        assert_eq!(value["draft_empty"], true);
        let foreign = script(
            include_str!("metaai_user_turn.js"),
            "https://foreign.invalid",
            "line one\nline two",
        );
        assert_eq!(page.evaluate(&foreign)["valid"], false);
        page.evaluate(
            "document.querySelector('[contenteditable]').textContent='line one\\nline two'",
        );
        assert_eq!(page.evaluate(&read)["count"], 1);
        assert_eq!(page.evaluate(&read)["draft_empty"], false);
        page.evaluate("document.querySelector('#turn').hidden=true");
        assert_eq!(page.evaluate(&read)["count"], 0);
    }

    #[tokio::test]
    async fn focused_keyboard_send_is_single_and_fails_closed() {
        let browser = Browser::builder().stealth(false).build().unwrap();
        let mut page = browser.new_page().await.unwrap();
        fixture(&mut page, r#"<main><section style="pointer-events:none">
            <div id="editor" contenteditable="true" role="textbox" data-lexical-editor="true" style="pointer-events:auto">fixture prompt</div>
            <button aria-label="Send">Send</button></section></main>"#).await;
        page.evaluate("(() => {globalThis.calls=0;const editor=document.querySelector('#editor');editor.focus();editor.addEventListener('keydown',event=>{if(event.key==='Enter'){globalThis.calls++;event.preventDefault();}});return true;})()");
        let send = script(
            include_str!("metaai_keyboard_send.js"),
            "https://www.meta.ai",
            "fixture prompt",
        );
        assert_eq!(page.evaluate(&send), "dispatched", "{}", page.evaluate("JSON.stringify({focused:document.activeElement===document.querySelector('#editor'),rect:document.querySelector('#editor').getClientRects().length,styles:Array.from(document.querySelectorAll('html,body,main,section,#editor')).map(n=>({tag:n.tagName,opacity:getComputedStyle(n).opacity,visibility:getComputedStyle(n).visibility}))})"));
        assert_eq!(page.evaluate("globalThis.calls").as_f64(), Some(1.0));
        page.evaluate("document.querySelector('button').setAttribute('disabled','')");
        assert_eq!(page.evaluate(&send), "send_disabled");
        page.evaluate("(() => {document.querySelector('button').removeAttribute('disabled');document.querySelector('#editor').textContent='changed';return true;})()");
        assert_eq!(page.evaluate(&send), "draft_changed");
        page.evaluate("(() => {document.querySelector('#editor').textContent='fixture prompt';document.body.insertAdjacentHTML('beforeend','<div aria-modal=\"true\">Modal</div>');return true;})()");
        assert_eq!(page.evaluate(&send), "modal");
        assert_eq!(page.evaluate("globalThis.calls").as_f64(), Some(1.0));
        let foreign = script(
            include_str!("metaai_keyboard_send.js"),
            "https://foreign.invalid",
            "fixture prompt",
        );
        assert_eq!(page.evaluate(&foreign), "off_origin");
    }

    #[tokio::test]
    async fn paste_focuses_unfocused_composer_and_rechecks_focus_mutations() {
        let browser = Browser::builder().stealth(false).build().unwrap();
        let mut page = browser.new_page().await.unwrap();
        let body = r#"<main><section>
            <div id="editor" contenteditable="true" role="textbox" data-lexical-editor="true"></div>
            <button aria-label="Send">Send</button></section></main>"#;
        let paste = script(
            include_str!("metaai_paste.js"),
            "https://www.meta.ai",
            "line one\n你好 👋 \"quoted\"",
        )
        .replace("__SELECTOR__", "'#editor'");
        fixture(&mut page, body).await;
        page.evaluate(
            r#"(() => {
            globalThis.pastes = 0;
            const editor = document.querySelector('#editor');
            editor.addEventListener('paste', event => {
                globalThis.pastes++;
                editor.textContent = event.clipboardData.getData('text/plain');
                event.preventDefault();
            });
            return true;
        })()"#,
        );
        assert_ne!(page.evaluate("document.activeElement.id"), "editor");
        assert_eq!(
            page.evaluate(&paste),
            true,
            "an unfocused actionable editor must accept paste"
        );
        assert_eq!(page.evaluate("globalThis.pastes").as_f64(), Some(1.0));
        assert_eq!(
            page.evaluate("document.querySelector('#editor').textContent"),
            "line one\n你好 👋 \"quoted\""
        );

        // Focus handlers can replace, disable or obscure the selected editor.
        // Never dispatch to a stale node or a newly introduced editor.
        for mutation in [
            "editor.setAttribute('aria-disabled','true')",
            "editor.removeAttribute('data-lexical-editor')",
            "editor.replaceWith(editor.cloneNode(true))",
            "document.body.insertAdjacentHTML('beforeend','<div aria-modal=\"true\">Modal</div>')",
            "editor.blur()",
            "editor.insertAdjacentHTML('afterend','<div id=\"editor\" contenteditable=\"true\" role=\"textbox\" data-lexical-editor=\"true\"></div>')",
        ] {
            fixture(&mut page, body).await;
            let setup = format!(r#"(() => {{
                globalThis.pastes = 0;
                const editor = document.querySelector('#editor');
                editor.addEventListener('paste', event => {{ globalThis.pastes++; event.preventDefault(); }});
                editor.addEventListener('focus', () => {{ {mutation}; }});
                return true;
            }})()"#);
            page.evaluate(&setup);
            assert_eq!(page.evaluate(&paste), false, "focus mutation must reject paste");
            assert_eq!(page.evaluate("globalThis.pastes").as_f64(), Some(0.0));
        }
    }

    #[tokio::test]
    async fn focused_editor_helper_can_be_embedded_with_caller_named_el() {
        let browser = Browser::builder().stealth(false).build().unwrap();
        let mut page = browser.new_page().await.unwrap();
        fixture(&mut page, r#"<section style="pointer-events:none"><div id="editor" contenteditable="true" role="textbox" data-lexical-editor="true" style="pointer-events:auto">draft</div></section>"#).await;
        page.evaluate("document.querySelector('#editor').focus()");
        let helper = include_str!("metaai_composer_pointer.js")
            .replace("location.origin", "'https://www.meta.ai'")
            .replace("__ELEMENT__", "el");
        let probe =
            format!("(() => {{ const el=document.querySelector('#editor'); return {helper}; }})()");
        assert_eq!(page.evaluate(&probe), true);
        page.evaluate("document.querySelector('#editor').setAttribute('aria-readonly','true')");
        assert_eq!(page.evaluate(&probe), false);
    }
    #[tokio::test]
    async fn composer_preserves_opencode_formatting_and_rejects_hidden_or_changed_text() {
        let browser = Browser::builder().stealth(false).build().unwrap();
        let mut page = browser.new_page().await.unwrap();
        fixture(&mut page, r#"<main><section>
            <div id="editor" contenteditable="true" role="textbox" data-lexical-editor="true" style="white-space:pre-wrap"><span></span></div>
            <button aria-label="Send">Send</button>
        </section></main>"#).await;
        let expected = "line one\n    line two\n\tline three 👋";
        let value = serde_json::to_string(expected).unwrap();
        page.evaluate(&format!(
            "document.querySelector('#editor span').textContent={value}"
        ));
        assert!(
            !common::composer_retains_prompt(&page, "#editor", expected).unwrap(),
            "fixture must reproduce rendered whitespace collapse"
        );
        assert!(common::metaai_composer_retains_prompt(&page, "#editor", expected).unwrap());
        assert!(!common::metaai_composer_retains_prompt(
            &page,
            "#editor",
            "line one line two line three 👋"
        )
        .unwrap());
        page.evaluate(r#"(() => {const editor=document.querySelector('#editor');editor.focus();editor.addEventListener('keydown',event=>{if(event.key==='Enter')event.preventDefault();});return true;})()"#);
        let send = script(
            include_str!("metaai_keyboard_send.js"),
            "https://www.meta.ai",
            expected,
        );
        assert_eq!(page.evaluate(&send), "dispatched");
        page.evaluate("document.querySelector('#editor span').style.display='none'");
        assert!(!common::metaai_composer_retains_prompt(&page, "#editor", expected).unwrap());
        assert_eq!(page.evaluate(&send), "draft_changed");
        page.evaluate("document.querySelector('#editor').innerHTML='<span>line one</span><br><span>    line two</span><br><span>\\tline three 👋</span>'");
        assert!(common::metaai_composer_retains_prompt(&page, "#editor", expected).unwrap());
        page.evaluate(
            "document.querySelector('#editor').innerHTML='<p>line one</p><p>line two</p>'",
        );
        assert!(
            !common::metaai_composer_retains_prompt(&page, "#editor", "line oneline two").unwrap()
        );
    }
}
