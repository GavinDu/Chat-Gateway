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
/// document.activeElement via execCommand('insertText') with a
/// value/textContent fallback, dispatching input/change events so
/// framework-controlled inputs observe the change.
pub async fn human_type(page: &Page, selectors: &[&str], text: &str) -> anyhow::Result<()> {
    click_first(page, selectors)?;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let text_js = serde_json::to_string(text)?;
    let js = format!(
        "(function() {{ var el = document.activeElement; if (!el) return false; try {{ document.execCommand('selectAll', false, null); }} catch (e) {{}} var ok = false; try {{ ok = document.execCommand('insertText', false, {text}); }} catch (e) {{}} if (!ok) {{ if ('value' in el) {{ el.value = {text}; }} else {{ el.textContent = {text}; }} }} el.dispatchEvent(new InputEvent('input', {{ bubbles: true }})); el.dispatchEvent(new Event('change', {{ bubbles: true }})); return true; }})()",
        text = text_js
    );
    if page.evaluate(&js).as_bool() != Some(true) {
        anyhow::bail!("typing via document.activeElement failed");
    }
    Ok(())
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

    if discovered.is_empty() {
        Ok(cfg_models.to_vec())
    } else {
        // Merge with configured aliases per Go behavior
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

    // 3. Send
    click_first(page, set.send)?;
    Ok(())
}
