use obscura_core::Page;
use std::time::Duration;

/// The distinct browser timeout phases are preserved as typed errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTimeoutPhase {
    Submission,
    FirstContent,
    Generation,
}

impl ProviderTimeoutPhase {
    /// Only confirmed new assistant text advances from first-content to
    /// generation. A clicked Send, cleared composer or unchanged old answer
    /// alone cannot prove the current model began responding.
    pub fn after_submission(new_content_observed: bool) -> Self {
        if new_content_observed {
            Self::Generation
        } else {
            Self::FirstContent
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Submission => "submission",
            Self::FirstContent => "first_content",
            Self::Generation => "generation",
        }
    }
}

#[cfg(test)]
mod provider_timeout_phase_tests {
    use super::ProviderTimeoutPhase;

    #[test]
    fn typed_timeout_distinguishes_no_first_content_from_partial_generation() {
        assert_eq!(ProviderTimeoutPhase::Submission.as_str(), "submission");
        let no_content = ProviderTimeoutPhase::after_submission(false);
        assert_eq!(no_content, ProviderTimeoutPhase::FirstContent);
        assert_eq!(no_content.as_str(), "first_content");
        let partial = ProviderTimeoutPhase::after_submission(true);
        assert_eq!(partial, ProviderTimeoutPhase::Generation);
        assert_eq!(partial.as_str(), "generation");
    }
}

#[derive(Debug)]
pub struct ProviderTimeoutError {
    provider: String,
    timeout: Duration,
    phase: ProviderTimeoutPhase,
}

impl ProviderTimeoutError {
    pub fn new(provider: &str, timeout: Duration, phase: ProviderTimeoutPhase) -> Self {
        Self {
            provider: provider.to_owned(),
            timeout,
            phase,
        }
    }
}

impl std::fmt::Display for ProviderTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.phase {
            ProviderTimeoutPhase::Submission => write!(
                f,
                "provider '{}' timed out while submitting the prompt after {:?}",
                self.provider, self.timeout
            ),
            ProviderTimeoutPhase::FirstContent => write!(
                f,
                "provider '{}' did not return new assistant content within {:?}",
                self.provider, self.timeout
            ),
            ProviderTimeoutPhase::Generation => write!(
                f,
                "provider '{}' did not complete within {:?}",
                self.provider, self.timeout
            ),
        }
    }
}

impl std::error::Error for ProviderTimeoutError {}

/// A framework-controlled editor discarded a draft after input dispatch.
/// Do not interpret synthetic input events as successful typing. This typed
/// error stays free of the prompt, cookies, page content, or browser URL.
#[derive(Debug)]
pub struct ProviderComposerInputRejectedError;

impl std::fmt::Display for ProviderComposerInputRejectedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("provider input did not retain the prompt after framework render")
    }
}

impl std::error::Error for ProviderComposerInputRejectedError {}

/// A syntactically successful Send click is not proof the provider accepted
/// this prompt. This is a distinct failure from a submission *timeout* or a
/// no-first-content timeout after acknowledged submission. The error carries
/// no prompt, page URL, cookies, DOM text, account identifier or model output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderSubmissionUnacknowledgedError;

impl ProviderSubmissionUnacknowledgedError {
    pub fn phase(self) -> ProviderTimeoutPhase {
        ProviderTimeoutPhase::Submission
    }
}

impl std::fmt::Display for ProviderSubmissionUnacknowledgedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("provider did not acknowledge prompt submission with a current-turn signal")
    }
}

impl std::error::Error for ProviderSubmissionUnacknowledgedError {}

#[cfg(test)]
mod provider_submission_unacknowledged_error_tests {
    use super::{ProviderSubmissionUnacknowledgedError, ProviderTimeoutError};

    #[test]
    fn redacted_typed_error_is_distinct_from_timeout_and_does_not_echo_prompt() {
        let error = anyhow::Error::new(ProviderSubmissionUnacknowledgedError);
        let typed = error
            .downcast_ref::<ProviderSubmissionUnacknowledgedError>()
            .expect("unacknowledged submission must remain downcastable");
        assert_eq!(typed.phase().as_str(), "submission");
        assert!(error.downcast_ref::<ProviderTimeoutError>().is_none());
        assert_eq!(
            error.to_string(),
            "provider did not acknowledge prompt submission with a current-turn signal"
        );
        assert!(!error.to_string().contains("secret fixture prompt"));
    }
}

/// Static per-provider selector/URL data. Each adapter is a SelectorSet
/// plus the shared interaction logic in this module (ported from v1's
/// per-adapter discover/send bodies, re-expressed in the real obscura
/// API: query_selector instead of find_element, sync evaluate returning
/// serde_json::Value, Element::click synchronous, no type_str).
#[derive(Debug, Clone, Copy)]
pub struct SelectorSet {
    pub url: &'static str,
    pub picker: &'static [&'static str],
    pub input: &'static [&'static str],
    pub send: &'static [&'static str],
    pub response: &'static [&'static str],
    /// JS regex alternation used to filter discovered model labels.
    pub model_keywords: &'static str,
    /// Model-picker trigger→option pairs (Go's modelSelectors). Taken
    /// from Go's pairs for the 5 Go-covered providers; empty for
    /// grok/mistral/perplexity, which have no pair data anywhere —
    /// selection then no-ops gracefully (see select_model).
    pub model_pairs: &'static [(&'static str, &'static str)],
}

/// Effective selector data owned by each adapter. Keeping the same shared
/// send/response implementation avoids drift between configured and built-in paths.
#[derive(Debug, Clone)]
pub struct EffectiveSelectorSet {
    pub picker: Vec<String>,
    pub input: Vec<String>,
    pub send: Vec<String>,
    pub response: Vec<String>,
}

impl EffectiveSelectorSet {
    pub fn new(base: &SelectorSet, overrides: &crate::config::ProviderSelectorOverrides) -> Self {
        let collect = |original: &[&str], configured: &Option<Vec<String>>| {
            configured
                .clone()
                .unwrap_or_else(|| original.iter().map(|s| (*s).to_owned()).collect())
        };
        Self {
            picker: collect(base.picker, &overrides.picker),
            input: collect(base.input, &overrides.input),
            send: collect(base.send, &overrides.send),
            response: collect(base.response, &overrides.response),
        }
    }

    pub fn input_refs(&self) -> Vec<&str> {
        self.input.iter().map(String::as_str).collect()
    }
    pub fn send_refs(&self) -> Vec<&str> {
        self.send.iter().map(String::as_str).collect()
    }
    pub fn response_refs(&self) -> Vec<&str> {
        self.response.iter().map(String::as_str).collect()
    }
    pub fn picker_refs(&self) -> Vec<&str> {
        self.picker.iter().map(String::as_str).collect()
    }
    pub fn adapter_selectors(&self) -> crate::providers::AdapterSelectors {
        crate::providers::AdapterSelectors {
            input_selector: self.input.first().cloned().unwrap_or_default(),
            send_button: self.send.first().cloned().unwrap_or_default(),
            response_container: self.response.first().cloned().unwrap_or_default(),
            model_picker: self.picker.first().cloned().unwrap_or_default(),
        }
    }
}

impl SelectorSet {
    pub fn effective(
        &self,
        overrides: &crate::config::ProviderSelectorOverrides,
    ) -> EffectiveSelectorSet {
        EffectiveSelectorSet::new(self, overrides)
    }
}

/// Streaming indicator shared by all providers. Keep this valid CSS only;
/// text matching is handled by the explicit text-selector helper below.
pub const STREAMING_SELECTOR: &str = r#"[data-is-streaming="true"]"#;

/// Snapshot of the three DOM signals used by the response polling loop.
/// A single page evaluation keeps the signals from drifting across ticks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseProbe {
    pub text: Option<String>,
    pub count: usize,
    pub streaming: bool,
    pub post_answer_affordance: bool,
    pub url: String,
    pub challenge: bool,
}

fn decode_response_probe(value: serde_json::Value) -> ResponseProbe {
    let url = value
        .get("url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let url_challenge = crate::provider_guard::is_challenge_url(&url);
    ResponseProbe {
        text: value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
        // Obscura may deserialize a whole-number JS node count as 1.0.
        // Reuse the submission-count decoder so a new response isn't
        // mistaken for zero during generation polling.
        count: value
            .get("count")
            .and_then(decode_dom_count)
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(0),
        streaming: value
            .get("streaming")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        post_answer_affordance: value
            .get("postAnswerAffordance")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        url,
        challenge: url_challenge
            || value
                .get("challenge")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
    }
}

/// Read response text, response-node count, and streaming state in one
/// JavaScript evaluation per polling tick.
pub fn response_probe(
    page: &Page,
    selectors: &[&str],
    check_body_challenge: bool,
) -> ResponseProbe {
    let Ok(selectors) = serde_json::to_string(selectors) else {
        return ResponseProbe::default();
    };
    let Ok(streaming_selector) = serde_json::to_string(STREAMING_SELECTOR) else {
        return ResponseProbe::default();
    };
    let script = format!(
        r#"(() => {{
            const selectors = {selectors};
            const visible = el => {{
                try {{
                    return el.getClientRects().length > 0 &&
                        window.getComputedStyle(el).visibility !== "hidden" &&
                        !el.closest('[hidden], [inert], [aria-hidden="true"]') &&
                        !el.disabled && !el.matches(':disabled') &&
                        !el.closest('fieldset[disabled], [aria-disabled="true"]');
                }} catch (_) {{ return false; }}
            }};
            const nodes = new Set();
            let text = null;
            let lastResponseNode = null;
            for (const selector of selectors) {{
                try {{
                    let matches = [];
                    if (selector.startsWith("text:")) {{
                        const parts = selector.slice(5).split("=");
                        if (parts.length >= 2 && parts[0]) {{
                            const tag = parts.shift();
                            const needle = parts.join("=");
                            matches = Array.from(document.querySelectorAll(tag)).filter(el =>
                                ((el.innerText || el.textContent || "").trim()).includes(needle)
                            );
                        }}
                    }} else {{
                        matches = Array.from(document.querySelectorAll(selector));
                    }}
                    matches = matches.filter(visible);
                    matches.forEach(node => nodes.add(node));
                    if (text === null && matches.length) {{
                        // Frameworks can append a visible but empty assistant
                        // placeholder after a completed reply. Select the last
                        // non-whitespace response instead of treating that
                        // placeholder as the reply or hiding the earlier text.
                        for (let i = matches.length - 1; i >= 0; i -= 1) {{
                            const node = matches[i];
                            const content = node.innerText || node.textContent || "";
                            if (content.trim()) {{
                                text = content;
                                lastResponseNode = node;
                                break;
                            }}
                        }}
                    }}
                }} catch (_) {{}}
            }}
            // Hidden, disabled or unrelated Stop controls are not proof
            // of active streaming, just as hidden response nodes are not
            // evidence of a new provider answer.
            let streaming = false;
            try {{
                streaming = Array.from(document.querySelectorAll({streaming_selector})).some(visible);
            }} catch (_) {{}}
            if (!streaming) {{
                streaming = Array.from(document.querySelectorAll("button"))
                    .some(button => visible(button) &&
                        /^(stop|stop generating|stop generation|stop responding|stop response|stop answering)$/i.test(
                            (button.getAttribute("aria-label") || button.innerText || button.textContent || "").trim()
                        ));
            }}
            const url = String(location.href || "");
            const body = ({check_body_challenge} ? {challenge_text_js} : "").toLowerCase();
            // URL challenge detection runs in Rust using the shared host/path
            // classifier. Query values and chat text must not trip the guard.
            const challenge = ["verify you are human", "confirm you are not a robot", "unusual traffic", "complete the captcha", "security challenge", "checking your browser"].some(marker => body.includes(marker));
            let postAnswerAffordance = false;
            if (lastResponseNode) {{
                const scopeSelector = "article, [data-message-author-role='assistant'], [data-testid='assistant-message'], [data-testid*='conversation-turn'], [data-testid*='message']";
                // A Copy/Retry control is terminal evidence ONLY when scoped to
                // the same visible response turn. Climbing past an <article>
                // can discover an old reply's Copy button in the conversation
                // root and falsely complete the current partial answer.
                let scope = null;
                try {{ scope = lastResponseNode.closest(scopeSelector); }} catch (_) {{}}
                if (scope) {{
                    let buttons = [];
                    try {{ buttons = Array.from(scope.querySelectorAll("button, [role='button']")); }} catch (_) {{}}
                    postAnswerAffordance = buttons.some(button => visible(button) &&
                        /copy|regenerate|retry|good response|bad response|thumbs up|thumbs down/.test(
                            ((button.getAttribute("aria-label") || "") + " " +
                            (button.getAttribute("data-testid") || "") + " " +
                            (button.innerText || button.textContent || "")).toLowerCase()
                        ));
                }}
                // An unrecognized turn root cannot safely borrow controls
                // from an ancestor. Retain the conservative idle fallback.
            }}
            return {{ text, count: nodes.size, streaming, postAnswerAffordance, url, challenge }};
        }})()"#,
        check_body_challenge = check_body_challenge,
        challenge_text_js = crate::provider_guard::NON_MESSAGE_CHALLENGE_TEXT_JS,
    );
    decode_response_probe(page.evaluate(&script))
}

#[cfg(test)]
mod chat_text_challenge_regression_tests {
    #[tokio::test]
    async fn assistant_quoting_challenge_phrases_does_not_trip_response_poller() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <div data-message-author-role="assistant"><div class="answer">To verify you are human, follow the on-screen instructions</div></div>
            <textarea id="composer"></textarea>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let normal = super::response_probe(&page, &[".answer"], true);
        assert!(
            !normal.challenge,
            "response text must not be a challenge page"
        );
        assert_eq!(
            normal.text.as_deref(),
            Some("To verify you are human, follow the on-screen instructions")
        );
        page.evaluate(
            "document.body.insertAdjacentHTML('beforeend', '<h1>Security challenge</h1>')",
        );
        let blocked = super::response_probe(&page, &[".answer"], true);
        assert!(
            blocked.challenge,
            "visible verification UI must still block"
        );
    }
}

#[cfg(test)]
mod streaming_visibility_regression_tests {
    use super::response_probe;

    #[tokio::test]
    async fn hidden_stop_controls_and_unrelated_buttons_do_not_extend_generation() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><div class="answer">done</div><button id="stop" aria-label="Stop generating" style="display:none">Stop</button><span data-is-streaming="true" hidden></span><button aria-label="Stop sharing">Stop sharing</button></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let idle = response_probe(&page, &[".answer"], false);
        assert!(!idle.streaming, "hidden controls are not active generation");

        page.evaluate("document.querySelector('#stop').style.display = 'block'");
        let active = response_probe(&page, &[".answer"], false);
        assert!(
            active.streaming,
            "a visible Stop generating button is active"
        );
    }
    #[tokio::test]
    async fn disabled_stop_and_hidden_copy_never_signal_progress_or_completion() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <article><div class="answer">done</div><button id="copy" aria-label="Copy" hidden>Copy</button></article>
            <fieldset disabled><button id="stop" aria-label="Stop generating">Stop</button></fieldset>
            <div aria-disabled="true"><span data-is-streaming="true">stale indicator</span></div>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let idle = response_probe(&page, &[".answer"], false);
        assert!(
            !idle.streaming,
            "disabled Stop controls must not indicate streaming"
        );
        assert!(
            !idle.post_answer_affordance,
            "a hidden Copy control cannot confirm completion"
        );
        page.evaluate("document.querySelector('#copy').hidden = false");
        let finished = response_probe(&page, &[".answer"], false);
        assert!(finished.post_answer_affordance);
        page.evaluate("document.querySelector('fieldset').disabled = false");
        let active = response_probe(&page, &[".answer"], false);
        assert!(active.streaming);
    }

    #[tokio::test]
    async fn copy_from_previous_turn_cannot_complete_latest_partial_response() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <section id="conversation">
              <article id="old"><div class="answer">completed earlier reply</div>
                <button id="old-copy" aria-label="Copy">Copy</button></article>
              <article id="current"><div><div class="answer">new partial reply</div></div>
                <button id="current-copy" aria-label="Copy" hidden>Copy</button></article>
            </section>
        </body></html>"#;
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        let before = response_probe(&page, &[".answer"], false);
        assert_eq!(before.text.as_deref(), Some("new partial reply"));
        assert!(
            !before.post_answer_affordance,
            "old article's visible Copy must not finish a new partial response"
        );
        page.evaluate("document.querySelector('#current-copy').hidden = false");
        let current = response_probe(&page, &[".answer"], false);
        assert!(
            current.post_answer_affordance,
            "same-article Copy is a scoped post-answer signal"
        );
        page.evaluate("document.querySelector('#current-copy').disabled = true");
        let disabled = response_probe(&page, &[".answer"], false);
        assert!(
            !disabled.post_answer_affordance,
            "disabled current Copy must not borrow an enabled old Copy"
        );
    }

    #[tokio::test]
    async fn missing_turn_root_fails_closed_for_post_answer_affordance() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><section>
            <div class="answer">incomplete response without known turn root</div>
            <button aria-label="Copy">Copy older message</button>
        </section></body></html>"#;
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let probe = response_probe(&page, &[".answer"], false);
        assert!(
            !probe.post_answer_affordance,
            "a generic parent container cannot prove completion"
        );
    }

    #[tokio::test]
    async fn visible_empty_trailing_response_does_not_hide_last_populated_reply() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <article><div class="answer">previous reply</div></article>
            <article><div class="answer" id="placeholder">  </div></article>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        let previous = response_probe(&page, &[".answer"], false);
        assert_eq!(previous.text.as_deref(), Some("previous reply"));
        assert_eq!(
            previous.count, 2,
            "visible placeholders still count as nodes"
        );

        page.evaluate("document.querySelector('#placeholder').textContent = 'new reply'");
        let new_reply = response_probe(&page, &[".answer"], false);
        assert_eq!(new_reply.text.as_deref(), Some("new reply"));

        page.evaluate("document.querySelectorAll('.answer')[0].textContent = '  '");
        page.evaluate("document.querySelector('#placeholder').textContent = '  '");
        let blank = response_probe(&page, &[".answer"], false);
        assert_eq!(
            blank.text, None,
            "whitespace cannot masquerade as generated text"
        );
    }

    #[tokio::test]
    async fn hidden_assistant_node_does_not_override_visible_response_or_count() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><article><div class="answer">visible answer</div><div class="answer" hidden>stale hidden answer</div></article></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        let response = response_probe(&page, &[".answer"], false);
        assert_eq!(response.text.as_deref(), Some("visible answer"));
        assert_eq!(response.count, 1);
        page.evaluate("document.querySelector('.answer').hidden = true");
        let hidden = response_probe(&page, &[".answer"], false);
        assert_eq!(hidden.text, None);
        assert_eq!(hidden.count, 0);
    }
}

/// Parse the internal `text:TAG=NEEDLE` selector syntax. This is deliberately
/// not CSS: Obscura's selector engine must never receive Playwright-only pseudos.
fn text_selector_parts(selector: &str) -> Option<(&str, &str)> {
    let rest = selector.strip_prefix("text:")?;
    let (tag, needle) = rest.split_once('=')?;
    if tag.is_empty() || needle.is_empty() {
        return None;
    }
    Some((tag, needle))
}

#[cfg(test)]
fn text_selector_matches(page: &Page, selector: &str) -> bool {
    let Some((tag, needle)) = text_selector_parts(selector) else {
        return false;
    };
    let Ok(tag) = serde_json::to_string(tag) else {
        return false;
    };
    let Ok(needle) = serde_json::to_string(needle) else {
        return false;
    };
    page.evaluate(&format!(
        "Array.from(document.querySelectorAll({tag})).some(el => ((el.innerText || el.textContent || '').trim()).includes({needle}))"
    ))
    .as_bool() == Some(true)
}

fn click_text_selector(page: &Page, selector: &str) -> anyhow::Result<bool> {
    let Some((tag, needle)) = text_selector_parts(selector) else {
        return Ok(false);
    };
    let tag = serde_json::to_string(tag)?;
    let needle = serde_json::to_string(needle)?;
    let clicked = page.evaluate(&format!(
        "(() => {{ const el = Array.from(document.querySelectorAll({tag})).find(el => {{ try {{ return ((el.innerText || el.textContent || '').trim()).includes({needle}) && el.getClientRects().length > 0 && getComputedStyle(el).visibility !== 'hidden' && !el.disabled && !el.matches(':disabled') && !el.closest('[hidden], [inert], [aria-hidden=\"true\"], [aria-disabled=\"true\"], fieldset[disabled]'); }} catch (_) {{ return false; }} }}); if (!el) return false; el.click(); return true; }})()"
    ));
    Ok(clicked.as_bool() == Some(true))
}

#[cfg(test)]
pub fn element_exists(page: &Page, selector: &str) -> bool {
    if text_selector_parts(selector).is_some() {
        text_selector_matches(page, selector)
    } else {
        page.query_selector(selector).is_some()
    }
}

/// A login input, checkbox or upload control is not a chat composer even
/// when a user-supplied selector happens to match it. Only native text/search
/// inputs and textareas, or elements effectively contenteditable, can accept
/// a chat prompt. Follow the closest explicit contenteditable value: a child can override an
/// ancestor's false value, while an unannotated SPA placeholder is never an
/// editor. Use DOM attributes rather than relying on engine-specific
/// isContentEditable support in the pinned embedded browser.
fn editable_composer_condition(element: &str) -> String {
    format!(
        "({element}.tagName === 'TEXTAREA' || ({element}.tagName === 'INPUT' && ({element}.type === 'text' || {element}.type === 'search')) || ({element}.tagName !== 'INPUT' && (() => {{ let node = {element}; while (node) {{ const value = node.getAttribute('contenteditable'); if (value !== null && value !== 'inherit') return value === '' || value === 'true' || value === 'plaintext-only'; node = node.parentElement; }} return false; }})()))"
    )
}

/// A DOM match alone is not a usable chat composer. Login/status probes
/// must ignore hidden, disabled and read-only inputs left in SPA shells.
/// This reports UI readiness only, never proof of account authentication.
pub fn visible_composer_exists(page: &Page, selectors: &[&str]) -> bool {
    let Ok(selectors) = serde_json::to_string(selectors) else {
        return false;
    };
    let editable = editable_composer_condition("input");
    let script = format!(
        r#"(() => {{
            for (const selector of {selectors}) {{
                try {{
                    for (const input of document.querySelectorAll(selector)) {{
                        if ({editable} &&
                            input.getClientRects().length > 0 &&
                            window.getComputedStyle(input).visibility !== "hidden" &&
                            !input.closest('[hidden], [inert], [aria-hidden="true"], [aria-disabled="true"], [aria-readonly="true"]') &&
                            !input.disabled && !input.matches(':disabled') &&
                            !input.closest('fieldset[disabled]') && !input.readOnly &&
                            input.getAttribute("aria-disabled") !== "true" &&
                            input.getAttribute("aria-readonly") !== "true" &&
                            input.getAttribute("contenteditable") !== "false") return true;
                    }}
                }} catch (_) {{}}
            }}
            return false;
        }})()"#
    );
    page.evaluate(&script).as_bool() == Some(true)
}

/// Choose the first actionable input from a selector that may match
/// both a hidden sizing placeholder and the actual visible composer.
fn visible_input_query(selector_js: &str) -> String {
    let editable = editable_composer_condition("el");
    format!(
        "Array.from(document.querySelectorAll({selector_js})).find(el => {editable} && el.getClientRects().length > 0 && window.getComputedStyle(el).visibility !== 'hidden' && !el.closest('[hidden], [inert], [aria-hidden=\"true\"], [aria-disabled=\"true\"], [aria-readonly=\"true\"]') && !el.disabled && !el.matches(':disabled') && !el.closest('fieldset[disabled]') && !el.readOnly && el.getAttribute('aria-disabled') !== 'true' && el.getAttribute('aria-readonly') !== 'true' && el.getAttribute('contenteditable') !== 'false')"
    )
}

/// Fail closed when multiple search, thread, or dialog editors are actionable.
/// A single editor matching multiple configured selectors counts only once.
fn actionable_composer_count_expression(selectors: &[&str]) -> anyhow::Result<String> {
    let selectors = serde_json::to_string(selectors)?;
    let editable = editable_composer_condition("el");
    Ok(format!(
        r#"(() => {{
            const matches = new Set();
            for (const selector of {selectors}) {{
                try {{
                    for (const el of document.querySelectorAll(selector)) {{
                        if ({editable} &&
                            el.getClientRects().length > 0 &&
                            window.getComputedStyle(el).visibility !== 'hidden' &&
                            !el.closest('[hidden], [inert], [aria-hidden="true"], [aria-disabled="true"], [aria-readonly="true"]') &&
                            !el.disabled && !el.matches(':disabled') &&
                            !el.closest('fieldset[disabled]') &&
                            !el.readOnly && el.getAttribute('aria-disabled') !== 'true' &&
                            el.getAttribute('aria-readonly') !== 'true' &&
                            el.getAttribute('contenteditable') !== 'false') {{
                            matches.add(el);
                        }}
                    }}
                }} catch (_) {{}}
            }}
            return matches.size;
        }})()"#
    ))
}

fn require_unique_actionable_composer(page: &Page, selectors: &[&str]) -> anyhow::Result<()> {
    let expr = actionable_composer_count_expression(selectors)?;
    if page.evaluate(&expr).as_f64() == Some(1.0) {
        Ok(())
    } else {
        anyhow::bail!("provider actionable composer ambiguous or unavailable")
    }
}

/// The pinned Obscura engine may attach a synthetic `value` property to a
/// contenteditable node without mutating the visible DOM or its framework
/// model. Only native input/textarea controls may prove draft retention by
/// value. All other actionable contenteditables must prove *visible* text.
/// This expression is reused at typing, Send, and submission snapshot time.
fn composer_draft_expression(element: &str) -> String {
    format!(
        "({element}.tagName === 'TEXTAREA' || {element}.tagName === 'INPUT' ? {element}.value : ({element}.innerText ?? {element}.textContent ?? ''))"
    )
}

/// Re-read the actionable composer after framework renders, not just after
/// the initial typing event. Never log the prompt when the value changes.
fn composer_retains_prompt(page: &Page, selector: &str, prompt: &str) -> anyhow::Result<bool> {
    let selector_js = serde_json::to_string(selector)?;
    let el_js = visible_input_query(&selector_js);
    let prompt_js = serde_json::to_string(prompt)?;
    let draft = composer_draft_expression("el");
    Ok(page
        .evaluate(&format!(
            "(() => {{ const el = {el_js}; if (!el) return false; const value = {draft}; return value === {prompt_js}; }})()"
        ))
        .as_bool()
        == Some(true))
}

#[cfg(test)]
mod visible_composer_tests {
    use super::{click_send_or_enter, visible_composer_exists};

    #[tokio::test]
    async fn perplexity_ambiguous_visible_editors_fail_closed_and_overrides_deduplicate() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <textarea id="thread" class="candidate"></textarea>
            <textarea id="search" class="candidate"></textarea>
            <textarea id="hidden" class="candidate" hidden></textarea>
            <textarea id="readonly" class="candidate" readonly></textarea>
            <input type="email" class="candidate">
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let inputs = &["textarea.candidate", "#thread", ".candidate"];
        assert!(super::require_unique_actionable_composer(&page, inputs).is_err());
        page.evaluate("document.querySelector('#search').setAttribute('hidden', '')");
        assert!(super::require_unique_actionable_composer(&page, inputs).is_ok());
        page.evaluate("document.querySelector('#thread').setAttribute('disabled', '')");
        assert!(super::require_unique_actionable_composer(&page, inputs).is_err());
        page.evaluate("document.querySelector('#thread').removeAttribute('disabled')");
        page.evaluate("document.querySelector('#search').removeAttribute('hidden')");
        assert!(super::require_unique_actionable_composer(&page, &["#thread"]).is_ok());
        assert!(super::require_unique_actionable_composer(&page, &["#search", "#thread"]).is_err());
    }

    #[tokio::test]
    async fn hidden_disabled_and_readonly_composers_are_not_reported_ready() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><textarea id="hidden" style="display:none"></textarea><textarea id="disabled" disabled></textarea><textarea id="readonly" readonly></textarea><fieldset disabled><textarea id="fieldset-disabled"></textarea></fieldset><div id="noneditable" contenteditable="false" role="textbox">stale</div><div id="aria-readonly" contenteditable="true" aria-readonly="true">stale</div><textarea id="visible"></textarea></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        assert!(!visible_composer_exists(
            &page,
            &[
                "#hidden",
                "#disabled",
                "#readonly",
                "#fieldset-disabled",
                "#noneditable",
                "#aria-readonly"
            ]
        ));
        assert!(visible_composer_exists(&page, &["#hidden", "#visible"]));
        page.evaluate("document.querySelector('#visible').hidden = true");
        assert!(!visible_composer_exists(&page, &["#visible"]));
    }
    #[tokio::test]
    async fn login_and_nontext_inputs_cannot_masquerade_as_chat_composer() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <input id="email" type="email" value="account@example.test">
            <input id="password" type="password" value="credential">
            <input id="checkbox" type="checkbox" value="untouched">
            <input id="search" type="search">
            <input id="text" type="text">
            <textarea id="textarea"></textarea>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        assert!(!visible_composer_exists(
            &page,
            &["#email", "#password", "#checkbox"]
        ));
        assert!(visible_composer_exists(
            &page,
            &["#search", "#text", "#textarea"]
        ));
        super::human_type(
            &page,
            &["#email, #password, #checkbox, #text"],
            "safe draft",
        )
        .await
        .unwrap();
        assert_eq!(
            page.evaluate("document.querySelector('#text').value")
                .as_str(),
            Some("safe draft")
        );
        assert_eq!(
            page.evaluate("document.querySelector('#password').value")
                .as_str(),
            Some("credential")
        );
        assert_eq!(
            page.evaluate("document.querySelector('#checkbox').value")
                .as_str(),
            Some("untouched")
        );
    }

    #[tokio::test]
    async fn noneditable_dom_placeholders_cannot_win_composer_selection() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <div id="plain" role="textbox">Looks like an input</div>
            <div contenteditable="false"><div id="blocked" class="editor">Locked</div></div>
            <div aria-readonly="true"><div id="readonly-child" contenteditable="true">Read only</div></div>
            <div contenteditable="true"><div id="editable" class="editor">Can edit</div></div>
            <div contenteditable="false"><div id="override" contenteditable="true">Explicit opt-in</div></div>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        assert!(!visible_composer_exists(
            &page,
            &["#plain", "#blocked", "#readonly-child"]
        ));
        assert!(visible_composer_exists(&page, &["#editable", "#override"]));

        // The actual typing and retention helpers share visible_input_query.
        // They must skip the first fake match instead of targeting its content.
        let query = super::visible_input_query(r#"'#blocked, #editable' "#.trim());
        let selected = page.evaluate(&format!(
            "(() => {{ const el = {query}; return el ? el.id : ''; }})()"
        ));
        assert_eq!(selected.as_str(), Some("editable"));
    }

    #[tokio::test]
    async fn inherited_aria_disabled_composer_cannot_dispatch_send_or_enter() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <div id="wrapper" aria-disabled="true"><form>
              <textarea id="prompt">draft</textarea>
              <button id="send" type="button" onclick="window.sent=(window.sent||0)+1">Send</button>
            </form></div>
            <script>
              window.enters=0;
              document.querySelector('#prompt').addEventListener('keydown',()=>window.enters++);
            </script>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        assert!(!visible_composer_exists(&page, &["#prompt"]));
        assert!(
            click_send_or_enter(&mut page, &["#send"], "#prompt", 250)
                .await
                .is_err(),
            "an aria-disabled ancestor must refuse send even via Enter fallback"
        );
        assert_eq!(page.evaluate("window.sent || 0").as_f64(), Some(0.0));
        assert_eq!(page.evaluate("window.enters || 0").as_f64(), Some(0.0));

        page.evaluate("document.querySelector('#wrapper').removeAttribute('aria-disabled')");
        assert!(visible_composer_exists(&page, &["#prompt"]));
        assert!(click_send_or_enter(&mut page, &["#send"], "#prompt", 250)
            .await
            .unwrap());
        assert_eq!(page.evaluate("window.sent || 0").as_f64(), Some(1.0));
    }
}

#[cfg(test)]
mod response_probe_tests {
    use super::{decode_response_probe, ResponseProbe};

    #[test]
    fn challenge_probe_does_not_disable_provider_for_query_text() {
        let query = serde_json::json!({
            "url": "https://gemini.google.com/app?prompt=security%20challenge",
            "challenge": false
        });
        assert!(!decode_response_probe(query).challenge);
        let challenge = serde_json::json!({
            "url": "https://chatgpt.com/cdn-cgi/challenge-platform/h/g/",
            "challenge": false
        });
        assert!(decode_response_probe(challenge).challenge);
        let body = serde_json::json!({
            "url": "https://chatgpt.com/",
            "challenge": true
        });
        assert!(decode_response_probe(body).challenge);
    }

    #[test]
    fn decodes_response_probe_fields_and_handles_malformed_values() {
        assert_eq!(
            decode_response_probe(serde_json::json!({
                "text": "answer",
                "count": 4,
                "streaming": true,
                "postAnswerAffordance": true,
                "url": "https://provider.example/challenge",
                "challenge": true
            })),
            ResponseProbe {
                text: Some("answer".to_string()),
                count: 4,
                streaming: true,
                post_answer_affordance: true,
                url: "https://provider.example/challenge".to_owned(),
                challenge: true,
            }
        );
        assert_eq!(
            decode_response_probe(serde_json::json!({"text": "", "count": -1})),
            ResponseProbe::default()
        );
        // Obscura's JS bridge emits whole numeric DOM counts as floats.
        // Polling must agree with submission acknowledgement on those counts.
        assert_eq!(
            decode_response_probe(serde_json::json!({"count": 3.0})).count,
            3
        );
        assert_eq!(
            decode_response_probe(serde_json::json!({"count": 2.5})).count,
            0
        );
        assert_eq!(
            decode_response_probe(serde_json::json!({"count": "4"})).count,
            0
        );
    }
}

/// Advance browser tasks while preserving the configured wall-clock poll
/// interval. settle is a maximum budget and may return immediately when idle;
/// counting those fast returns as elapsed ticks would truncate responses.
pub async fn drive_page(page: &mut Page, duration: Duration) {
    let deadline = tokio::time::Instant::now() + duration;
    page.settle(duration.as_millis().max(1).min(u64::MAX as u128) as u64)
        .await;
    tokio::time::sleep_until(deadline).await;
}

/// Wait for an actionable composer, not merely a stale hidden input node.
pub async fn wait_for_selector(
    page: &mut Page,
    selectors: &[&str],
    timeout_ms: u64,
) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        if visible_composer_exists(page, selectors) {
            return Ok(());
        }
        drive_page(page, Duration::from_millis(150)).await;
    }
    anyhow::bail!("timeout waiting for actionable provider composer")
}

/// Click the first matching element (obscura Element::click is
/// synchronous, scrolls into view first, and focuses the element).
pub fn click_first(page: &Page, selectors: &[&str]) -> anyhow::Result<()> {
    for sel in selectors {
        if text_selector_parts(sel).is_some() {
            if click_text_selector(page, sel)? {
                return Ok(());
            }
        } else if let Some(el) = page.query_selector(sel) {
            el.click()?;
            return Ok(());
        }
    }
    anyhow::bail!("none of selectors matched: {:?}", selectors)
}

/// Focus the first matching input and type into it. Obscura has no
/// type_str: clicking focuses the element, then the text is inserted into
/// document.activeElement, dispatching input/change events so
/// framework-controlled inputs observe the change.
///
/// Two Go concepts, in merged form: the dispatched InputEvent carries
/// Go's `inputType: 'insertText'` + `data` fields (free, and
/// framework-observable), and plain TEXTAREA or INPUT composers use the
/// native value setter branch instead of execCommand. Everything else
/// uses editor-scoped Range selection + execCommand insertText. That dispatch
/// does NOT establish valid visible draft retention by itself.
pub async fn human_type(page: &Page, selectors: &[&str], text: &str) -> anyhow::Result<()> {
    human_type_with_provider(page, selectors, text, None).await
}

async fn human_type_with_provider(
    page: &Page,
    selectors: &[&str],
    text: &str,
    expected_provider: Option<&str>,
) -> anyhow::Result<()> {
    let selector = selectors
        .iter()
        .copied()
        .find(|selector| visible_composer_exists(page, &[*selector]))
        .ok_or_else(|| anyhow::anyhow!("actionable provider composer disappeared before typing"))?;
    let selector_js = serde_json::to_string(selector)?;
    let el_js = visible_input_query(&selector_js);
    // An unsupported scoped contenteditable edit must fail closed. Setting
    // synthetic .value or raw textContent may look correct while bypassing
    // framework/ProseMirror state; do not dispatch input/change on that path.
    // The DOM Range is confined to the selected editor, not the entire page.
    // Preserve Obscura's native click for ordinary single inputs. When the
    // first matching node is hidden, avoid clicking that stale placeholder;
    // the JS input path below focuses the visible match instead.
    let first_is_actionable = page.evaluate(&format!(
        "(() => {{ const el = {el_js}; return !!el && el === document.querySelector({selector_js}); }})()"
    ));
    if first_is_actionable.as_bool() == Some(true) {
        click_first(page, &[selector])?;
    }
    tokio::time::sleep(Duration::from_millis(80)).await;
    let text_js = serde_json::to_string(text)?;
    let route_guard = provider_origin_guard_js(expected_provider)?;
    let composer_guard = if expected_provider == Some("perplexity") {
        let count = actionable_composer_count_expression(selectors)?;
        format!("if ({count} !== 1) return 'ambiguous-composer';")
    } else {
        String::new()
    };
    let js = format!(
        "(function() {{ {route_guard} {composer_guard} var el = {el_js}; if (!el) return false; el.focus(); if (el.tagName === 'TEXTAREA' || el.tagName === 'INPUT') {{ try {{ var prototype = el.tagName === 'INPUT' ? window.HTMLInputElement.prototype : window.HTMLTextAreaElement.prototype; var setter = Object.getOwnPropertyDescriptor(prototype, 'value').set; setter.call(el, {text}); }} catch (e) {{ el.value = {text}; }} el.dispatchEvent(new InputEvent('input', {{ bubbles: true, inputType: 'insertText', data: {text} }})); el.dispatchEvent(new Event('change', {{ bubbles: true }})); return true; }} var selection = window.getSelection(); if (!selection) return false; try {{ var range = document.createRange(); range.selectNodeContents(el); selection.removeAllRanges(); selection.addRange(range); }} catch (e) {{ return false; }} var ok = false; try {{ ok = document.execCommand('insertText', false, {text}); }} catch (e) {{}} if (!ok) return 'editor-unsupported'; el.dispatchEvent(new InputEvent('input', {{ bubbles: true, inputType: 'insertText', data: {text} }})); el.dispatchEvent(new Event('change', {{ bubbles: true }})); return true; }})()",
        text = text_js,
        el_js = el_js,
    );
    let typed = page.evaluate(&js);
    if typed.as_str() == Some("off-provider") {
        anyhow::bail!("provider left its chat origin before typing");
    }
    if typed.as_str() == Some("ambiguous-composer") {
        anyhow::bail!("provider actionable composer ambiguous or unavailable");
    }
    if typed.as_str() == Some("editor-unsupported") {
        return Err(anyhow::Error::new(ProviderComposerInputRejectedError));
    }
    if typed.as_bool() != Some(true) {
        anyhow::bail!("typing into the provider input failed");
    }
    // Controlled editors may discard synthetic input on their next render.
    // A successful event dispatch alone is not evidence that the prompt stuck.
    tokio::time::sleep(Duration::from_millis(120)).await;
    if !composer_retains_prompt(page, selector, text)? {
        return Err(anyhow::Error::new(ProviderComposerInputRejectedError));
    }
    Ok(())
}

#[cfg(test)]
mod scoped_contenteditable_selection_tests {
    #[tokio::test]
    async fn scoped_selection_never_overwrites_unrelated_editor_or_page_text() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <h1 id="outside-heading">Original heading</h1>
            <div id="outside" contenteditable="true">Unrelated editor</div>
            <div id="target" contenteditable="true"><p>Old draft</p></div>
            <button id="send" type="button" onclick="window.sent=true">Send</button>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        page.evaluate("document.querySelector('#outside').focus()");
        let error = super::human_type(&page, &["#target"], "scoped draft")
            .await
            .unwrap_err();
        assert!(error
            .downcast_ref::<super::ProviderComposerInputRejectedError>()
            .is_some());
        // The pinned Obscura contenteditable implementation can expose a
        // synthetic retained value without updating visible nested markup.
        // That provider-input correctness gap is tracked separately in #988.
        // This atomic fixture verifies only that text selection never
        // overwrites unrelated page content or dispatches a Send click.
        assert_eq!(
            page.evaluate("document.querySelector('#outside').textContent")
                .as_str(),
            Some("Unrelated editor")
        );
        assert_eq!(
            page.evaluate("document.querySelector('#outside-heading').textContent")
                .as_str(),
            Some("Original heading")
        );
        assert_eq!(
            page.evaluate("window.sent === true").as_bool(),
            Some(false),
            "typing must not submit the prompt or click a send button"
        );
    }

    #[tokio::test]
    async fn unsupported_rich_editor_never_mutates_raw_dom_or_synthetic_value() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <div id="target" contenteditable="true"><p>Original nested draft</p></div>
            <button id="send" type="button" onclick="window.sent=true">Send</button>
            <script>
                const editor = document.querySelector('#target');
                editor.value = 'original synthetic value';
                window.editorInputs = 0;
                window.editorChanges = 0;
                editor.addEventListener('input', () => window.editorInputs++);
                editor.addEventListener('change', () => window.editorChanges++);
                document.execCommand = function() { return false; };
            </script>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        let error = super::human_type(&page, &["#target"], "secret multiline\n新 prompt")
            .await
            .unwrap_err();
        assert!(error
            .downcast_ref::<super::ProviderComposerInputRejectedError>()
            .is_some());
        assert!(!error.to_string().contains("secret"));
        assert_eq!(
            page.evaluate("document.querySelector('#target').innerHTML")
                .as_str(),
            Some("<p>Original nested draft</p>")
        );
        assert_eq!(
            page.evaluate("document.querySelector('#target').value")
                .as_str(),
            Some("original synthetic value")
        );
        assert_eq!(page.evaluate("window.editorInputs").as_f64(), Some(0.0));
        assert_eq!(page.evaluate("window.editorChanges").as_f64(), Some(0.0));
        assert_eq!(page.evaluate("window.sent === true").as_bool(), Some(false));
    }

    #[tokio::test]
    async fn native_textarea_still_accepts_exact_multiline_unicode() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <textarea id="native"></textarea>
            <script>
                window.nativeInputs = 0;
                document.querySelector('#native').addEventListener(
                    'input', () => window.nativeInputs++);
            </script>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let prompt = "你好\n\nemoji ☃️";
        super::human_type(&page, &["#native"], prompt)
            .await
            .unwrap();
        assert_eq!(
            page.evaluate("document.querySelector('#native').value")
                .as_str(),
            Some(prompt)
        );
        assert_eq!(page.evaluate("window.nativeInputs").as_f64(), Some(1.0));
    }
}

/// Dispatch an Enter keypress sequence on the element matching
/// `selector`. Obscura exposes no keyboard API, so this is necessarily
/// an approximation of Go's rod pressEnter: synthetic KeyboardEvents,
/// not trusted keystrokes. Frameworks listening only to trusted input
/// may ignore it.
pub fn press_enter(page: &Page, selector: &str) -> anyhow::Result<()> {
    press_enter_with_provider(page, selector, None)
}

fn press_enter_with_provider(
    page: &Page,
    selector: &str,
    expected_provider: Option<&str>,
) -> anyhow::Result<()> {
    let sel_js = serde_json::to_string(selector)?;
    let el_js = visible_input_query(&sel_js);
    let route_guard = provider_origin_guard_js(expected_provider)?;
    let js = format!(
        "(function() {{ {route_guard} var el = {el_js}; if (!el) return false; el.focus(); ['keydown','keypress','keyup'].forEach(function(t) {{ el.dispatchEvent(new KeyboardEvent(t, {{key:'Enter', code:'Enter', keyCode:13, which:13, bubbles:true, cancelable:true}})); }}); return true; }})()"
    );
    let result = page.evaluate(&js);
    if result.as_str() == Some("off-provider") {
        anyhow::bail!("provider left its chat origin before Enter fallback");
    }
    if result.as_bool() != Some(true) {
        anyhow::bail!("press Enter failed: input element not found");
    }
    Ok(())
}

/// Click the send button if one matches within `timeout_ms`, else
/// dispatch Enter on the input (Go's send fallback). Returns true when
/// a button was clicked, false when Enter was used.
pub async fn click_send_or_enter(
    page: &mut Page,
    send_selectors: &[&str],
    input_selector: &str,
    timeout_ms: u64,
) -> anyhow::Result<bool> {
    click_send_or_enter_checked(page, send_selectors, input_selector, timeout_ms, None).await
}

/// For production sends, verify the draft inside the same page evaluation as
/// the click. A controlled editor may rerender during the send-button wait:
/// an earlier retention check is not safe evidence of the *current* draft.
async fn click_send_or_enter_checked(
    page: &mut Page,
    send_selectors: &[&str],
    input_selector: &str,
    timeout_ms: u64,
    expected_prompt: Option<&str>,
) -> anyhow::Result<bool> {
    click_send_or_enter_checked_with_policy(
        page,
        send_selectors,
        input_selector,
        timeout_ms,
        expected_prompt,
        true,
        None,
    )
    .await
}

/// Controlled ChatGPT/Grok/Qwen/Manus editors cannot rely on synthetic Enter:
/// a missing Send control must not trigger an untrusted keystroke sequence.
/// Keep the existing legacy fallback only for the other provider adapters.
async fn click_send_or_enter_checked_with_policy(
    page: &mut Page,
    send_selectors: &[&str],
    input_selector: &str,
    timeout_ms: u64,
    expected_prompt: Option<&str>,
    allow_untrusted_enter: bool,
    expected_provider: Option<&str>,
) -> anyhow::Result<bool> {
    click_send_or_enter_checked_with_unique_composer(
        page,
        send_selectors,
        input_selector,
        timeout_ms,
        expected_prompt,
        SendClickPolicy {
            allow_untrusted_enter,
            expected_provider,
            unique_input_selectors: None,
        },
    )
    .await
}

struct SendClickPolicy<'a> {
    allow_untrusted_enter: bool,
    expected_provider: Option<&'a str>,
    unique_input_selectors: Option<&'a [&'a str]>,
}

/// Perplexity-only Send guard. The earlier composer check is not enough when
/// a controlled SPA renders a second visible editor while Send is pending.
/// The click check runs inside the same synchronous evaluation as the click.
async fn click_send_or_enter_checked_with_unique_composer(
    page: &mut Page,
    send_selectors: &[&str],
    input_selector: &str,
    timeout_ms: u64,
    expected_prompt: Option<&str>,
    policy: SendClickPolicy<'_>,
) -> anyhow::Result<bool> {
    let SendClickPolicy {
        allow_untrusted_enter,
        expected_provider,
        unique_input_selectors,
    } = policy;
    let start = std::time::Instant::now();
    let input_js = serde_json::to_string(input_selector)?;
    let input_expr = visible_input_query(&input_js);
    let expected_js = serde_json::to_string(&expected_prompt)?;
    let draft = composer_draft_expression("input");
    let route_guard = provider_origin_guard_js(expected_provider)?;
    let composer_guard = if let Some(inputs) = unique_input_selectors {
        let expr = actionable_composer_count_expression(inputs)?;
        format!("if ({expr} !== 1) return 'ambiguous-composer';")
    } else {
        String::new()
    };
    while start.elapsed().as_millis() < timeout_ms as u128 {
        for selector in send_selectors {
            let selector_js = serde_json::to_string(selector)?;
            let js = format!(
                "(() => {{ {route_guard} {composer_guard} const input = {input_expr}; if (!input) return 'missing'; const expected = {expected_js}; if (expected !== null && {draft} !== expected) return 'changed'; const inputForm = input.form || input.closest('form'); let root = input; for (let depth = 0; root && depth < 12; depth++, root = root.parentElement) {{ if (root.tagName === 'BODY' || root.tagName === 'HTML') break; for (const button of root.querySelectorAll({selector_js})) {{ const buttonForm = button.form || button.closest('form'); if (buttonForm && buttonForm !== inputForm) continue; if (button.getClientRects().length && window.getComputedStyle(button).visibility !== 'hidden' && window.getComputedStyle(button).pointerEvents !== 'none' && !button.closest('[inert], [hidden], [aria-hidden=true], [aria-disabled=true]') && !button.disabled && !button.matches(':disabled') && !button.closest('fieldset[disabled]') && button.getAttribute('aria-disabled') !== 'true') {{ button.click(); return 'clicked'; }} }} if (root.tagName === 'FORM') break; }} const form = input.form || input.closest('form'); if (form && form.id) {{ for (const button of document.querySelectorAll({selector_js})) {{ if (button.tagName !== 'BUTTON' || (button.form !== form && button.getAttribute('form') !== form.id) || form.contains(button)) continue; if (button.getClientRects().length && window.getComputedStyle(button).visibility !== 'hidden' && window.getComputedStyle(button).pointerEvents !== 'none' && !button.closest('[inert], [hidden], [aria-hidden=true], [aria-disabled=true]') && !button.disabled && !button.matches(':disabled') && !button.closest('fieldset[disabled]') && button.getAttribute('aria-disabled') !== 'true') {{ button.click(); return 'clicked'; }} }} }} return 'waiting'; }})()"
            );
            match page.evaluate(&js).as_str() {
                Some("clicked") => return Ok(true),
                Some("off-provider") => {
                    anyhow::bail!("provider left its chat origin before Send click")
                }
                Some("ambiguous-composer") => {
                    anyhow::bail!("provider actionable composer ambiguous or unavailable")
                }
                Some("changed" | "missing") if expected_prompt.is_some() => {
                    anyhow::bail!("provider draft changed or disappeared while waiting for send")
                }
                _ => {}
            }
        }
        drive_page(page, Duration::from_millis(150)).await;
    }
    // Recheck after the final asynchronous wait before any Enter fallback.
    if let Some(inputs) = unique_input_selectors {
        require_unique_actionable_composer(page, inputs)?;
    }
    // A disabled send control is an explicit refusal by the application.
    // Synthetic Enter is not trusted and must not bypass that state.
    let send_js = serde_json::to_string(send_selectors)?;
    let disabled = page.evaluate(&format!(
        "(() => {{ const input = {input_expr}; if (!input) return true; const selectors = {send_js}; const inputForm = input.form || input.closest('form'); let root = input; for (let depth = 0; root && depth < 12; depth++, root = root.parentElement) {{ if (root.tagName === 'BODY' || root.tagName === 'HTML') break; for (const selector of selectors) {{ for (const button of root.querySelectorAll(selector)) {{ const buttonForm = button.form || button.closest('form'); if (buttonForm && buttonForm !== inputForm) continue; if (button.getClientRects().length && window.getComputedStyle(button).visibility !== 'hidden' && window.getComputedStyle(button).pointerEvents !== 'none' && !button.closest('[inert], [aria-hidden=true]') && (button.disabled || button.matches(':disabled') || !!button.closest('fieldset[disabled]') || button.getAttribute('aria-disabled') === 'true' || !!button.closest('[aria-disabled=true]') || window.getComputedStyle(button).pointerEvents === 'none')) return true; }} }} if (root.tagName === 'FORM') break; }} const form = input.form || input.closest('form'); if (form && form.id) {{ for (const selector of selectors) {{ for (const button of document.querySelectorAll(selector)) {{ if (button.tagName !== 'BUTTON' || (button.form !== form && button.getAttribute('form') !== form.id) || form.contains(button)) continue; if (button.getClientRects().length && window.getComputedStyle(button).visibility !== 'hidden' && window.getComputedStyle(button).pointerEvents !== 'none' && !button.closest('[inert], [aria-hidden=true]') && (button.disabled || button.matches(':disabled') || !!button.closest('fieldset[disabled]') || button.getAttribute('aria-disabled') === 'true' || !!button.closest('[aria-disabled=true]') || window.getComputedStyle(button).pointerEvents === 'none')) return true; }} }} }} return false; }})()"
    ));
    if disabled.as_bool() == Some(true) {
        anyhow::bail!("provider send control is disabled or composer is missing");
    }
    // The composer can also change while we wait for a send control that is
    // absent. Never dispatch synthetic Enter against a different draft.
    if let Some(expected) = expected_prompt {
        if !composer_retains_prompt(page, input_selector, expected)? {
            anyhow::bail!("provider draft changed or disappeared before Enter fallback");
        }
    }
    // Missing controls are not permission to synthesize keyboard input on
    // ChatGPT/Grok/Qwen/Manus: those editors must offer an actionable Send button.
    if !allow_untrusted_enter {
        anyhow::bail!(
            "provider Send control unavailable; synthetic Enter is not a supported submission path"
        );
    }
    // Enter dispatch remains a best-effort legacy fallback for other sites.
    press_enter_with_provider(page, input_selector, expected_provider)?;
    Ok(false)
}

#[cfg(test)]
mod draft_at_click_tests {
    use super::{click_send_or_enter_checked, click_send_or_enter_checked_with_policy};

    #[tokio::test]
    async fn perplexity_send_click_rejects_newly_ambiguous_editors_atomically() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <form><textarea id="thread" class="candidate">draft</textarea>
                <button id="send" type="button" onclick="window.sent=true">Send</button>
            </form>
            <textarea id="search" class="candidate"></textarea>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let result = super::click_send_or_enter_checked_with_unique_composer(
            &mut page,
            &["#send"],
            "#thread",
            100,
            Some("draft"),
            super::SendClickPolicy {
                allow_untrusted_enter: true,
                expected_provider: None,
                unique_input_selectors: Some(&["textarea.candidate", "#thread"]),
            },
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("ambiguous"));
        assert_eq!(page.evaluate("window.sent === true").as_bool(), Some(false));

        // The same editor matched by two configured selectors counts once.
        page.evaluate("document.querySelector('#search').setAttribute('hidden', '')");
        let clicked = super::click_send_or_enter_checked_with_unique_composer(
            &mut page,
            &["#send"],
            "#thread",
            100,
            Some("draft"),
            super::SendClickPolicy {
                allow_untrusted_enter: true,
                expected_provider: None,
                unique_input_selectors: Some(&["textarea.candidate", "#thread"]),
            },
        )
        .await
        .unwrap();
        assert!(clicked);
        assert_eq!(page.evaluate("window.sent === true").as_bool(), Some(true));
    }

    #[tokio::test]
    async fn pointer_events_blocked_send_is_not_clicked_or_bypassed() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><form>
            <textarea id="prompt">draft</textarea>
            <button id="send" type="button" style="pointer-events:none"
                onclick="window.sent=true">Send</button>
        </form></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let result = click_send_or_enter_checked_with_policy(
            &mut page,
            &["#send"],
            "#prompt",
            100,
            Some("draft"),
            false,
            None,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(page.evaluate("window.sent === true").as_bool(), Some(false));
    }

    #[tokio::test]
    async fn foreign_redirect_cannot_click_matching_send_button() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><form>
            <textarea id="prompt">draft</textarea>
            <button id="send" type="button" onclick="window.sent=(window.sent||0)+1">Send</button>
        </form></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let error = click_send_or_enter_checked_with_policy(
            &mut page,
            &["#send"],
            "#prompt",
            100,
            Some("draft"),
            false,
            Some("chatgpt"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("chat origin"));
        assert_eq!(page.evaluate("window.sent || 0").as_f64(), Some(0.0));
        // The same fixture with no provider route binding can still use the
        // ordinary click path; refusal above is about the foreign origin.
        assert!(click_send_or_enter_checked_with_policy(
            &mut page,
            &["#send"],
            "#prompt",
            100,
            Some("draft"),
            true,
            None,
        )
        .await
        .unwrap());
        assert_eq!(page.evaluate("window.sent || 0").as_f64(), Some(1.0));
    }

    #[tokio::test]
    async fn missing_send_control_never_dispatches_untrusted_enter_for_guarded_providers() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><form>
            <textarea id="prompt">expected draft</textarea>
            <script>
                window.enters = 0;
                document.querySelector('#prompt').addEventListener('keydown', () => window.enters++);
            </script>
        </form></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let error = click_send_or_enter_checked_with_policy(
            &mut page,
            &["#missing-send"],
            "#prompt",
            100,
            Some("expected draft"),
            false,
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("synthetic Enter"));
        assert_eq!(page.evaluate("window.enters || 0").as_f64(), Some(0.0));
        assert_eq!(
            page.evaluate("document.querySelector('#prompt').value")
                .as_str(),
            Some("expected draft")
        );
        // The same harmless fixture still exercises the legacy path when
        // explicitly opted in for providers without strict acknowledgement.
        assert!(!click_send_or_enter_checked_with_policy(
            &mut page,
            &["#missing-send"],
            "#prompt",
            100,
            Some("expected draft"),
            true,
            None,
        )
        .await
        .unwrap());
        assert!(page.evaluate("window.enters || 0").as_f64().unwrap_or(0.0) > 0.0);
    }

    #[tokio::test]
    async fn changed_draft_must_not_click_enabled_button_or_dispatch_enter() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><form>
            <textarea id="prompt">changed by framework</textarea>
            <button id="send" type="button" onclick="window.sent=(window.sent||0)+1">Send</button>
            <script>
                window.enters = 0;
                document.querySelector('#prompt').addEventListener('keydown', () => window.enters++);
            </script>
        </form></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let error = click_send_or_enter_checked(
            &mut page,
            &["#send"],
            "#prompt",
            200,
            Some("original draft"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("provider draft changed"));
        assert_eq!(page.evaluate("window.sent || 0").as_f64(), Some(0.0));
        assert_eq!(page.evaluate("window.enters || 0").as_f64(), Some(0.0));
    }

    #[tokio::test]
    async fn external_form_associated_send_button_is_used_without_cross_composer_click() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <form id="unrelated"><textarea>other draft</textarea></form>
            <button form="unrelated" class="send" type="button"
                onclick="window.other=(window.other||0)+1">Wrong composer</button>
            <form id="target"><textarea id="prompt">current draft</textarea></form>
            <button form="target" class="send" type="button"
                onclick="window.sent=(window.sent||0)+1">Send</button>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        assert!(click_send_or_enter_checked(
            &mut page,
            &["button.send"],
            "#prompt",
            300,
            Some("current draft"),
        )
        .await
        .unwrap());
        assert_eq!(page.evaluate("window.sent || 0").as_f64(), Some(1.0));
        assert_eq!(page.evaluate("window.other || 0").as_f64(), Some(0.0));
    }

    #[tokio::test]
    async fn shared_panel_cannot_click_a_different_forms_send_button() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <div id="panel">
                <textarea id="prompt">current draft</textarea>
                <form id="other"><button class="send" type="button"
                    onclick="window.other++">Wrong form</button></form>
                <button class="send" type="button"
                    onclick="window.sent++">Current composer</button>
            </div>
            <script>window.other=0; window.sent=0;</script>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        assert!(click_send_or_enter_checked(
            &mut page,
            &["button.send"],
            "#prompt",
            250,
            Some("current draft"),
        )
        .await
        .unwrap());
        assert_eq!(page.evaluate("window.sent || 0").as_f64(), Some(1.0));
        assert_eq!(page.evaluate("window.other || 0").as_f64(), Some(0.0));
    }

    #[tokio::test]
    async fn unrelated_disabled_form_does_not_masquerade_as_our_disabled_send() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <div id="panel">
                <textarea id="prompt">current draft</textarea>
                <form id="other"><button class="send" type="button" disabled
                    onclick="window.other++">Wrong form</button></form>
            </div>
            <script>window.other=0;</script>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let error = click_send_or_enter_checked_with_policy(
            &mut page,
            &["button.send"],
            "#prompt",
            100,
            Some("current draft"),
            false,
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("synthetic Enter"));
        assert_eq!(page.evaluate("window.other || 0").as_f64(), Some(0.0));
    }

    #[tokio::test]
    async fn disabled_external_form_send_must_not_trigger_enter_fallback() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <form id="target"><textarea id="prompt">current draft</textarea></form>
            <button form="target" id="send" type="button" disabled
                onclick="window.sent=(window.sent||0)+1">Send</button>
            <script>
                window.enters=0;
                document.querySelector('#prompt').addEventListener('keydown',
                    () => { window.enters++; });
            </script>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let error = click_send_or_enter_checked(
            &mut page,
            &["#send"],
            "#prompt",
            200,
            Some("current draft"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("disabled"));
        assert_eq!(page.evaluate("window.sent || 0").as_f64(), Some(0.0));
        assert_eq!(page.evaluate("window.enters || 0").as_f64(), Some(0.0));
    }

    #[tokio::test]
    async fn unchanged_draft_can_click_send() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><form>
            <textarea id="prompt">original draft</textarea>
            <button id="send" type="button" onclick="window.sent=(window.sent||0)+1">Send</button>
        </form></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        assert!(click_send_or_enter_checked(
            &mut page,
            &["#send"],
            "#prompt",
            200,
            Some("original draft")
        )
        .await
        .unwrap());
        assert_eq!(page.evaluate("window.sent || 0").as_f64(), Some(1.0));
    }
}

fn matching_model_labels(labels: Vec<String>, keywords: &str) -> anyhow::Result<Vec<String>> {
    let matcher = regex::RegexBuilder::new(keywords)
        .case_insensitive(true)
        .build()?;
    Ok(normalize_model_labels(labels)
        .into_iter()
        .filter(|label| matcher.is_match(label))
        .collect())
}

/// Classify actual authentication redirects, not incidental keywords in
/// article paths, query strings, or conversation IDs. A login URL is evidence
/// of expired auth; an absent composer alone is not.
pub fn is_login_url(url: &str) -> bool {
    let Ok(uri) = url.parse::<axum::http::Uri>() else {
        return false;
    };
    if !matches!(uri.scheme_str(), Some("https" | "http")) {
        return false;
    }
    let host = uri.host().unwrap_or_default().to_ascii_lowercase();
    let host = host.trim_end_matches('.');
    if matches!(
        host,
        "accounts.google.com" | "accounts.x.com" | "login.microsoftonline.com"
    ) || host.starts_with("login.")
        || host.starts_with("auth.")
        || host.ends_with(".auth0.com")
    {
        return true;
    }
    uri.path().split('/').any(|segment| {
        matches!(
            segment.to_ascii_lowercase().as_str(),
            "login" | "signin" | "sign-in" | "auth" | "oauth" | "oauth2" | "authorize" | "sso"
        )
    })
}

pub fn is_login_page(page: &Page) -> bool {
    is_login_url(&page.url())
}

#[cfg(test)]
mod login_url_classification_tests {
    use super::is_login_url;

    #[test]
    fn recognizes_explicit_provider_and_sso_redirects() {
        for url in [
            "https://accounts.google.com/v3/signin/challenge",
            "https://accounts.x.com/",
            "https://chatgpt.com/auth/login",
            "https://claude.ai/login?next=%2Fnew",
            "https://login.microsoftonline.com/tenant/oauth2/authorize",
            "https://login.auth0.com/authorize",
        ] {
            assert!(is_login_url(url), "missed authentication redirect: {url}");
        }
    }

    #[test]
    fn does_not_treat_unrelated_url_text_as_expired_authentication() {
        for url in [
            "https://chatgpt.com/c/authoring",
            "https://grok.com/author",
            "https://gemini.google.com/app?q=auth",
            "https://claude.ai/new?redirect=https%3A%2F%2Fauth.openai.com%2Flogin",
            "https://accounts.x.com.attacker.invalid/",
            "data:text/html,<textarea></textarea>",
        ] {
            assert!(!is_login_url(url), "false login detection: {url}");
        }
    }
}

pub fn check_session(provider: &str, page: &Page) -> anyhow::Result<()> {
    if is_login_page(page) {
        let _ = provider;
        return Err(anyhow::Error::new(
            crate::browser::pool::ProviderSessionExpiredError,
        ));
    }
    Ok(())
}

/// Go discovery normalization (discovery.go normalizeModelOption +
/// isNonModelLabel), as a post-processing step on Rust v1's scraped
/// labels: first non-empty line, whitespace-collapsed, at most 120
/// chars, dropping the 14 non-model labels. Rust v1's config-alias
/// merge is kept (Go's discovered-only return is NOT adopted).
pub const NON_MODEL_LABELS: &[&str] = &[
    "selected",
    "current",
    "select",
    "close",
    "cancel",
    "settings",
    "new chat",
    "new conversation",
    "search",
    "more",
    "home",
    "explore",
    "recent",
    "history",
];

pub fn normalize_model_option(raw: &str) -> Option<String> {
    let first = raw.lines().find(|l| !l.trim().is_empty())?;
    let collapsed = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() || collapsed.len() > 120 {
        return None;
    }
    if NON_MODEL_LABELS
        .iter()
        .any(|l| l.eq_ignore_ascii_case(&collapsed))
    {
        return None;
    }
    Some(collapsed)
}

pub fn normalize_model_labels(labels: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in labels {
        if let Some(n) = normalize_model_option(&l) {
            if !out.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                out.push(n);
            }
        }
    }
    out
}

/// Model selection in the send path — Go's TrySelectModel +
/// selectRequestedModel (base.go) in Rust form. Blank model, or a model
/// equal (case-insensitive, trimmed) to a default alias, skips the UI
/// entirely. Otherwise, per pair: trigger click (3s budget) → 400ms →
/// first option whose text CONTAINS the alias (case-insensitive) →
/// click → 300ms → Escape + next pair on miss. Returns Err when a
/// selection was attempted and every pair missed, so the caller can
/// fail the attempt and let the router try the next provider.
pub async fn select_model(
    page: &mut Page,
    pairs: &[(&str, &str)],
    model: &str,
    default_aliases: &[String],
) -> anyhow::Result<()> {
    let model = model.trim();
    if model.is_empty() {
        return Ok(());
    }
    if default_aliases
        .iter()
        .any(|a| a.trim().eq_ignore_ascii_case(model))
    {
        return Ok(());
    }
    let target_js = serde_json::to_string(&model.to_lowercase())?;
    for (trigger, options) in pairs {
        let found = {
            let start = std::time::Instant::now();
            let mut hit = false;
            while start.elapsed().as_millis() < 3000 {
                if let Some(el) = page.query_selector(trigger) {
                    el.click()?;
                    hit = true;
                    break;
                }
                drive_page(page, Duration::from_millis(150)).await;
            }
            hit
        };
        if !found {
            continue;
        }
        drive_page(page, Duration::from_millis(400)).await;
        let opt_js = serde_json::to_string(options)?;
        let js = format!(
            "(function() {{ var els = document.querySelectorAll({}); for (var i = 0; i < els.length; i++) {{ var t = (els[i].innerText || els[i].textContent || ''); if (t.toLowerCase().indexOf({}) !== -1) {{ els[i].click(); return t; }} }} return null; }})()",
            opt_js, target_js
        );
        let v = page.evaluate(&js);
        if !v.is_null() {
            drive_page(page, Duration::from_millis(300)).await;
            return Ok(());
        }
        // Miss: Escape (synthetic, as with Enter — no keyboard API),
        // then the next pair.
        let _ = page.evaluate("(function() { var el = document.activeElement || document.body; el.dispatchEvent(new KeyboardEvent('keydown', {key:'Escape', code:'Escape', keyCode:27, which:27, bubbles:true, cancelable:true})); return true; })()");
    }
    anyhow::bail!("model '{model}' could not be selected in the provider UI");
}

/// Selector probe for --test-adapter: first matching selector per
/// group, or None when every selector in the group missed. Obscura's
/// query_selector is an instant snapshot of the loaded page, unlike
/// Go's ProbeSelectors which waited up to 5s per selector — probe
/// results therefore describe the page's current state only.
pub fn probe_selector_groups(
    page: &Page,
    groups: &[(&str, &[&str])],
) -> Vec<(String, Option<String>)> {
    groups
        .iter()
        .map(|(role, selectors)| {
            let hit = selectors
                .iter()
                .find(|s| page.query_selector(s).is_some())
                .map(|s| s.to_string());
            (role.to_string(), hit)
        })
        .collect()
}

/// Text of the last visible non-empty element matching `selector`,
/// or None when no actionable response is present. Old hidden SPA nodes
/// must not override the latest visible assistant response or satisfy probes.
pub fn last_element_text(page: &Page, selector: &str) -> Option<String> {
    let sel_js = serde_json::to_string(selector).ok()?;
    let js = format!(
        r#"(() => {{
            const nodes = Array.from(document.querySelectorAll({sel_js}));
            for (let i = nodes.length - 1; i >= 0; i--) {{
                const node = nodes[i];
                if (!node.getClientRects().length ||
                    window.getComputedStyle(node).visibility === 'hidden' ||
                    node.closest('[hidden], [inert], [aria-hidden="true"], [aria-disabled="true"], fieldset[disabled]') ||
                    node.disabled || node.matches(':disabled')) continue;
                const value = node.innerText || node.textContent || '';
                if (value.trim()) return value;
            }}
            return null;
        }})()"#
    );
    page.evaluate(&js).as_str().map(str::to_owned)
}

/// Response text for an adapter: first response selector (in fallback
/// order) that currently yields non-empty text.
pub fn response_text(page: &Page, selectors: &[&str]) -> Option<String> {
    for sel in selectors {
        if let Some(t) = last_element_text(page, sel) {
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

#[cfg(test)]
mod last_visible_response_text_tests {
    #[tokio::test]
    async fn hidden_and_disabled_last_nodes_cannot_override_real_response() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <div class="answer">visible answer</div>
            <div class="answer" hidden>stale hidden answer</div>
            <div aria-hidden="true"><div class="answer">stale aria answer</div></div>
            <div aria-disabled="true"><div class="answer">stale disabled answer</div></div>
            <div class="answer" style="visibility:hidden">stale style answer</div>
            </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        assert_eq!(
            super::last_element_text(&page, ".answer").as_deref(),
            Some("visible answer")
        );
        assert_eq!(
            super::response_text(&page, &[".answer"]).as_deref(),
            Some("visible answer")
        );
        page.evaluate("document.querySelector('.answer').hidden = true");
        assert!(super::last_element_text(&page, ".answer").is_none());
        assert!(super::response_text(&page, &[".answer"]).is_none());
    }

    #[tokio::test]
    async fn trailing_empty_visible_node_does_not_hide_earlier_answer() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body><div class='answer'>answer</div><div class='answer'></div></body></html>")
            .await
            .unwrap();
        assert_eq!(
            super::last_element_text(&page, ".answer").as_deref(),
            Some("answer")
        );
    }
}

/// Discover models without sending a message (v1 body): navigate, open
/// the model picker, scrape visible labels filtered by keyword regex,
/// merge with the configured aliases.
pub async fn discover_models(
    page: &mut Page,
    set: &SelectorSet,
    cfg_models: &[String],
    overrides: &crate::config::ProviderSelectorOverrides,
) -> anyhow::Result<Vec<String>> {
    page.goto(set.url).await?;
    drive_page(page, Duration::from_millis(1200)).await;

    // Try to open model picker without sending message
    let effective = set.effective(overrides);
    for sel in effective.picker_refs() {
        if click_first(page, &[sel]).is_ok() {
            drive_page(page, Duration::from_millis(600)).await;
            break;
        }
    }

    // Extract visible model labels (obscura evaluate takes an expression
    // and returns a Value synchronously, so call the v1 arrow function)
    let js_fn = r#"() => {
        const texts = [];
        document.querySelectorAll('[role="option"], [data-testid*="model"], button, li, div').forEach(el => {
            const t = el.innerText?.trim();
            if (t && t.length < 40 && /(KEYWORDS)/i.test(t)) texts.push(t);
        });
        return [...new Set(texts)].slice(0, 20);
    }"#.replace("KEYWORDS", set.model_keywords);
    let discovered: Vec<String> =
        serde_json::from_value(page.evaluate(&format!("({})()", js_fn))).unwrap_or_default();
    // Go discovery normalization as a post-processing step (see above).
    let discovered = matching_model_labels(discovered, set.model_keywords)?;

    if discovered.is_empty() {
        Ok(cfg_models.to_vec())
    } else {
        // Merge with configured aliases (Rust v1 behavior; Go's
        // discovered-only return is deliberately NOT adopted).
        let mut merged = cfg_models.to_vec();
        for m in discovered {
            if !merged.iter().any(|x| x.eq_ignore_ascii_case(&m)) {
                merged.push(m);
            }
        }
        Ok(merged)
    }
}

/// ChatGPT, Grok, Qwen and Manus require an observable composer/response
/// transition to acknowledge a send. Match actual provider hosts and Grok's alternate X
/// route, never incidental substrings in query parameters or attacker hosts.
pub(crate) fn submission_provider_for_url(url: &str) -> Option<&'static str> {
    let Ok(uri) = url.parse::<axum::http::Uri>() else {
        return None;
    };
    if uri.scheme_str() != Some("https")
        || uri.authority().is_none_or(|authority| {
            authority.as_str().contains('@') || authority.port_u16().is_some_and(|port| port != 443)
        })
    {
        return None;
    }
    let host = uri
        .host()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    match host.as_str() {
        "chatgpt.com" | "www.chatgpt.com" => Some("chatgpt"),
        "grok.com" | "www.grok.com" => Some("grok"),
        "chat.qwen.ai" => Some("qwen"),
        "manus.im" | "www.manus.im" => Some("manus"),
        "x.com" | "www.x.com" if uri.path() == "/i/grok" || uri.path().starts_with("/i/grok/") => {
            Some("grok")
        }
        _ => None,
    }
}

/** Declare a closed set of browser origins for all provider adapters.
 * A matching generic text box on an unrelated page is never authorization
 * to type. ChatGPT/Grok/Qwen retain their separate strict submission contract.
 */
fn provider_origin_for_url(url: &str) -> Option<&'static str> {
    if let Some(provider) = submission_provider_for_url(url) {
        return Some(provider);
    }
    let uri = url.parse::<axum::http::Uri>().ok()?;
    if uri.scheme_str() != Some("https")
        || uri.authority().is_none_or(|authority| {
            authority.as_str().contains('@') || authority.port_u16().is_some_and(|port| port != 443)
        })
    {
        return None;
    }
    match uri
        .host()?
        .trim_end_matches('.')
        .to_ascii_lowercase()
        .as_str()
    {
        "claude.ai" | "www.claude.ai" => Some("claude"),
        "gemini.google.com" => Some("gemini"),
        "chat.qwen.ai" => Some("qwen"),
        "www.meta.ai" | "meta.ai" => Some("metaai"),
        "chat.mistral.ai" => Some("mistral"),
        "www.perplexity.ai" | "perplexity.ai" => Some("perplexity"),
        "manus.im" | "www.manus.im" => Some("manus"),
        _ => None,
    }
}

/// Reject every unrecognized origin, regardless of matching page selectors.
fn require_provider_origin(provider: &str, url: &str) -> anyhow::Result<()> {
    if provider_origin_for_url(url) == Some(provider) {
        Ok(())
    } else {
        anyhow::bail!("provider chat origin unavailable before prompt dispatch")
    }
}

/// Only an explicit in-memory fixture can bypass real-origin navigation in
/// unit tests. The check is removed from production builds entirely.
#[cfg(test)]
fn is_provider_test_fixture(page: &Page) -> bool {
    page.url().starts_with("data:text/html,")
        && page
            .evaluate("document.documentElement.hasAttribute('data-chat-gateway-test-fixture')")
            .as_bool()
            == Some(true)
}

fn require_page_provider_origin(provider: &str, page: &Page) -> anyhow::Result<()> {
    #[cfg(test)]
    if is_provider_test_fixture(page) {
        return Ok(());
    }
    require_provider_origin(provider, &page.url())
}

/// Inline page guard reused for typing *and* the atomic Send click. A
/// Rust-side check before an await is insufficient if navigation occurs
/// while a framework-controlled composer is settling.
fn provider_origin_guard_js(expected_provider: Option<&str>) -> anyhow::Result<String> {
    let provider = serde_json::to_string(&expected_provider)?;
    Ok(format!(
        r#"const expectedProvider = {provider};
           if (expectedProvider !== null) {{
               // The pinned embedded browser exposes Location fields but
               // does not implement the WHATWG URL constructor.
               const currentUrl = location;
               if (!currentUrl || !currentUrl.hostname) return 'off-provider';
               const host = currentUrl.hostname.toLowerCase().replace(/\.$/, '');
               const path = currentUrl.pathname;
               let actualProvider = null;
               switch (host) {{
                   case 'chatgpt.com': case 'www.chatgpt.com': actualProvider = 'chatgpt'; break;
                   case 'grok.com': case 'www.grok.com': actualProvider = 'grok'; break;
                   case 'x.com': case 'www.x.com':
                       if (path === '/i/grok' || path.startsWith('/i/grok/')) actualProvider = 'grok';
                       break;
                   case 'claude.ai': case 'www.claude.ai': actualProvider = 'claude'; break;
                   case 'gemini.google.com': actualProvider = 'gemini'; break;
                   case 'chat.qwen.ai': actualProvider = 'qwen'; break;
                   case 'www.meta.ai': case 'meta.ai': actualProvider = 'metaai'; break;
                   case 'chat.mistral.ai': actualProvider = 'mistral'; break;
                   case 'www.perplexity.ai': case 'perplexity.ai': actualProvider = 'perplexity'; break;
                   case 'manus.im': case 'www.manus.im': actualProvider = 'manus'; break;
               }}
               if (currentUrl.protocol !== 'https:' || currentUrl.username ||
                   currentUrl.password || (currentUrl.port && currentUrl.port !== '443') ||
                   actualProvider !== expectedProvider) return 'off-provider';
           }}"#
    ))
}

#[cfg(test)]
mod all_provider_origin_fence_tests {
    use super::{provider_origin_for_url, provider_origin_guard_js, require_provider_origin};

    #[test]
    fn every_provider_has_explicit_matching_https_origin() {
        for (provider, url) in [
            ("chatgpt", "https://chatgpt.com/c/abc"),
            ("chatgpt", "https://www.chatgpt.com/"),
            ("grok", "https://grok.com/chat/abc"),
            ("grok", "https://x.com/i/grok/abc"),
            ("claude", "https://claude.ai/new"),
            ("gemini", "https://gemini.google.com/app"),
            ("qwen", "https://chat.qwen.ai/"),
            ("metaai", "https://www.meta.ai/"),
            ("mistral", "https://chat.mistral.ai/chat"),
            ("perplexity", "https://www.perplexity.ai/search/abc"),
            ("manus", "https://manus.im/app"),
        ] {
            assert_eq!(provider_origin_for_url(url), Some(provider), "{url}");
            require_provider_origin(provider, url).unwrap();
        }
    }

    #[test]
    fn rejects_cross_provider_redirects_and_spoofed_origins() {
        for (provider, url) in [
            ("qwen", "https://chatgpt.com/c/abc"),
            ("claude", "https://manus.im/app"),
            (
                "perplexity",
                "https://www.perplexity.ai.attacker.invalid/search",
            ),
            ("metaai", "https://attacker.invalid/?next=www.meta.ai"),
            ("gemini", "https://gemini.google.com@attacker.invalid/app"),
            ("manus", "http://manus.im/app"),
            ("mistral", "https://chat.mistral.ai:8443/chat"),
            ("grok", "https://x.com/home"),
            ("chatgpt", "data:text/html,hello"),
            ("qwen", "about:blank"),
        ] {
            assert_ne!(provider_origin_for_url(url), Some(provider), "{url}");
            assert!(require_provider_origin(provider, url).is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn redirected_composer_cannot_receive_prompt_or_send() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body><form><textarea id='prompt'></textarea><button id='send' type='button' onclick='window.sent=true'>Send</button></form></body></html>").await.unwrap();
        let error =
            super::human_type_with_provider(&page, &["#prompt"], "private draft", Some("qwen"))
                .await
                .unwrap_err();
        assert!(error.to_string().contains("chat origin"));
        assert!(!error.to_string().contains("private draft"));
        assert_eq!(
            page.evaluate("document.querySelector('#prompt').value")
                .as_str(),
            Some("")
        );

        page.evaluate("document.querySelector('#prompt').value = 'private draft'");
        let rejected = super::click_send_or_enter_checked_with_policy(
            &mut page,
            &["#send"],
            "#prompt",
            100,
            Some("private draft"),
            true,
            Some("qwen"),
        )
        .await
        .unwrap_err();
        assert!(rejected.to_string().contains("chat origin"));
        assert_eq!(page.evaluate("window.sent === true").as_bool(), Some(false));

        let enter = super::press_enter_with_provider(&page, "#prompt", Some("qwen")).unwrap_err();
        assert!(enter.to_string().contains("chat origin"));
    }

    #[test]
    fn inline_page_guard_declares_every_provider_and_embedded_safe_location() {
        // The embedded VM does not support a synthetic WHATWG URL/Location
        // object with ordinary browser property semantics. Verify the route
        // cases against parsed Rust URIs above and exercise actual foreign
        // page rejection in redirected_composer_cannot_receive_prompt_or_send.
        let guard = provider_origin_guard_js(Some("qwen")).unwrap();
        assert!(guard.contains("const currentUrl = location;"));
        assert!(!guard.contains("new URL("));
        for provider in [
            "chatgpt",
            "grok",
            "claude",
            "gemini",
            "qwen",
            "metaai",
            "mistral",
            "perplexity",
            "manus",
        ] {
            let named = format!("actualProvider = '{provider}'");
            assert!(
                guard.contains(&named),
                "missing inline route for {provider}"
            );
        }
        assert!(guard.contains("currentUrl.protocol !== 'https:'"));
        assert!(guard.contains("currentUrl.username"));
        assert!(guard.contains("currentUrl.port"));
    }
}

fn requires_submission_acknowledgement(url: &str) -> bool {
    submission_provider_for_url(url).is_some()
}

/// A browser page recognized as ChatGPT, Grok or Qwen must never be operated by
/// another adapter simply because it exposes a matching generic composer.
/// This is NOT an all-provider origin allowlist: unknown origins still need
/// their own provider-specific browser-route checks and live evidence.
#[cfg(test)]
fn require_matching_known_provider_origin(adapter: &str, url: &str) -> anyhow::Result<()> {
    if submission_provider_for_url(url).is_some_and(|observed| observed != adapter) {
        anyhow::bail!("provider adapter does not match current chat origin");
    }
    Ok(())
}

#[cfg(test)]
mod provider_adapter_origin_consistency_tests {
    use super::require_matching_known_provider_origin as require;

    #[test]
    fn recognized_chat_origins_accept_only_the_matching_adapter() {
        for (url, expected) in [
            ("https://chatgpt.com/c/abc", "chatgpt"),
            ("https://www.chatgpt.com/", "chatgpt"),
            ("https://grok.com/chat/123", "grok"),
            ("https://x.com/i/grok/thread/123", "grok"),
            ("https://www.x.com/i/grok", "grok"),
            ("https://chat.qwen.ai/chat/abc", "qwen"),
        ] {
            assert!(require(expected, url).is_ok());
            for wrong in ["manus", "claude", "qwen", "chatgpt", "grok"] {
                if wrong == expected {
                    continue;
                }
                let error = require(wrong, url).unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "provider adapter does not match current chat origin"
                );
                assert!(!error.to_string().contains(url));
            }
        }
    }

    #[test]
    fn unrelated_and_spoofed_urls_are_not_mistaken_for_known_provider_origins() {
        // No match is NOT authorization to send: unknown providers retain
        // separate unresolved signed-in route acceptance.
        for url in [
            "https://chatgpt.com.attacker.invalid/c/abc",
            "https://x.com/home?q=/i/grok",
            "https://grok.com.attacker.invalid/",
            "http://chatgpt.com/",
            "https://chatgpt.com:8443/",
            "data:text/html,<textarea>grok.com</textarea>",
        ] {
            assert!(require("manus", url).is_ok(), "{url}");
        }
    }
}

/// A composer's DOM selectors are not trusted evidence of a ChatGPT/Grok/Qwen
/// origin. Reject unrelated pages before input or click, and preserve the
/// typed expired-session error for explicit authentication redirects.
pub fn require_submission_provider_route(provider: &str, page: &Page) -> anyhow::Result<()> {
    check_session(provider, page)?;
    // Unit fixtures need to exercise the real adapters without live network
    // access. This exception exists only in test builds and requires an
    // explicit marker on an in-memory data: document, never a provider page.
    #[cfg(test)]
    if page.url().starts_with("data:text/html,")
        && page
            .evaluate("document.documentElement.hasAttribute('data-chat-gateway-test-fixture')")
            .as_bool()
            == Some(true)
    {
        return Ok(());
    }
    if submission_provider_for_url(&page.url()) != Some(provider) {
        anyhow::bail!("provider chat origin unavailable before prompt submission");
    }
    Ok(())
}

#[cfg(test)]
mod provider_submission_route_tests {
    use super::requires_submission_acknowledgement;

    #[test]
    fn requires_ack_on_real_chatgpt_and_both_grok_routes() {
        for url in [
            "https://chatgpt.com/",
            "https://chatgpt.com/c/abcdef",
            "https://grok.com/",
            "https://grok.com/chat/123",
            "https://x.com/i/grok",
            "https://x.com/i/grok/",
            "https://www.x.com/i/grok/conversation/123",
            "https://chat.qwen.ai/",
            "https://chat.qwen.ai/chat/abc",
        ] {
            assert!(
                requires_submission_acknowledgement(url),
                "missing acknowledgement guard for {url}"
            );
        }
    }

    #[test]
    fn chatgpt_and_grok_routes_remain_distinct_across_provider_navigation() {
        use super::submission_provider_for_url;
        assert_eq!(
            submission_provider_for_url("https://chatgpt.com/c/abc"),
            Some("chatgpt")
        );
        assert_eq!(
            submission_provider_for_url("https://grok.com/chat/abc"),
            Some("grok")
        );
        assert_eq!(
            submission_provider_for_url("https://x.com/i/grok/abc"),
            Some("grok")
        );
        assert_eq!(
            submission_provider_for_url("https://chat.qwen.ai/chat/abc"),
            Some("qwen")
        );
        assert_ne!(
            submission_provider_for_url("https://chatgpt.com/"),
            submission_provider_for_url("https://x.com/i/grok")
        );
        for url in [
            "http://chatgpt.com/",
            "https://chatgpt.com:8443/",
            "https://x.com/home",
            "https://chatgpt.com.attacker.invalid/",
        ] {
            assert!(submission_provider_for_url(url).is_none(), "{url}");
        }
    }

    #[test]
    fn qwen_strict_submission_scope_never_accepts_spoofed_host_or_enter_only() {
        for url in ["https://chat.qwen.ai/", "https://chat.qwen.ai/chat/current"] {
            assert_eq!(super::submission_provider_for_url(url), Some("qwen"));
            assert!(requires_submission_acknowledgement(url));
        }
        for url in [
            "http://chat.qwen.ai/",
            "https://chat.qwen.ai:8443/",
            "https://chat.qwen.ai.attacker.invalid/",
            "https://elsewhere.invalid/?origin=chat.qwen.ai",
        ] {
            assert_ne!(super::submission_provider_for_url(url), Some("qwen"));
            assert!(!requires_submission_acknowledgement(url));
        }
        // Strict send_prompt requires actionable Send and prompt-bound snapshot,
        // never a best-effort synthetic Enter fallback.
    }

    #[test]
    fn manus_strict_submission_scope_rejects_spoofed_origin() {
        for url in ["https://manus.im/app", "https://www.manus.im/app"] {
            assert_eq!(super::submission_provider_for_url(url), Some("manus"));
            assert!(requires_submission_acknowledgement(url));
        }
        for url in [
            "http://manus.im/app",
            "https://manus.im:8443/app",
            "https://manus.im.attacker.invalid/app",
            "https://elsewhere.invalid/?next=manus.im",
        ] {
            assert_ne!(super::submission_provider_for_url(url), Some("manus"));
            assert!(!requires_submission_acknowledgement(url));
        }
    }

    #[tokio::test]
    async fn unrelated_page_with_matching_send_button_cannot_serve_as_chatgpt() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto(
            "data:text/html,<html><body><textarea id='prompt'>draft</textarea><button id='send'>Send</button></body></html>",
        )
        .await
        .unwrap();
        let error = super::require_submission_provider_route("chatgpt", &page).unwrap_err();
        assert!(error.to_string().contains("chat origin unavailable"));
    }

    #[test]
    fn ignores_urls_with_provider_names_only_in_queries_or_subdomains() {
        for url in [
            "https://example.test/?next=https%3A%2F%2Fgrok.com",
            "https://grok.com.attacker.invalid/",
            "https://chatgpt.com.attacker.invalid/",
            "https://x.com/home?q=%2Fi%2Fgrok",
            "https://x.com/i/grokish",
            "https://claude.ai/",
            "data:text/html,<div>grok.com</div>",
        ] {
            assert!(
                !requires_submission_acknowledgement(url),
                "incorrect provider acknowledgement guard for {url}"
            );
        }
    }
}

/// Wait for the input, type the prompt, click send (v1 send steps 1-3;
/// the response stream is polled by providers::run_send).
pub async fn send_prompt(
    provider: &str,
    page: &mut Page,
    set: &EffectiveSelectorSet,
    prompt: &str,
) -> anyhow::Result<()> {
    let initial_url = page.url();
    check_session(provider, page)?;
    require_page_provider_origin(provider, page)?;
    #[cfg(test)]
    let send_guard_provider = if is_provider_test_fixture(page) {
        None
    } else {
        Some(provider)
    };
    #[cfg(not(test))]
    let send_guard_provider = Some(provider);
    let verify_submission = requires_submission_acknowledgement(&initial_url);
    let expected_submission_provider = submission_provider_for_url(&initial_url);
    let inputs = set.input_refs();
    wait_for_selector(page, &inputs, 15_000)
        .await
        .inspect_err(|_| {
            tracing::warn!(
                provider = provider,
                stage = "composer_wait",
                outcome = "timeout_or_error",
                "provider composer diagnostic"
            );
        })?;
    // Waiting can redirect into a *different recognized provider* that
    // exposes a similar editor. Never type the requested provider's prompt
    // into another provider's composer.
    check_session(provider, page)?;
    require_page_provider_origin(provider, page)?;
    // A search/dialog editor can coexist with a thread editor. Never pick
    // whichever is first without a source-grounded thread-scoped selector.
    if provider == "perplexity" {
        require_unique_actionable_composer(page, &inputs)?;
    }
    // Waiting for the composer can navigate to a foreign site containing an
    // equally generic input. Verify the initial provider before typing.
    if let Some(provider) = expected_submission_provider {
        require_submission_provider_route(provider, page)?;
    }
    if let Err(error) = human_type_with_provider(page, &inputs, prompt, send_guard_provider).await {
        tracing::warn!(
            provider = provider,
            stage = "input_dispatch",
            outcome = "failed",
            "provider composer diagnostic"
        );
        return Err(error);
    }
    drive_page(page, Duration::from_millis(200)).await;
    check_session(provider, page)?;
    require_page_provider_origin(provider, page)?;
    if provider == "perplexity" {
        require_unique_actionable_composer(page, &inputs)?;
    }
    let input_sel = inputs
        .iter()
        .copied()
        .find(|s| visible_composer_exists(page, &[*s]))
        .unwrap_or(inputs[0]);
    // The provider may re-render or clear the composer while the browser is
    // settling after typing. Never click Send with a missing or changed draft.
    if !composer_retains_prompt(page, input_sel, prompt)? {
        tracing::warn!(
            provider = provider,
            stage = "framework_render",
            outcome = "draft_not_retained",
            "provider composer diagnostic"
        );
        return Err(anyhow::Error::new(ProviderComposerInputRejectedError));
    }
    check_session(provider, page)?;
    require_page_provider_origin(provider, page)?;
    if let Some(provider) = expected_submission_provider {
        require_submission_provider_route(provider, page)?;
    }
    if provider == "perplexity" {
        require_unique_actionable_composer(page, &inputs)?;
    }
    let sends = set.send_refs();
    // The click/Enter dispatch is not proof that a controlled SPA accepted
    // the prompt. ChatGPT, Grok, Qwen and Manus require a visible transition.
    let baseline = if verify_submission {
        Some(submission_snapshot(page, input_sel, &set.response_refs())?)
    } else {
        None
    };
    if verify_submission {
        click_send_or_enter_checked_with_policy(
            page,
            &sends,
            input_sel,
            5_000,
            Some(prompt),
            false,
            send_guard_provider,
        )
        .await
        .inspect_err(|_| {
            tracing::warn!(
                provider = provider,
                stage = "send_control",
                outcome = "unavailable_or_rejected",
                "provider composer diagnostic"
            );
        })?;
    } else if provider == "perplexity" {
        click_send_or_enter_checked_with_unique_composer(
            page,
            &sends,
            input_sel,
            5_000,
            Some(prompt),
            SendClickPolicy {
                allow_untrusted_enter: true,
                expected_provider: send_guard_provider,
                unique_input_selectors: Some(&inputs),
            },
        )
        .await?;
    } else {
        click_send_or_enter_checked_with_policy(
            page,
            &sends,
            input_sel,
            5_000,
            Some(prompt),
            true,
            send_guard_provider,
        )
        .await?;
    }
    if let Some(before) = baseline {
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            drive_page(page, Duration::from_millis(150)).await;
            if submission_provider_for_url(&page.url()) != expected_submission_provider {
                check_session("browser", page)?;
                anyhow::bail!(
                    "provider left its chat origin before prompt submission was acknowledged"
                );
            }
            let after = submission_snapshot(page, input_sel, &set.response_refs())?;
            if submission_observed_for_adapter(provider, page, &before, &after, prompt)? {
                tracing::debug!(
                    provider = provider,
                    stage = "submission",
                    outcome = "acknowledged",
                    "provider composer diagnostic"
                );
                return Ok(());
            }
        }
        let error = ProviderSubmissionUnacknowledgedError;
        tracing::warn!(
            provider = provider,
            stage = error.phase().as_str(),
            outcome = "not_acknowledged",
            "provider composer diagnostic"
        );
        return Err(anyhow::Error::new(error));
    }
    Ok(())
}

/// Capture only DOM transition signals; no prompt content is logged.
fn submission_snapshot(
    page: &Page,
    input_selector: &str,
    response_selectors: &[&str],
) -> anyhow::Result<SubmissionSnapshot> {
    let input = serde_json::to_string(input_selector)?;
    let input_expr = visible_input_query(&input);
    let draft = composer_draft_expression("el");
    let responses = serde_json::to_string(response_selectors)?;
    let js = format!(
        r#"(() => {{
            const el = {input_expr};
            const value = el ? {draft} : null;
            const responses = {responses};
            const visible = node => {{
                try {{
                    return node.getClientRects().length > 0 &&
                        window.getComputedStyle(node).visibility !== "hidden" &&
                        !node.closest('[hidden], [inert], [aria-hidden="true"]') &&
                        !node.disabled && !node.matches(':disabled') &&
                        !node.closest('fieldset[disabled], [aria-disabled="true"]');
                }} catch (_) {{ return false; }}
            }};
            const nodes = new Set();
            for (const selector of responses) {{
                try {{ document.querySelectorAll(selector).forEach(node => {{ if (visible(node)) nodes.add(node); }}); }} catch (_) {{}}
            }}
            // Clear-on-click alone can happen when a controlled SPA drops an
            // unsubmitted draft. A newly rendered visible user turn, unlike
            // an empty composer, is positive evidence of application uptake.
            const userTurns = new Set();
            for (const selector of [
                '[data-message-author-role="user"]',
                '[data-testid="user-message"]',
                '[data-testid="message-user"]'
            ]) {{
                try {{ document.querySelectorAll(selector).forEach(node => {{
                    if (visible(node)) userTurns.add(node);
                }}); }} catch (_) {{}}
            }}
            // Visible response nodes and actionable generation controls
            // are additional evidence of a submission transition.
            const streaming = Array.from(document.querySelectorAll('[data-is-streaming="true"]')).some(visible) ||
                Array.from(document.querySelectorAll("button")).some(button => visible(button) &&
                    /^(stop|stop generating|stop generation|stop responding|stop response|stop answering)$/i.test(
                        (button.getAttribute("aria-label") || button.innerText || button.textContent || "").trim()
                    ));
            return {{ inputPresent: !!el, inputEmpty: value !== null && !value.trim(), responseCount: nodes.size,
                userTurnCount: userTurns.size, streaming }};
        }})()"#
    );
    let v = page.evaluate(&js);
    Ok(SubmissionSnapshot {
        input_present: v
            .get("inputPresent")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        input_empty: v
            .get("inputEmpty")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        // Obscura's JS bridge may represent integer DOM counts as JSON floats.
        // Failing to decode 1.0 hid a new assistant node and caused false
        // submission timeouts for controlled composers that retain their text.
        response_count: v
            .get("responseCount")
            .and_then(decode_dom_count)
            .unwrap_or(0),
        user_turn_count: v
            .get("userTurnCount")
            .and_then(decode_dom_count)
            .unwrap_or(0),
        streaming: v
            .get("streaming")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
    })
}

/// For Qwen and Manus, a response-node increase plus an emptied composer can
/// happen when history hydrates over a dropped draft. Require a fresh
/// prompt-bound user turn or a newly activated generation indicator instead.
/// This intentionally fails closed until signed-in DOM is evidenced.
fn submission_observed_for_adapter(
    provider: &str,
    page: &Page,
    before: &SubmissionSnapshot,
    after: &SubmissionSnapshot,
    prompt: &str,
) -> anyhow::Result<bool> {
    if matches!(provider, "qwen" | "manus")
        && after.user_turn_count <= before.user_turn_count
        && !(after.streaming && !before.streaming)
    {
        return Ok(false);
    }
    submission_observed_for_prompt(page, before, after, prompt)
}

/// A new user-turn node may come from unrelated SPA history hydration. Only
/// accept a user-turn-only acknowledgement when the *last visible turn* shows
/// this prompt. The comparison stays in the page; no prompt text is returned
/// to the caller or written to logs. New answer/generation transitions retain
/// their separate acknowledgement paths.
fn submission_observed_for_prompt(
    page: &Page,
    before: &SubmissionSnapshot,
    after: &SubmissionSnapshot,
    prompt: &str,
) -> anyhow::Result<bool> {
    if !submission_accepted(before, after) {
        return Ok(false);
    }
    let response_transition =
        after.response_count > before.response_count || (after.streaming && !before.streaming);
    // An unrelated history turn can arrive alongside newly hydrated answer
    // nodes. When a new user turn exists, it must belong to this prompt even
    // if a response/generation transition was also observed.
    if after.user_turn_count <= before.user_turn_count {
        // History hydration can insert a visible assistant node while a
        // framework-controlled editor still holds the unsent draft. Without
        // a new user turn we need evidence the draft was consumed, or that
        // the provider entered *new* active generation. A response count
        // increase alone must not acknowledge an unrelated historical reply.
        let draft_consumed = !before.input_empty && (!after.input_present || after.input_empty);
        let generation_started = after.streaming && !before.streaming;
        return Ok(response_transition && (draft_consumed || generation_started));
    }
    let user_turn_transition = !before.input_empty && (!after.input_present || after.input_empty);
    if !user_turn_transition && !response_transition {
        return Ok(false);
    }
    let prompt_js = serde_json::to_string(prompt)?;
    let matched = page.evaluate(&format!(
        r#"(() => {{
            const expected = {prompt_js};
            // Whitespace collapse can acknowledge a DIFFERENT prompt: a
            // multiline rich-text request is not the same as one line.
            // Only canonicalize transport CRLF/CR, preserving all other
            // intentional Unicode, spaces, indentation and line breaks.
            const canonical = text => (text || '').replace(/\r\n?/g, '\n');
            if (!expected || !expected.trim()) return false;
            const turns = Array.from(document.querySelectorAll(
                '[data-message-author-role="user"], [data-testid="user-message"], [data-testid="message-user"]'
            )).filter(node => {{
                try {{
                    return node.getClientRects().length > 0 &&
                        window.getComputedStyle(node).visibility !== 'hidden' &&
                        !node.closest('[hidden], [inert], [aria-hidden="true"], [aria-disabled="true"]');
                }} catch (_) {{ return false; }}
            }});
            if (!turns.length) return false;
            const last = turns[turns.length - 1];
            return canonical(last.innerText || last.textContent || '') === canonical(expected);
        }})()"#
    ));
    Ok(matched.as_bool() == Some(true))
}

// The Obscura JS bridge can encode a whole DOM count as a JSON float.
fn decode_dom_count(value: &serde_json::Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0 && *n < u64::MAX as f64 && n.fract() == 0.0)
            .map(|n| n as u64)
    })
}

#[derive(Debug, Clone, Copy)]
struct SubmissionSnapshot {
    input_present: bool,
    input_empty: bool,
    response_count: u64,
    user_turn_count: u64,
    streaming: bool,
}

fn submission_accepted(before: &SubmissionSnapshot, after: &SubmissionSnapshot) -> bool {
    // A cleared draft alone is not proof that the provider received a turn.
    // A newly visible user turn plus a cleared/unmounted composer is stronger
    // evidence than either transition in isolation.
    let user_turn_ack = after.user_turn_count > before.user_turn_count
        && !before.input_empty
        && (!after.input_present || after.input_empty);
    user_turn_ack
        || after.response_count > before.response_count
        || (after.streaming && !before.streaming)
}

#[cfg(test)]
mod pre_send_retention_tests {
    #[tokio::test]
    async fn late_framework_clear_after_typing_does_not_click_send() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><form>
            <textarea id="prompt"></textarea>
            <button id="send" type="button" onclick="window.sent=(window.sent||0)+1">Send</button>
            </form><script>
            document.querySelector('#prompt').addEventListener('input', () => {
                setTimeout(() => { document.querySelector('#prompt').value = ''; }, 220);
            });
            </script></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let overrides = crate::config::ProviderSelectorOverrides {
            input: Some(vec!["#prompt".to_owned()]),
            send: Some(vec!["#send".to_owned()]),
            ..Default::default()
        };
        let selectors = crate::providers::chatgpt::SELECTORS.effective(&overrides);
        page.evaluate(
            "document.documentElement.setAttribute('data-chat-gateway-test-fixture', '')",
        );
        let result = super::send_prompt("chatgpt", &mut page, &selectors, "user draft").await;
        assert!(result.is_err(), "a cleared draft must not be submitted");
        assert_eq!(page.evaluate("window.sent || 0").as_f64(), Some(0.0));
    }
}

#[cfg(test)]
mod composer_rejected_input_tests {
    use super::ProviderComposerInputRejectedError;

    #[tokio::test]
    async fn framework_draft_rejection_is_typed_and_contains_no_prompt() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><textarea id="prompt"></textarea>
            <script>
              document.querySelector('#prompt').addEventListener('input', () => {
                  document.querySelector('#prompt').value = '';
              });
            </script></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let error = super::human_type(&page, &["#prompt"], "private-message")
            .await
            .unwrap_err();
        assert!(error
            .downcast_ref::<ProviderComposerInputRejectedError>()
            .is_some());
        assert!(!error.to_string().contains("private-message"));
        assert_eq!(
            error.to_string(),
            "provider input did not retain the prompt after framework render"
        );
    }

    #[tokio::test]
    async fn synthetic_value_spoof_on_nested_contenteditable_never_authorizes_send() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <form>
              <div id="target" role="textbox" contenteditable="true"><p>Old draft</p></div>
              <button id="send" type="button" onclick="window.sendCount=(window.sendCount||0)+1">Send</button>
            </form>
            <div id="other" contenteditable="true">Unrelated draft</div>
            <div id="empty" contenteditable="true"></div>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        page.evaluate(r#"document.querySelector('#target').value = 'private message';"#);
        assert_eq!(
            page.evaluate("document.querySelector('#target').value")
                .as_str(),
            Some("private message")
        );
        assert!(!super::composer_retains_prompt(&page, "#target", "private message").unwrap());
        page.evaluate("document.querySelector('#empty').value = 'private message'");
        let snapshot = super::submission_snapshot(&page, "#empty", &[".assistant"]).unwrap();
        assert!(
            snapshot.input_empty,
            "synthetic value on an empty contenteditable must not count as a real draft"
        );
        let rejected_click = super::click_send_or_enter_checked_with_policy(
            &mut page,
            &["#send"],
            "#target",
            250,
            Some("private message"),
            false,
            None,
        )
        .await;
        assert!(
            rejected_click.is_err(),
            "last-moment Send check must reject synthetic value"
        );
        assert_eq!(page.evaluate("window.sendCount || 0").as_f64(), Some(0.0));
        let error = super::human_type(&page, &["#target"], "private message")
            .await
            .unwrap_err();
        assert!(error
            .downcast_ref::<ProviderComposerInputRejectedError>()
            .is_some());
        assert!(!error.to_string().contains("private message"));
        assert_eq!(page.evaluate("window.sendCount || 0").as_f64(), Some(0.0));
        assert_eq!(
            page.evaluate("document.querySelector('#other').textContent")
                .as_str(),
            Some("Unrelated draft")
        );
    }

    #[test]
    fn read_only_draft_contract_never_uses_synthetic_value_for_contenteditable() {
        let expression = super::composer_draft_expression("input");
        assert!(expression.contains("input.tagName === 'TEXTAREA'"));
        assert!(expression.contains("input.tagName === 'INPUT'"));
        assert!(expression.contains("input.innerText ?? input.textContent"));
        assert!(!expression.contains("'value' in"));
    }

    #[tokio::test]
    async fn controlled_contenteditable_draft_with_disabled_send_is_not_submitted() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <div id="prompt" contenteditable="true"></div>
            <button id="send" disabled>Send</button>
            <script>
                const editor = document.querySelector('#prompt');
                // The embedded browser's execCommand path may not fire an
                // input event. A controlled editor can also restore its
                // canonical empty document on a later framework render.
                setInterval(() => {
                    editor.textContent = '';
                    document.querySelector('#send').disabled = true;
                }, 10);
            </script></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        // The pinned embedded browser may retain a synthetic DOM draft even
        // when the controlled editor never enables submission. DOM retention
        // is not proof that the framework accepted the prompt.
        let error = super::human_type(&page, &["#prompt"], "private-message")
            .await
            .unwrap_err();
        assert!(error
            .downcast_ref::<ProviderComposerInputRejectedError>()
            .is_some());
        assert!(!error.to_string().contains("private-message"));
        assert_eq!(
            page.evaluate("document.querySelector('#send').disabled")
                .as_bool(),
            Some(true)
        );
        // The data: fixture is intentionally not a ChatGPT origin. Exercise
        // the strict ChatGPT/Grok send policy directly: send_prompt only
        // enables that policy on an authenticated provider route.
        let result = super::click_send_or_enter_checked_with_policy(
            &mut page,
            &["#send"],
            "#prompt",
            250,
            Some("private-message"),
            false,
            None,
        )
        .await;
        assert!(
            result.is_err(),
            "disabled Send must never acknowledge a prompt"
        );
        assert!(!result.unwrap_err().to_string().contains("private-message"));
    }
}

#[cfg(test)]
mod native_input_setter_tests {
    #[tokio::test]
    async fn html_input_composer_uses_native_value_setter_for_controlled_frameworks() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body><input id='prompt' type='text'></body></html>")
            .await
            .unwrap();
        // Simulate a framework-installed own-property value setter. Direct
        // assignment can fool its value tracker into discarding a synthetic
        // input event; the native prototype setter must bypass that trap.
        page.evaluate(
            r#"(() => {
                const el = document.querySelector('#prompt');
                const native = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value');
                window.directWrites = 0;
                window.inputEvents = 0;
                Object.defineProperty(el, 'value', {
                    configurable: true,
                    get() { return native.get.call(this); },
                    set(value) {
                        window.directWrites++;
                        native.set.call(this, value);
                    }
                });
                // Some webviews do not support execCommand('insertText').
                Object.defineProperty(document, 'execCommand', {
                    configurable: true, value: () => false
                });
                el.addEventListener('input', () => {
                    window.inputEvents++;
                    window.lastInput = el.value;
                });
                return true;
            })()"#,
        );
        super::human_type(&page, &["#prompt"], "input draft")
            .await
            .unwrap();
        assert_eq!(
            page.evaluate("window.lastInput").as_str(),
            Some("input draft")
        );
        assert_eq!(page.evaluate("window.directWrites").as_f64(), Some(0.0));
        assert_eq!(page.evaluate("window.inputEvents").as_f64(), Some(1.0));
    }
}

#[cfg(test)]
mod prompt_bound_submission_tests {
    use super::{
        submission_observed_for_adapter, submission_observed_for_prompt, submission_snapshot,
    };

    #[tokio::test]
    async fn unrelated_hydrated_turn_cannot_acknowledge_a_cleared_draft() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <textarea id="prompt">fixture request</textarea>
            <div data-message-author-role="user">older turn</div>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        let before = submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        page.evaluate(
            r#"(() => {
                document.querySelector('#prompt').value = '';
                document.body.insertAdjacentHTML('beforeend',
                    '<div data-message-author-role="user">unrelated history load</div>');
                return true;
            })()"#,
        );
        let after = submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(super::submission_accepted(&before, &after));
        assert!(
            !submission_observed_for_prompt(&page, &before, &after, "fixture request").unwrap()
        );

        // A matching latest visible user message is accepted only after the
        // composer transition and a new user turn.
        page.evaluate("document.body.lastElementChild.textContent = 'fixture request'");
        assert!(submission_observed_for_prompt(&page, &before, &after, "fixture request").unwrap());

        // An unrelated last visible message must defeat an older matching turn.
        page.evaluate(
            "document.body.insertAdjacentHTML('beforeend', '<div data-testid=\"user-message\">other request</div>')",
        );
        assert!(
            !submission_observed_for_prompt(&page, &before, &after, "fixture request").unwrap()
        );
    }

    #[tokio::test]
    async fn user_turn_evidence_preserves_multiline_unicode_and_significant_spacing() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <textarea id="prompt">original request</textarea>
            <div data-message-author-role="user">older turn</div>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let before = submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        page.evaluate(
            r#"(() => {
                document.querySelector('#prompt').value = '';
                const turn = document.createElement('div');
                turn.setAttribute('data-message-author-role', 'user');
                turn.textContent = 'line one line two';
                document.body.appendChild(turn);
            })()"#,
        );
        let after = submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(after.user_turn_count > before.user_turn_count);
        assert!(!submission_observed_for_prompt(&page, &before, &after, "line one\nline two").unwrap());

        // A framework-rendered actual multiline turn can acknowledge it.
        page.evaluate(
            "document.body.lastElementChild.textContent = 'line one\\nline two'",
        );
        assert!(submission_observed_for_prompt(&page, &before, &after, "line one\nline two").unwrap());
        assert!(!submission_observed_for_prompt(&page, &before, &after, "line one line two").unwrap());
        assert!(!submission_observed_for_prompt(&page, &before, &after, "line one  line two").unwrap());
        assert!(!submission_observed_for_prompt(&page, &before, &after, "line one\n line two").unwrap());

        // CRLF is only a transport line ending; the actual significant
        // newline, emoji and non-ASCII characters remain exact.
        page.evaluate(
            "document.body.lastElementChild.textContent = '你好\\n\\nemoji ☃️'",
        );
        assert!(submission_observed_for_prompt(&page, &before, &after, "你好\r\n\r\nemoji ☃️").unwrap());
        assert!(!submission_observed_for_prompt(&page, &before, &after, "你好 emoji ☃️").unwrap());
        assert!(!submission_observed_for_prompt(&page, &before, &after, "你好\n\nemoji ☃").unwrap());
    }

    #[tokio::test]
    async fn an_unrelated_new_user_turn_blocks_even_a_new_response_node() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <textarea id="prompt">expected prompt</textarea>
            <div data-message-author-role="user">old prompt</div>
            <div class="answer">old reply</div>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        let before = submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        page.evaluate(
            r#"(() => {
                document.querySelector('#prompt').value = '';
                document.body.insertAdjacentHTML('beforeend',
                    '<div data-message-author-role="user">unrelated hydration</div><div class="answer">hydrated response</div>');
                return true;
            })()"#,
        );
        let after = submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(after.response_count > before.response_count);
        assert!(after.user_turn_count > before.user_turn_count);
        assert!(
            !submission_observed_for_prompt(&page, &before, &after, "expected prompt").unwrap()
        );
        page.evaluate(
            "document.querySelectorAll('[data-message-author-role=\"user\"]')[1].textContent = 'expected prompt'",
        );
        assert!(submission_observed_for_prompt(&page, &before, &after, "expected prompt").unwrap());
    }

    #[tokio::test]
    async fn hydrated_response_without_draft_consumption_cannot_acknowledge_send() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto(
            "data:text/html,<html><body><textarea id='prompt'>draft</textarea></body></html>",
        )
        .await
        .unwrap();
        let before = submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        // A background history load is not evidence that a click submitted
        // this unchanged draft, even when a new assistant DOM node appears.
        page.evaluate(
            "document.body.insertAdjacentHTML('beforeend', '<div class=\"answer\">old reply</div>')",
        );
        let hydrated = submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(super::submission_accepted(&before, &hydrated));
        assert!(!submission_observed_for_prompt(&page, &before, &hydrated, "draft").unwrap());

        // If the composer clears in addition to a new response, the provider
        // has a second observable submission transition.
        page.evaluate("document.querySelector('#prompt').value = ''");
        let consumed = submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(submission_observed_for_prompt(&page, &before, &consumed, "draft").unwrap());
    }

    #[tokio::test]
    async fn qwen_style_send_ack_rejects_unchanged_draft_and_history_hydration() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <textarea id="qwen-prompt">fresh request</textarea>
            <div data-testid="user-message">old request</div>
            <div class="answer">old answer</div>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let before = submission_snapshot(&page, "#qwen-prompt", &[".answer"]).unwrap();
        assert!(
            !submission_observed_for_adapter("qwen", &page, &before, &before, "fresh request")
                .unwrap()
        );

        // History hydration while the draft remains cannot certify that an
        // unrelated completed answer belongs to the current request.
        page.evaluate(r#"document.body.insertAdjacentHTML('beforeend', '<div class="answer">hydrated old answer</div>')"#);
        let hydrated = submission_snapshot(&page, "#qwen-prompt", &[".answer"]).unwrap();
        assert!(!submission_observed_for_adapter(
            "qwen",
            &page,
            &before,
            &hydrated,
            "fresh request"
        )
        .unwrap());

        // Even if the composer drops its value while historical content
        // hydrates, Qwen has no prompt-correlated acceptance signal yet.
        page.evaluate("document.querySelector('#qwen-prompt').value = ''");
        let dropped = submission_snapshot(&page, "#qwen-prompt", &[".answer"]).unwrap();
        assert!(!submission_observed_for_adapter(
            "qwen",
            &page,
            &before,
            &dropped,
            "fresh request"
        )
        .unwrap());

        // A dropped draft and the wrong user's turn must not count as success.
        page.evaluate("document.querySelector('#qwen-prompt').value = ''");
        page.evaluate(r#"document.body.insertAdjacentHTML('beforeend', '<div data-testid="user-message">unrelated request</div>')"#);
        let unrelated = submission_snapshot(&page, "#qwen-prompt", &[".answer"]).unwrap();
        assert!(!submission_observed_for_adapter(
            "qwen",
            &page,
            &before,
            &unrelated,
            "fresh request"
        )
        .unwrap());
        page.evaluate("document.body.lastElementChild.textContent = 'fresh request'");
        assert!(submission_observed_for_adapter(
            "qwen",
            &page,
            &before,
            &unrelated,
            "fresh request"
        )
        .unwrap());
    }

    #[tokio::test]
    async fn manus_chat_ack_requires_current_prompt_not_history_hydration() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <textarea id="manus">fresh chat</textarea>
            <div class="answer">old output</div>
            <div data-testid="user-message">old turn</div>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let before = submission_snapshot(&page, "#manus", &[".answer"]).unwrap();
        page.evaluate(r#"document.body.insertAdjacentHTML('beforeend', '<div class="answer">hydrated output</div>')"#);
        page.evaluate("document.querySelector('#manus').value = ''");
        let hydrated = submission_snapshot(&page, "#manus", &[".answer"]).unwrap();
        assert!(
            !submission_observed_for_adapter("manus", &page, &before, &hydrated, "fresh chat")
                .unwrap()
        );

        page.evaluate(r#"document.body.insertAdjacentHTML('beforeend', '<div data-testid="user-message">unrelated</div>')"#);
        let wrong = submission_snapshot(&page, "#manus", &[".answer"]).unwrap();
        assert!(
            !submission_observed_for_adapter("manus", &page, &before, &wrong, "fresh chat")
                .unwrap()
        );
        page.evaluate("document.body.lastElementChild.textContent = 'fresh chat'");
        assert!(
            submission_observed_for_adapter("manus", &page, &before, &wrong, "fresh chat").unwrap()
        );
    }

    #[tokio::test]
    async fn newly_started_generation_acknowledges_without_clearing_draft() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto(
            "data:text/html,<html><body><textarea id='prompt'>draft</textarea></body></html>",
        )
        .await
        .unwrap();
        let before = submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        page.evaluate(
            "document.body.insertAdjacentHTML('beforeend', '<button aria-label=\"Stop generating\">Stop</button>')",
        );
        let after = submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(after.streaming && !before.streaming);
        assert!(submission_observed_for_prompt(&page, &before, &after, "draft").unwrap());
    }
}

#[cfg(test)]
mod submission_transition_tests {
    use super::{submission_accepted, SubmissionSnapshot};

    #[test]
    fn dispatch_without_state_change_is_not_submission() {
        let before = SubmissionSnapshot {
            input_present: true,
            input_empty: false,
            response_count: 2,
            user_turn_count: 1,
            streaming: false,
        };
        assert!(!submission_accepted(&before, &before));
        assert!(!submission_accepted(
            &before,
            &SubmissionSnapshot {
                input_present: false,
                ..before
            }
        ));
        assert!(!submission_accepted(
            &before,
            &SubmissionSnapshot {
                input_empty: true,
                ..before
            }
        ));
        assert!(submission_accepted(
            &before,
            &SubmissionSnapshot {
                user_turn_count: 2,
                input_empty: true,
                ..before
            }
        ));
        assert!(submission_accepted(
            &before,
            &SubmissionSnapshot {
                response_count: 3,
                ..before
            }
        ));
        assert!(submission_accepted(
            &before,
            &SubmissionSnapshot {
                streaming: true,
                ..before
            }
        ));
    }

    #[tokio::test]
    async fn cleared_draft_without_new_visible_user_turn_does_not_acknowledge_submission() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><form>
            <textarea id="prompt">draft</textarea>
            <button id="send" type="button" onclick="document.querySelector('#prompt').value=''">Send</button>
            <div data-message-author-role="user">previous turn</div>
        </form></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        let before = super::submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert_eq!(before.user_turn_count, 1);
        assert!(
            super::click_send_or_enter(&mut page, &["#send"], "#prompt", 400)
                .await
                .unwrap()
        );
        let cleared = super::submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(cleared.input_empty);
        assert!(!super::submission_accepted(&before, &cleared));

        page.evaluate("document.body.insertAdjacentHTML('beforeend', '<div data-message-author-role=\\\"user\\\" hidden>new turn</div>')");
        let hidden = super::submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(!super::submission_accepted(&before, &hidden));

        page.evaluate(
            "document.querySelector('[data-message-author-role=user][hidden]').hidden = false",
        );
        let accepted = super::submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert_eq!(accepted.user_turn_count, 2);
        assert!(super::submission_accepted(&before, &accepted));
    }

    #[tokio::test]
    async fn mobile_form_submission_is_acknowledged_by_new_response_node() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body><form id='chat'><textarea id='mobile'>draft</textarea><button type='submit'>Send</button></form><script>document.querySelector('#chat').addEventListener('submit',function(e){e.preventDefault();const response=document.createElement('div');response.className='answer';response.textContent='OK';document.body.appendChild(response);});</script></body></html>")
            .await
            .unwrap();
        let before = super::submission_snapshot(&page, "#mobile", &[".answer"]).unwrap();
        assert_eq!(before.response_count, 0);
        assert!(!before.input_empty);
        assert!(
            super::click_send_or_enter(&mut page, &["button[type='submit']"], "#mobile", 400)
                .await
                .unwrap()
        );
        let after = super::submission_snapshot(&page, "#mobile", &[".answer"]).unwrap();
        assert_eq!(
            after.response_count, 1,
            "Obscura may decode DOM counts as 1.0"
        );
        assert!(!after.input_empty, "the fixture intentionally retains text");
        assert!(super::submission_accepted(&before, &after));
    }

    #[tokio::test]
    async fn selects_visible_enabled_send_when_earlier_matches_are_hidden_or_disabled() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><form><textarea id="prompt">draft</textarea><button class="send" type="button" style="display:none" onclick="window.hiddenClicks=(window.hiddenClicks||0)+1">hidden</button><button class="send" type="button" disabled>disabled</button><button class="send" type="button" onclick="window.visibleClicks=(window.visibleClicks||0)+1">send</button></form></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        assert!(
            super::click_send_or_enter(&mut page, &["button.send"], "#prompt", 400)
                .await
                .unwrap(),
            "a visible enabled send must be selected before Enter fallback"
        );
        assert_eq!(
            page.evaluate("window.hiddenClicks || 0").as_f64(),
            Some(0.0)
        );
        assert_eq!(
            page.evaluate("window.visibleClicks || 0").as_f64(),
            Some(1.0)
        );
    }

    #[tokio::test]
    async fn deeply_nested_composer_finds_form_send_without_synthetic_enter() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        // The send control is a sibling of eight nested editor wrappers.
        let wrappers = "<div>".repeat(8);
        let closers = "</div>".repeat(8);
        let html = format!(
            "<html><body><form><button type='button' id='send' onclick='window.sent=(window.sent||0)+1'>Send</button>{wrappers}<textarea id='prompt'>message</textarea>{closers}</form><script>window.entered=0;document.querySelector('#prompt').addEventListener('keydown',()=>window.entered++)</script></body></html>"
        );
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        assert!(
            super::click_send_or_enter(&mut page, &["#send"], "#prompt", 450)
                .await
                .unwrap(),
            "deep composer must click its actual send control"
        );
        assert_eq!(page.evaluate("window.sent || 0").as_f64(), Some(1.0));
        assert_eq!(page.evaluate("window.entered || 0").as_f64(), Some(0.0));
    }

    #[tokio::test]
    async fn hidden_or_unrelated_stop_control_does_not_acknowledge_submission() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><form><textarea id="prompt">draft</textarea><button id="stop" aria-label="Stop generating" style="display:none">Stop</button><button aria-label="Stop sharing">Stop sharing</button><span data-is-streaming="true" hidden></span></form></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        let baseline = super::submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(!baseline.streaming);
        assert!(!baseline.input_empty);
        // Simulate the SPA inserting an unrelated Stop control after click:
        // without a new assistant node this must still not count as an ack.
        page.evaluate(
            "document.querySelector('[aria-label=\"Stop sharing\"]').setAttribute('data-test', 'unchanged')",
        );
        let unrelated = super::submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(!super::submission_accepted(&baseline, &unrelated));

        // A genuinely visible generation Stop is an observable transition.
        page.evaluate("document.querySelector('#stop').style.display = 'block'");
        let active = super::submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(active.streaming);
        assert!(super::submission_accepted(&baseline, &active));
    }

    #[tokio::test]
    async fn disabled_stop_cannot_acknowledge_unchanged_composer() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><form>
            <textarea id="prompt">draft</textarea>
            <fieldset disabled><button id="stop" aria-label="Stop generating">Stop</button></fieldset>
            <span aria-disabled="true"><span data-is-streaming="true"></span></span>
        </form></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let before = super::submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(!before.streaming);
        page.evaluate("document.querySelector('fieldset').setAttribute('data-changed', 'yes')");
        let after = super::submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(!super::submission_accepted(&before, &after));
        page.evaluate("document.querySelector('fieldset').disabled = false");
        let streaming = super::submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(super::submission_accepted(&before, &streaming));
    }

    #[tokio::test]
    async fn hidden_response_node_does_not_acknowledge_an_unchanged_draft() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><textarea id="prompt">draft</textarea><div class="answer">old</div></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        let before = super::submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert_eq!(before.response_count, 1);
        page.evaluate("document.body.insertAdjacentHTML('beforeend', '<div class=\"answer\" hidden>old</div>')");
        let hidden = super::submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(!super::submission_accepted(&before, &hidden));
        page.evaluate("document.querySelector('.answer[hidden]').hidden = false");
        let visible = super::submission_snapshot(&page, "#prompt", &[".answer"]).unwrap();
        assert!(super::submission_accepted(&before, &visible));
    }

    #[test]
    fn numeric_dom_counts_reject_nan_and_fractional_values() {
        assert_eq!(super::decode_dom_count(&serde_json::json!(3.0)), Some(3));
        assert_eq!(super::decode_dom_count(&serde_json::json!(3)), Some(3));
        assert_eq!(super::decode_dom_count(&serde_json::json!(3.25)), None);
        assert_eq!(super::decode_dom_count(&serde_json::json!(-1.0)), None);
        assert_eq!(super::decode_dom_count(&serde_json::json!("3")), None);
    }

    #[test]
    fn preexisting_streaming_is_not_new_acknowledgement() {
        let before = SubmissionSnapshot {
            input_present: true,
            input_empty: false,
            response_count: 1,
            user_turn_count: 0,
            streaming: true,
        };
        assert!(!submission_accepted(&before, &before));
    }
}

#[cfg(test)]
mod text_selector_tests {
    use super::text_selector_parts;

    #[tokio::test]
    async fn text_click_skips_hidden_and_disabled_send_controls() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <button hidden onclick="window.clicked='hidden'">Send</button>
            <button disabled onclick="window.clicked='disabled'">Send</button>
            <div aria-disabled="true"><button onclick="window.clicked='ancestor'">Send</button></div>
            <button id="valid" onclick="window.clicked='valid'">Send</button>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        assert!(super::click_text_selector(&page, "text:button=Send").unwrap());
        assert_eq!(page.evaluate("window.clicked").as_str(), Some("valid"));
        page.evaluate("document.querySelector('#valid').disabled = true");
        assert!(!super::click_text_selector(&page, "text:button=Send").unwrap());
    }

    #[test]
    fn selector_override_is_role_specific_and_preserves_defaults() {
        let base = &crate::providers::chatgpt::SELECTORS;
        let override_cfg = crate::config::ProviderSelectorOverrides {
            input: Some(vec!["textarea[data-test='new-composer']".into()]),
            ..Default::default()
        };
        let effective = base.effective(&override_cfg);
        assert_eq!(
            effective.input_refs(),
            vec!["textarea[data-test='new-composer']"]
        );
        assert_eq!(effective.send_refs(), base.send);
        assert_eq!(effective.response_refs(), base.response);
    }

    #[test]
    fn parses_internal_text_selector() {
        assert_eq!(
            text_selector_parts("text:button=Stop"),
            Some(("button", "Stop"))
        );
        assert_eq!(
            text_selector_parts("text:div=Mistral Large"),
            Some(("div", "Mistral Large"))
        );
    }

    #[test]
    fn rejects_empty_or_css_like_text_selector() {
        assert_eq!(text_selector_parts("text:button="), None);
        assert_eq!(text_selector_parts("text:=Stop"), None);
        assert_eq!(text_selector_parts(r#"button:has-text("Stop")"#), None);
        assert_eq!(text_selector_parts("button[aria-label*=Stop]"), None);
    }

    #[test]
    fn provider_selectors_do_not_use_playwright_only_text_pseudos() {
        let sets = [
            &crate::providers::qwen::SELECTORS,
            &crate::providers::metaai::SELECTORS,
            &crate::providers::claude::SELECTORS,
            &crate::providers::chatgpt::SELECTORS,
            &crate::providers::gemini::SELECTORS,
            &crate::providers::grok::SELECTORS,
            &crate::providers::mistral::SELECTORS,
            &crate::providers::perplexity::SELECTORS,
        ];
        for selectors in sets {
            for selector in selectors
                .picker
                .iter()
                .chain(selectors.input.iter())
                .chain(selectors.send.iter())
                .chain(selectors.response.iter())
            {
                assert!(
                    !selector.contains(":has-text("),
                    "{} still uses a Playwright-only selector",
                    selectors.url
                );
                assert!(
                    !matches!(
                        *selector,
                        "button[type=\"submit\"]"
                            | "button[type=\"submit\"]:has(svg)"
                            | "button:has(svg)"
                            | "button:has(mat-icon)"
                    ),
                    "{} still uses a generic send-button selector",
                    selectors.url
                );
            }
        }
    }
}

#[cfg(test)]
mod browser_event_loop_tests {
    #[tokio::test]
    async fn disabled_send_button_never_dispatches_enter() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body><form><textarea id='prompt'></textarea><button id='send' disabled>Send</button></form><script>window.enterCount=0;document.querySelector('#prompt').addEventListener('keydown',()=>window.enterCount++)</script></body></html>")
            .await
            .unwrap();
        let result = super::click_send_or_enter(&mut page, &["#send"], "#prompt", 200).await;
        assert!(result.is_err());
        assert_eq!(page.evaluate("window.enterCount").as_f64(), Some(0.0));
    }

    #[tokio::test]
    async fn inherited_disabled_send_control_is_not_clicked_or_bypassed_by_enter() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body><form><textarea id='prompt'></textarea><fieldset disabled><button id='send' onclick='window.sent=(window.sent||0)+1'>Send</button></fieldset></form><script>window.enterCount=0;document.querySelector('#prompt').addEventListener('keydown',()=>window.enterCount++)</script></body></html>")
            .await
            .unwrap();
        assert!(
            super::click_send_or_enter(&mut page, &["#send"], "#prompt", 200)
                .await
                .is_err(),
            "a button in a disabled fieldset is not an actionable send control"
        );
        assert_eq!(page.evaluate("window.sent || 0").as_f64(), Some(0.0));
        assert_eq!(page.evaluate("window.enterCount").as_f64(), Some(0.0));
    }

    #[tokio::test]
    async fn hidden_duplicate_input_is_never_typed_or_used_for_send() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><form><textarea class="prompt" style="display:none" onclick="window.hiddenClicks=(window.hiddenClicks||0)+1"></textarea><textarea class="prompt"></textarea><button id="send" type="button" onclick="window.sent=(window.sent||0)+1">Send</button></form></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        super::wait_for_selector(&mut page, &["textarea.prompt"], 500)
            .await
            .unwrap();
        super::human_type(&page, &["textarea.prompt"], "visible message")
            .await
            .unwrap();
        assert_eq!(
            page.evaluate(
                "Array.from(document.querySelectorAll('textarea.prompt')).map(el => el.value)"
            ),
            serde_json::json!(["", "visible message"])
        );
        let baseline = super::submission_snapshot(&page, "textarea.prompt", &[".answer"]).unwrap();
        assert!(
            !baseline.input_empty,
            "read the visible input, not the hidden autosizer"
        );
        assert!(
            super::click_send_or_enter(&mut page, &["#send"], "textarea.prompt", 500)
                .await
                .unwrap(),
            "send must use the visible input's enclosing form"
        );
        assert_eq!(page.evaluate("window.sent || 0").as_f64(), Some(1.0));
        assert_eq!(
            page.evaluate("window.hiddenClicks || 0").as_f64(),
            Some(0.0)
        );
    }

    #[tokio::test]
    async fn hidden_or_disabled_input_does_not_short_circuit_composer_wait() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body><textarea id='hidden' hidden></textarea><textarea id='disabled' disabled></textarea></body></html>")
            .await
            .unwrap();
        assert!(
            super::wait_for_selector(&mut page, &["#hidden", "#disabled"], 200)
                .await
                .is_err(),
            "non-actionable inputs must not count as composer readiness"
        );
        page.evaluate("document.querySelector('#hidden').hidden = false");
        super::wait_for_selector(&mut page, &["#hidden", "#disabled"], 500)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn delayed_composer_is_observed_by_selector_wait() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body><script>globalThis.fixtureLoaded=true</script></body></html>")
            .await
            .unwrap();
        assert_eq!(
            page.evaluate("globalThis.fixtureLoaded"),
            serde_json::json!(true)
        );
        assert_eq!(page.evaluate("(() => { setTimeout(() => { const input = document.createElement('textarea'); input.setAttribute('id', 'delayed'); document.body.appendChild(input); }, 20); return true; })()"), serde_json::json!(true));
        super::wait_for_selector(&mut page, &["#delayed"], 600)
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod idle_browser_wait_tests {
    #[tokio::test]
    async fn idle_browser_pump_retains_the_poll_grace_instead_of_busy_spinning() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<script>globalThis.ready=true</script>")
            .await
            .unwrap();
        let started = std::time::Instant::now();
        super::drive_page(&mut page, std::time::Duration::from_millis(100)).await;
        assert!(started.elapsed() >= std::time::Duration::from_millis(100));
    }
}

#[cfg(test)]
mod discovered_label_tests {
    #[test]
    fn a_model_keyword_in_later_ui_text_does_not_advertise_the_heading_as_a_model() {
        let labels = vec![
            "Where should we start?\nGemini".into(),
            "Gemini 2.0 Flash".into(),
        ];
        assert_eq!(
            super::matching_model_labels(labels, "gemini|flash").unwrap(),
            vec!["Gemini 2.0 Flash"]
        );
    }
}

// Offline DOM contract test: exercise the *actual adapter selectors* against
// synthetic fixtures, not just data-contract markers. These examples do not
// make any claim about the current authenticated provider websites.
#[cfg(test)]
mod provider_fixture_selector_tests {
    use super::SelectorSet;

    fn fixture_url(html: &str) -> String {
        let mut uri = String::from("data:text/html,");
        for byte in html.bytes() {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                uri.push(char::from(byte));
            } else {
                use std::fmt::Write as _;
                write!(uri, "%{byte:02X}").expect("encoding fixture URL");
            }
        }
        uri
    }

    #[tokio::test]
    async fn every_provider_fixture_matches_registered_input_send_and_response_selectors() {
        let fixtures: &[(&str, &SelectorSet, &str, &str, &str)] = &[
            (
                "qwen",
                &crate::providers::qwen::SELECTORS,
                include_str!("../../tests/fixtures/qwen/idle.html"),
                include_str!("../../tests/fixtures/qwen/streaming.html"),
                include_str!("../../tests/fixtures/qwen/done.html"),
            ),
            (
                "metaai",
                &crate::providers::metaai::SELECTORS,
                include_str!("../../tests/fixtures/metaai/idle.html"),
                include_str!("../../tests/fixtures/metaai/streaming.html"),
                include_str!("../../tests/fixtures/metaai/done.html"),
            ),
            (
                "claude",
                &crate::providers::claude::SELECTORS,
                include_str!("../../tests/fixtures/claude/idle.html"),
                include_str!("../../tests/fixtures/claude/streaming.html"),
                include_str!("../../tests/fixtures/claude/done.html"),
            ),
            (
                "chatgpt",
                &crate::providers::chatgpt::SELECTORS,
                include_str!("../../tests/fixtures/chatgpt/idle.html"),
                include_str!("../../tests/fixtures/chatgpt/streaming.html"),
                include_str!("../../tests/fixtures/chatgpt/done.html"),
            ),
            (
                "gemini",
                &crate::providers::gemini::SELECTORS,
                include_str!("../../tests/fixtures/gemini/idle.html"),
                include_str!("../../tests/fixtures/gemini/streaming.html"),
                include_str!("../../tests/fixtures/gemini/done.html"),
            ),
            (
                "grok",
                &crate::providers::grok::SELECTORS,
                include_str!("../../tests/fixtures/grok/idle.html"),
                include_str!("../../tests/fixtures/grok/streaming.html"),
                include_str!("../../tests/fixtures/grok/done.html"),
            ),
            (
                "manus",
                &crate::providers::manus::SELECTORS,
                include_str!("../../tests/fixtures/manus/idle.html"),
                include_str!("../../tests/fixtures/manus/streaming.html"),
                include_str!("../../tests/fixtures/manus/done.html"),
            ),
            (
                "mistral",
                &crate::providers::mistral::SELECTORS,
                include_str!("../../tests/fixtures/mistral/idle.html"),
                include_str!("../../tests/fixtures/mistral/streaming.html"),
                include_str!("../../tests/fixtures/mistral/done.html"),
            ),
            (
                "perplexity",
                &crate::providers::perplexity::SELECTORS,
                include_str!("../../tests/fixtures/perplexity/idle.html"),
                include_str!("../../tests/fixtures/perplexity/streaming.html"),
                include_str!("../../tests/fixtures/perplexity/done.html"),
            ),
        ];
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        for (name, selectors, idle, streaming, done) in fixtures {
            for (state, html, expect_response) in [
                ("idle", *idle, false),
                ("streaming", *streaming, true),
                ("done", *done, true),
            ] {
                page.goto(&fixture_url(html)).await.unwrap();
                assert!(
                    selectors
                        .input
                        .iter()
                        .any(|selector| super::element_exists(&page, selector)),
                    "{name}/{state} has no matching registered input selector"
                );
                assert!(
                    selectors
                        .send
                        .iter()
                        .any(|selector| super::element_exists(&page, selector)),
                    "{name}/{state} has no matching registered send selector"
                );
                assert_eq!(
                    selectors
                        .response
                        .iter()
                        .any(|selector| super::element_exists(&page, selector)),
                    expect_response,
                    "{name}/{state} has unexpected response selector behavior"
                );
            }
        }
    }
}
