use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, SiteAdapter};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;

/// Mistral (Le Chat) - chat.mistral.ai. Model picker top; Large/Small/Codestral/Pixtral.
pub struct MistralAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://chat.mistral.ai",
    picker: &["button.model-selector", "text:div=Mistral Large", "select"],
    input: &[
        "textarea[placeholder*=\"Ask\"]",
        "div[contenteditable=\"true\"]",
        "textarea.chat-input",
    ],
    send: &["button[aria-label=\"Send\"]", "button.submit"],
    response: &["div.markdown", "div.assistant-message", "div.prose"],
    model_keywords: "mistral|codestral|pixtral|ministral",
    model_pairs: &[],
};

#[async_trait(?Send)]
impl SiteAdapter for MistralAdapter {
    fn name(&self) -> &str {
        "mistral"
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
        let effective = SELECTORS.effective(&self.cfg.selectors);
        let inputs = effective.input_refs();
        if let Err(error) = common::wait_for_selector(page, &inputs, 15_000).await {
            common::check_session("mistral", page)?;
            let diagnostics = mistral_readiness_diagnostics(page);
            anyhow::bail!(
                "Mistral composer unavailable: {error}; {diagnostics}. Check provider JavaScript asset loading, session and network access; selector overrides cannot repair an HTTP 403"
            );
        }
        common::send_prompt(page, &effective, prompt).await
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

/// Collect only non-sensitive script metadata. HTTP response status is exposed
/// through Resource Timing only when the browser and origin permit it; a missing
/// status is *not* evidence that asset fetching succeeded.
fn mistral_readiness_diagnostics(page: &Page) -> String {
    let evidence = page.evaluate(
        r#"(() => {
            const entries = performance.getEntriesByType('resource')
                .filter(entry => entry.initiatorType === 'script');
            const assetStatuses = [];
            for (const entry of entries) {
                if (assetStatuses.length >= 8) break;
                const status = entry.responseStatus;
                if (!Number.isInteger(status) || status < 400 || status >= 600) continue;
                try {
                    // Never pass resource URLs, paths, query strings, or credentials
                    // to Rust diagnostics. Only the hostname and HTTP status.
                    const host = new URL(entry.name, location.href).hostname.toLowerCase();
                    if (/^[a-z0-9.-]{1,128}$/.test(host)) {
                        assetStatuses.push({host, status});
                    }
                } catch (_) {}
            }
            return {
                readyState: document.readyState,
                scriptCount: document.scripts.length,
                loadedResources: entries.length,
                assetStatuses,
                challenge: !!document.querySelector('[id*="challenge"], [class*="challenge"], iframe[src*="challenge"]')
            };
        })()"#,
    );
    mistral_readiness_summary(&evidence)
}

fn mistral_readiness_summary(evidence: &serde_json::Value) -> String {
    let state = evidence
        .get("readyState")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    // Obscura may decode JavaScript integer values as JSON floating-point
    // numbers; as_u64() alone silently reports zero for 3.0.
    let count = |key: &str| {
        evidence
            .get(key)
            .and_then(|value| value.as_f64())
            .filter(|value| value.is_finite() && *value >= 0.0)
            .unwrap_or(0.0) as u64
    };
    let scripts = count("scriptCount");
    let resources = count("loadedResources");
    let challenge = evidence
        .get("challenge")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let asset_errors: Vec<String> = evidence
        .get("assetStatuses")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .take(8)
        .filter_map(|item| {
            let host = item.get("host")?.as_str()?;
            if host.is_empty()
                || host.len() > 128
                || !host
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-')
            {
                return None;
            }
            let status = item.get("status")?.as_f64()?;
            if !(400.0..600.0).contains(&status) || status.fract() != 0.0 {
                return None;
            }
            Some(format!("{host}:{}", status as u16))
        })
        .collect();
    // Only classify directly observed symptoms; a 403 alone does not prove
    // whether cookies, provider CDN policy or browser compatibility caused it.
    let blocker = if challenge {
        "challenge_visible"
    } else if asset_errors.iter().any(|error| error.ends_with(":403")) {
        "script_http_403"
    } else if !asset_errors.is_empty() {
        "script_http_error"
    } else if scripts == 0 {
        "no_script_elements"
    } else {
        "undetermined"
    };
    let failures = if asset_errors.is_empty() {
        "not_observable".to_string()
    } else {
        asset_errors.join(",")
    };
    format!(
        "document_ready={state}, script_elements={scripts}, script_resource_entries={resources}, challenge_marker={challenge}, blocker={blocker}, script_asset_http_errors={failures}; Resource Timing statuses can be unavailable for cross-origin assets"
    )
}

#[cfg(test)]
mod readiness_tests {
    #[tokio::test]
    async fn missing_composer_returns_actionable_readiness_error() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body><script>window.ready=true</script></body></html>")
            .await
            .unwrap();
        let diagnostics = super::mistral_readiness_diagnostics(&page);
        assert!(diagnostics.contains("script_elements="));
        assert!(diagnostics.contains("script_asset_http_errors=not_observable"));
        assert!(!diagnostics.contains("data:text/html"));
    }
    #[test]
    fn script_asset_diagnostics_preserve_browser_counts_and_redact_urls() {
        let evidence = serde_json::json!({
            "readyState": "complete",
            "scriptCount": 3.0,
            "loadedResources": 4.0,
            "challenge": false,
            "assetStatuses": [
                {"host": "static.mistral.ai", "status": 403.0},
                {"host": "https://static.mistral.ai/secret?token=private", "status": 403.0},
                {"host": "static.mistral.ai", "status": 200.0},
                {"host": "other-cdn.example", "status": 502.0}
            ]
        });
        let result = super::mistral_readiness_summary(&evidence);
        assert!(result.contains("script_elements=3"));
        assert!(result.contains("script_resource_entries=4"));
        assert!(result.contains("static.mistral.ai:403"));
        assert!(result.contains("other-cdn.example:502"));
        assert!(result.contains("blocker=script_http_403"));
        assert!(!result.contains("secret"));
        assert!(!result.contains("token"));
        assert!(!result.contains("https://"));
        assert!(!result.contains("static.mistral.ai:200"));
    }

    #[test]
    fn challenge_and_unobservable_script_failures_are_distinct() {
        let challenge = serde_json::json!({
            "readyState": "complete",
            "scriptCount": 2.0,
            "loadedResources": 1.0,
            "challenge": true,
            "assetStatuses": [{"host": "static.mistral.ai", "status": 403.0}]
        });
        assert!(super::mistral_readiness_summary(&challenge).contains("blocker=challenge_visible"));
        let missing = serde_json::json!({
            "readyState": "complete",
            "scriptCount": 0.0,
            "loadedResources": 0.0,
            "challenge": false,
            "assetStatuses": []
        });
        let diagnostics = super::mistral_readiness_summary(&missing);
        assert!(diagnostics.contains("blocker=no_script_elements"));
        assert!(diagnostics.contains("script_asset_http_errors=not_observable"));
        assert!(!diagnostics.contains("403"));
    }
}
