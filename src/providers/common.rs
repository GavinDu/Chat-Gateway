use obscura_core::Page;
use std::time::Duration;

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

impl SelectorSet {
    pub fn adapter_selectors(&self) -> crate::providers::AdapterSelectors {
        crate::providers::AdapterSelectors {
            input_selector: self.input[0].to_string(),
            send_button: self.send[0].to_string(),
            response_container: self.response[0].to_string(),
            model_picker: self.picker[0].to_string(),
        }
    }
}

/// Streaming indicator shared by all providers (v1 template).
pub const STREAMING_SELECTOR: &str = r#"[data-is-streaming="true"], button[aria-label*="Stop"], button:has-text("Stop")"#;

pub fn element_exists(page: &Page, selector: &str) -> bool {
    page.query_selector(selector).is_some()
}

pub fn is_streaming(page: &Page) -> bool {
    element_exists(page, STREAMING_SELECTOR)
}

/// Wait until any of the selectors matches (polls query_selector).
pub async fn wait_for_selector(page: &Page, selectors: &[&str], timeout_ms: u64) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        if selectors.iter().any(|sel| page.query_selector(sel).is_some()) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    anyhow::bail!("timeout waiting for {:?}", selectors)
}

/// Click the first matching element (obscura Element::click is
/// synchronous, scrolls into view first, and focuses the element).
pub fn click_first(page: &Page, selectors: &[&str]) -> anyhow::Result<()> {
    for sel in selectors {
        if let Some(el) = page.query_selector(sel) {
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
    let text_js = serde_json::to_string(text)?;
    let js = format!(
        "(function() {{ var el = document.activeElement; if (!el) return false; if (el.tagName === 'TEXTAREA') {{ try {{ var setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set; setter.call(el, {text}); }} catch (e) {{ el.value = {text}; }} el.dispatchEvent(new InputEvent('input', {{ bubbles: true, inputType: 'insertText', data: {text} }})); el.dispatchEvent(new Event('change', {{ bubbles: true }})); return true; }} try {{ document.execCommand('selectAll', false, null); }} catch (e) {{}} var ok = false; try {{ ok = document.execCommand('insertText', false, {text}); }} catch (e) {{}} if (!ok) {{ if ('value' in el) {{ el.value = {text}; }} else {{ el.textContent = {text}; }} }} el.dispatchEvent(new InputEvent('input', {{ bubbles: true, inputType: 'insertText', data: {text} }})); el.dispatchEvent(new Event('change', {{ bubbles: true }})); return true; }})()",
        text = text_js
    );
    if page.evaluate(&js).as_bool() != Some(true) {
        anyhow::bail!("typing via document.activeElement failed");
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
pub async fn click_send_or_enter(page: &Page, send_selectors: &[&str], input_selector: &str, timeout_ms: u64) -> anyhow::Result<bool> {
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        if let Some(sel) = send_selectors.iter().find(|s| page.query_selector(s).is_some()) {
            if let Some(el) = page.query_selector(sel) {
                el.click()?;
                return Ok(true);
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    // Go proceeds after pressEnter regardless of its outcome.
    let _ = press_enter(page, input_selector);
    Ok(false)
}

/// Login/session-expiry detection, Go concept in Rust form (Rust v1 had
/// none): Go's login keyword list (base.go) matched against the page
/// URL. Callers fail the attempt with a distinct session_expired error
/// so the API can report it and the router can skip the provider.
pub const LOGIN_KEYWORDS: &[&str] = &["login", "signin", "sign-in", "auth", "accounts.google.com", "auth0"];

pub fn is_login_page(page: &Page) -> bool {
    let url = page.url().to_lowercase();
    LOGIN_KEYWORDS.iter().any(|kw| url.contains(kw))
}

pub fn check_session(provider: &str, page: &Page) -> anyhow::Result<()> {
    if is_login_page(page) {
        anyhow::bail!("session_expired: {provider} session expired, re-login required");
    }
    Ok(())
}

/// Go discovery normalization (discovery.go normalizeModelOption +
/// isNonModelLabel), as a post-processing step on Rust v1's scraped
/// labels: first non-empty line, whitespace-collapsed, at most 120
/// chars, dropping the 14 non-model labels. Rust v1's config-alias
/// merge is kept (Go's discovered-only return is NOT adopted).
pub const NON_MODEL_LABELS: &[&str] = &[
    "selected", "current", "select", "close", "cancel", "settings",
    "new chat", "new conversation", "search", "more", "home", "explore",
    "recent", "history",
];

pub fn normalize_model_option(raw: &str) -> Option<String> {
    let first = raw.lines().find(|l| !l.trim().is_empty())?;
    let collapsed = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() || collapsed.len() > 120 {
        return None;
    }
    if NON_MODEL_LABELS.iter().any(|l| l.eq_ignore_ascii_case(&collapsed)) {
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
pub async fn select_model(page: &Page, pairs: &[(&str, &str)], model: &str, default_aliases: &[String]) -> anyhow::Result<()> {
    let model = model.trim();
    if model.is_empty() {
        return Ok(());
    }
    if default_aliases.iter().any(|a| a.trim().eq_ignore_ascii_case(model)) {
        return Ok(());
    }
    let target_js = serde_json::to_string(&model.to_lowercase())?;
    for (trigger, options) in pairs {
        let found = {
            let start = std::time::Instant::now();
            let mut hit = false;
            while start.elapsed().as_millis() < 3000 {
                if let Some(el) = page.query_selector(trigger) {
                    let _ = el.click();
                    hit = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            hit
        };
        if !found {
            continue;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        let opt_js = serde_json::to_string(options)?;
        let js = format!(
            "(function() {{ var els = document.querySelectorAll({}); for (var i = 0; i < els.length; i++) {{ var t = (els[i].innerText || els[i].textContent || ''); if (t.toLowerCase().indexOf({}) !== -1) {{ els[i].click(); return t; }} }} return null; }})()",
            opt_js, target_js
        );
        let v = page.evaluate(&js);
        if !v.is_null() {
            tokio::time::sleep(Duration::from_millis(300)).await;
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
pub fn probe_selector_groups(page: &Page, groups: &[(&str, &[&str])]) -> Vec<(String, Option<String>)> {
    groups.iter().map(|(role, selectors)| {
        let hit = selectors.iter().find(|s| page.query_selector(s).is_some()).map(|s| s.to_string());
        (role.to_string(), hit)
    }).collect()
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
    if v.is_null() { None } else { Some(v.as_str().unwrap_or("").to_string()) }
}

/// Response text for an adapter: first response selector (in fallback
/// order) that currently yields non-empty text.
pub fn response_text(page: &Page, set: &SelectorSet) -> Option<String> {
    for sel in set.response {
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
pub async fn discover_models(page: &mut Page, set: &SelectorSet, cfg_models: &[String]) -> anyhow::Result<Vec<String>> {
    page.goto(set.url).await?;
    tokio::time::sleep(Duration::from_millis(1200)).await;

    // Try to open model picker without sending message
    for sel in set.picker {
        if let Some(el) = page.query_selector(sel) {
            let _ = el.click();
            tokio::time::sleep(Duration::from_millis(600)).await;
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
    let discovered = normalize_model_labels(discovered);

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
pub async fn send_prompt(page: &Page, set: &SelectorSet, prompt: &str) -> anyhow::Result<()> {
    // 1. Wait for input ready
    wait_for_selector(page, set.input, 15000).await?;

    // 2. Focus + clear + type (Obscura stealth is built in)
    human_type(page, set.input, prompt).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 3. Send: button click, with Go's Enter fallback when no send
    // button appears within the 5s send budget. The input selector is
    // the first input selector that currently matches, else the first
    // listed input selector.
    let input_sel = set.input.iter().copied()
        .find(|s| page.query_selector(s).is_some())
        .unwrap_or(set.input[0]);
    click_send_or_enter(page, set.send, input_sel, 5000).await?;
    Ok(())
}
