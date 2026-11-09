use obscura_core::Page;
use std::time::Duration;

/// The distinct browser timeout phases are preserved as typed errors.
#[derive(Debug, Clone, Copy)]
pub enum ProviderTimeoutPhase {
    Submission,
    Generation,
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
            ProviderTimeoutPhase::Generation => write!(
                f,
                "provider '{}' did not complete within {:?}",
                self.provider, self.timeout
            ),
        }
    }
}

impl std::error::Error for ProviderTimeoutError {}

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
pub const STREAMING_SELECTOR: &str = r#"[data-is-streaming="true"], button[aria-label*="Stop"]"#;

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
                    matches.forEach(node => nodes.add(node));
                    if (text === null && matches.length) {{
                        const last = matches[matches.length - 1];
                        const content = last.innerText || last.textContent || "";
                        if (content) {{
                            text = content;
                            lastResponseNode = last;
                        }}
                    }}
                }} catch (_) {{}}
            }}
            let streaming = false;
            try {{ streaming = document.querySelector({streaming_selector}) !== null; }} catch (_) {{}}
            if (!streaming) {{
                streaming = Array.from(document.querySelectorAll("button"))
                    .some(button => ((button.innerText || button.textContent || "").trim()).toLowerCase().includes("stop"));
            }}
            const url = String(location.href || "");
            const body = ({check_body_challenge} ? (document.body?.innerText || "").slice(0, 12000) : "").toLowerCase();
            // URL challenge detection runs in Rust using the shared host/path
            // classifier. Query values and chat text must not trip the guard.
            const challenge = ["verify you are human", "confirm you are not a robot", "unusual traffic", "complete the captcha", "security challenge", "checking your browser"].some(marker => body.includes(marker));
            let postAnswerAffordance = false;
            if (lastResponseNode) {{
                const scopeSelector = "article, [data-message-author-role='assistant'], [data-testid='assistant-message'], [data-testid*='conversation-turn'], [data-testid*='message']";
                let scope = lastResponseNode;
                for (let depth = 0; depth < 6 && scope; depth += 1, scope = scope.parentElement) {{
                    let buttons = [];
                    try {{ buttons = Array.from(scope.querySelectorAll("button, [role='button']")); }} catch (_) {{}}
                    const labels = buttons.map(button => ((button.getAttribute("aria-label") || "") + " " + (button.getAttribute("data-testid") || "") + " " + (button.innerText || button.textContent || "")).toLowerCase());
                    if (labels.some(label => /copy|regenerate|retry|good response|bad response|thumbs up|thumbs down/.test(label))) {{
                        postAnswerAffordance = true;
                        break;
                    }}
                    if (scope.matches && scope.matches(scopeSelector) && depth >= 3) break;
                }}
            }}
            return {{ text, count: nodes.size, streaming, postAnswerAffordance, url, challenge }};
        }})()"#,
        check_body_challenge = check_body_challenge,
    );
    decode_response_probe(page.evaluate(&script))
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
        "(() => {{ const el = Array.from(document.querySelectorAll({tag})).find(el => ((el.innerText || el.textContent || '').trim()).includes({needle}) && el.getClientRects().length); if (!el) return false; el.click(); return true; }})()"
    ));
    Ok(clicked.as_bool() == Some(true))
}

pub fn element_exists(page: &Page, selector: &str) -> bool {
    if text_selector_parts(selector).is_some() {
        text_selector_matches(page, selector)
    } else {
        page.query_selector(selector).is_some()
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

/// Wait until any of the selectors matches (polls query_selector).
pub async fn wait_for_selector(
    page: &mut Page,
    selectors: &[&str],
    timeout_ms: u64,
) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        if selectors.iter().any(|sel| element_exists(page, sel)) {
            return Ok(());
        }
        drive_page(page, Duration::from_millis(150)).await;
    }
    anyhow::bail!("timeout waiting for {:?}", selectors)
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
/// framework-observable), and plain TEXTAREA inputs use Go's qwen-style
/// native value setter branch instead of execCommand. Everything else
/// keeps merged's path (selectAll clear + execCommand insertText).
pub async fn human_type(page: &Page, selectors: &[&str], text: &str) -> anyhow::Result<()> {
    click_first(page, selectors)?;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let selector = selectors
        .iter()
        .copied()
        .find(|selector| element_exists(page, selector))
        .ok_or_else(|| anyhow::anyhow!("provider input disappeared before typing"))?;
    let selector_js = serde_json::to_string(selector)?;
    let text_js = serde_json::to_string(text)?;
    let js = format!(
        "(function() {{ var el = document.querySelector({selector}); if (!el) return false; el.focus(); if (el.tagName === 'TEXTAREA') {{ try {{ var setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set; setter.call(el, {text}); }} catch (e) {{ el.value = {text}; }} el.dispatchEvent(new InputEvent('input', {{ bubbles: true, inputType: 'insertText', data: {text} }})); el.dispatchEvent(new Event('change', {{ bubbles: true }})); return true; }} try {{ document.execCommand('selectAll', false, null); }} catch (e) {{}} var ok = false; try {{ ok = document.execCommand('insertText', false, {text}); }} catch (e) {{}} if (!ok) {{ if ('value' in el) {{ el.value = {text}; }} else {{ el.textContent = {text}; }} }} el.dispatchEvent(new InputEvent('input', {{ bubbles: true, inputType: 'insertText', data: {text} }})); el.dispatchEvent(new Event('change', {{ bubbles: true }})); return true; }})()",
        text = text_js,
        selector = selector_js,
    );
    if page.evaluate(&js).as_bool() != Some(true) {
        anyhow::bail!("typing into the provider input failed");
    }
    // Controlled editors may discard synthetic input on their next render.
    // A successful event dispatch alone is not evidence that the prompt stuck.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let retained = page.evaluate(&format!(
        "(() => {{ const el = document.querySelector({selector_js}); if (!el) return false; const value = ('value' in el ? el.value : (el.innerText || el.textContent || '')); return value === {text_js}; }})()"
    ));
    if retained.as_bool() != Some(true) {
        anyhow::bail!("provider input did not retain the prompt after framework render");
    }
    Ok(())
}

/// Dispatch an Enter keypress sequence on the element matching
/// `selector`. Obscura exposes no keyboard API, so this is necessarily
/// an approximation of Go's rod pressEnter: synthetic KeyboardEvents,
/// not trusted keystrokes. Frameworks listening only to trusted input
/// may ignore it.
pub fn press_enter(page: &Page, selector: &str) -> anyhow::Result<()> {
    let sel_js = serde_json::to_string(selector)?;
    let js = format!(
        "(function() {{ var el = document.querySelector({}); if (!el) return false; el.focus(); ['keydown','keypress','keyup'].forEach(function(t) {{ el.dispatchEvent(new KeyboardEvent(t, {{key:'Enter', code:'Enter', keyCode:13, which:13, bubbles:true, cancelable:true}})); }}); return true; }})()",
        sel_js
    );
    if page.evaluate(&js).as_bool() != Some(true) {
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
    let start = std::time::Instant::now();
    let input_js = serde_json::to_string(input_selector)?;
    while start.elapsed().as_millis() < timeout_ms as u128 {
        for selector in send_selectors {
            let selector_js = serde_json::to_string(selector)?;
            let js = format!(
                "(() => {{ const input = document.querySelector({input_js}); if (!input) return false; let root = input; for (let depth = 0; root && depth < 6; depth++, root = root.parentElement) {{ if (root.tagName === 'BODY' || root.tagName === 'HTML') break; const button = root.querySelector({selector_js}); if (button && !button.disabled && button.getAttribute('aria-disabled') !== 'true') {{ button.click(); return true; }} if (root.tagName === 'FORM') break; }} return false; }})()"
            );
            if page.evaluate(&js).as_bool() == Some(true) {
                return Ok(true);
            }
        }
        drive_page(page, Duration::from_millis(150)).await;
    }
    // A disabled send control is an explicit refusal by the application.
    // Synthetic Enter is not trusted and must not bypass that state.
    let send_js = serde_json::to_string(send_selectors)?;
    let disabled = page.evaluate(&format!(
        "(() => {{ const input = document.querySelector({input_js}); if (!input) return true; const selectors = {send_js}; let root = input; for (let depth = 0; root && depth < 6; depth++, root = root.parentElement) {{ if (root.tagName === 'BODY' || root.tagName === 'HTML') break; for (const selector of selectors) {{ const button = root.querySelector(selector); if (button && (button.disabled || button.getAttribute('aria-disabled') === 'true')) return true; }} if (root.tagName === 'FORM') break; }} return false; }})()"
    ));
    if disabled.as_bool() == Some(true) {
        anyhow::bail!("provider send control is disabled or composer is missing");
    }
    // Enter dispatch is a best-effort fallback, not a submission acknowledgement.
    press_enter(page, input_selector)?;
    Ok(false)
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

/// Text of the last element matching `selector` (innerText, falling back
/// to textContent), or None when nothing matches. Replaces chromiumoxide
/// find_elements + inner_text polling.
pub fn last_element_text(page: &Page, selector: &str) -> Option<String> {
    let sel_js = serde_json::to_string(selector).ok()?;
    let js = format!(
        "(function() {{ var els = document.querySelectorAll({}); if (!els.length) return null; var el = els[els.length - 1]; return el.innerText || el.textContent || ''; }})()",
        sel_js
    );
    let v = page.evaluate(&js);
    if v.is_null() {
        None
    } else {
        Some(v.as_str().unwrap_or("").to_string())
    }
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

/// Wait for the input, type the prompt, click send (v1 send steps 1-3;
/// the response stream is polled by providers::run_send).
pub async fn send_prompt(
    page: &mut Page,
    set: &EffectiveSelectorSet,
    prompt: &str,
) -> anyhow::Result<()> {
    let inputs = set.input_refs();
    wait_for_selector(page, &inputs, 15_000).await?;
    human_type(page, &inputs, prompt).await?;
    drive_page(page, Duration::from_millis(200)).await;
    let input_sel = inputs
        .iter()
        .copied()
        .find(|s| page.query_selector(s).is_some())
        .unwrap_or(inputs[0]);
    let sends = set.send_refs();
    // The click/Enter dispatch is not proof that a controlled SPA accepted
    // the prompt. ChatGPT and Grok require a visible submission transition.
    let verify_submission = page.url().contains("chatgpt.com") || page.url().contains("grok.com");
    let baseline = if verify_submission {
        Some(submission_snapshot(page, input_sel, &set.response_refs())?)
    } else {
        None
    };
    click_send_or_enter(page, &sends, input_sel, 5_000).await?;
    if let Some(before) = baseline {
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            drive_page(page, Duration::from_millis(150)).await;
            let after = submission_snapshot(page, input_sel, &set.response_refs())?;
            if submission_accepted(&before, &after) {
                return Ok(());
            }
        }
        anyhow::bail!("provider did not acknowledge prompt submission (composer unchanged and no new response)");
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
    let responses = serde_json::to_string(response_selectors)?;
    let js = format!(
        r#"(() => {{
            const el = document.querySelector({input});
            const value = el ? ('value' in el ? el.value : (el.innerText || el.textContent || '')) : null;
            const responses = {responses};
            const nodes = new Set();
            for (const selector of responses) {{
                try {{ document.querySelectorAll(selector).forEach(node => nodes.add(node)); }} catch (_) {{}}
            }}
            return {{ inputPresent: !!el, inputEmpty: value !== null && !value.trim(), responseCount: nodes.size,
                streaming: !!document.querySelector('[data-is-streaming="true"], button[aria-label*="Stop"]') }};
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
        streaming: v
            .get("streaming")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
    })
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
    streaming: bool,
}

fn submission_accepted(before: &SubmissionSnapshot, after: &SubmissionSnapshot) -> bool {
    (after.input_present && after.input_empty && !before.input_empty)
        || after.response_count > before.response_count
        || (after.streaming && !before.streaming)
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
        assert!(submission_accepted(
            &before,
            &SubmissionSnapshot {
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
            streaming: true,
        };
        assert!(!submission_accepted(&before, &before));
    }
}

#[cfg(test)]
mod text_selector_tests {
    use super::text_selector_parts;

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
