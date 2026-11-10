use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, SiteAdapter};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;

/// Strongly observed HTTP 403 for a script asset while Mistral's composer
/// remains unavailable. Do not infer if CDN policy, network, browser, or
/// missing authentication caused it; no path/query/cookie is retained.
#[derive(Debug)]
pub struct MistralAssetBlockedError;

impl std::fmt::Display for MistralAssetBlockedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Mistral composer blocked: JavaScript asset returned HTTP 403")
    }
}

impl std::error::Error for MistralAssetBlockedError {}

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
            if diagnostics.contains("blocker=script_http_403") {
                // Observed script HTTP status, not speculation about its cause.
                // The API maps this typed failure to a redacted blocked status.
                tracing::warn!(
                    provider = "mistral",
                    diagnostics = %diagnostics,
                    "Mistral composer blocked by directly observed script HTTP 403"
                );
                return Err(anyhow::Error::new(MistralAssetBlockedError));
            }
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
    let mut evidence = page.evaluate(
        r#"(() => {
            // JS bundles loaded by link rel=modulepreload (or preload as=script)
            // may report initiatorType=link, fetch, or other rather than script.
            // Include only explicit script-asset preloads, not stylesheet
            // failures that could otherwise be misclassified as JS blockers.
            const scriptPreloads = new Set(
                Array.from(document.querySelectorAll(
                    'link[rel~="modulepreload"], link[rel~="preload"][as="script"]'
                )).map(link => link.href)
            );
            // Some embedded engines report module/script fetches with a
            // non-script initiator. Match actual script-tag sources too;
            // never promote unrelated fetches or stylesheets to JS failures.
            const scriptSources = new Set(
                Array.from(document.scripts).map(script => script.src).filter(Boolean)
            );
            let resourceTimingAvailable = false;
            let entries = [];
            try {
                const resources = performance.getEntriesByType('resource');
                if (Array.isArray(resources)) {
                    resourceTimingAvailable = true;
                    entries = resources.filter(entry => entry.initiatorType === 'script' ||
                        scriptPreloads.has(entry.name) || scriptSources.has(entry.name));
                }
            } catch (_) {
                // Some restricted/partial pages lack Resource Timing. Do not
                // mistake a failed probe for proof that all scripts loaded.
            }
            const assetStatuses = [];
            let scriptHttp403Observed = false;
            for (const entry of entries) {
                const status = entry.responseStatus;
                if (!Number.isInteger(status) || status < 400 || status >= 600) continue;
                try {
                    // A late script 403 must not disappear when the bounded
                    // diagnostics list already contains eight other failures.
                    // Never pass asset URLs, paths, queries, or credentials to Rust.
                    const asset = new URL(entry.name, location.href);
                    if (asset.protocol !== 'https:' && asset.protocol !== 'http:') continue;
                    const host = asset.hostname.toLowerCase();
                    if (/^[a-z0-9.-]{1,128}$/.test(host)) {
                        if (status === 403) scriptHttp403Observed = true;
                        if (assetStatuses.length < 8) assetStatuses.push({host, status});
                    }
                } catch (_) {}
            }
            return {
                readyState: document.readyState,
                scriptCount: document.scripts.length,
                loadedResources: entries.length,
                resourceTimingAvailable,
                assetStatuses,
                scriptHttp403Observed
            };
        })()"#,
    );
    // A CSS class containing "challenge" may be part of ordinary UI state.
    // Use the same host/path and challenge-body proof as the provider guard,
    // without forwarding body text or URL query values into diagnostics.
    if let Some(object) = evidence.as_object_mut() {
        object.insert(
            "challenge".to_owned(),
            crate::provider_guard::is_challenge_page(page).into(),
        );
    }
    mistral_readiness_summary(&evidence)
}

fn mistral_readiness_summary(evidence: &serde_json::Value) -> String {
    // A blocked script or unsupported browser probe can make evaluate() fail
    // entirely. Missing evidence is not proof that zero script tags exist.
    if !evidence.is_object() {
        return "document_ready=unknown, blocker=diagnostic_unavailable, script_asset_http_errors=not_observable; browser readiness probe did not return structured evidence".to_owned();
    }
    // Do not forward untrusted page-provided strings into diagnostics.
    // A restricted browser or altered document getter can return arbitrary
    // text; preserve only the standardized readyState values.
    let state = match evidence.get("readyState").and_then(|value| value.as_str()) {
        Some("loading") => "loading",
        Some("interactive") => "interactive",
        Some("complete") => "complete",
        _ => "unknown",
    };
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
    let resource_timing = match evidence
        .get("resourceTimingAvailable")
        .and_then(|v| v.as_bool())
    {
        Some(true) => "available",
        Some(false) => "unavailable",
        None => "unknown",
    };
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
    } else if evidence
        .get("scriptHttp403Observed")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
        || asset_errors.iter().any(|error| error.ends_with(":403"))
    {
        "script_http_403"
    } else if !asset_errors.is_empty() {
        "script_http_error"
    } else if scripts == 0 {
        "no_script_elements"
    } else if resource_timing == "unavailable" {
        "resource_timing_unavailable"
    } else {
        "undetermined"
    };
    let failures = if asset_errors.is_empty() {
        "not_observable".to_string()
    } else {
        asset_errors.join(",")
    };
    format!(
        "document_ready={state}, script_elements={scripts}, script_resource_entries={resources}, resource_timing={resource_timing}, challenge_marker={challenge}, blocker={blocker}, script_asset_http_errors={failures}; Resource Timing statuses can be unavailable for cross-origin assets"
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
    #[tokio::test]
    async fn restricted_resource_timing_is_reported_without_false_script_success() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body><script>window.ready=true</script></body></html>")
            .await
            .unwrap();
        page.evaluate(
            r#"Object.defineProperty(performance, 'getEntriesByType', {
                configurable: true,
                value: () => { throw new Error('synthetic blocked timing API'); }
            })"#,
        );
        let diagnostics = super::mistral_readiness_diagnostics(&page);
        assert!(diagnostics.contains("script_elements=1"), "{diagnostics}");
        assert!(
            diagnostics.contains("resource_timing=unavailable"),
            "{diagnostics}"
        );
        assert!(
            diagnostics.contains("blocker=resource_timing_unavailable"),
            "{diagnostics}"
        );
        assert!(!diagnostics.contains("synthetic blocked timing API"));
        assert!(!diagnostics.contains("blocker=script_http_403"));
    }

    #[test]
    fn unexpected_document_ready_state_cannot_inject_page_text_into_diagnostics() {
        for legitimate in ["loading", "interactive", "complete"] {
            let evidence = serde_json::json!({
                "readyState": legitimate,
                "scriptCount": 1,
                "loadedResources": 0,
                "assetStatuses": []
            });
            let result = super::mistral_readiness_summary(&evidence);
            assert!(result.contains(&format!("document_ready={legitimate}")));
        }
        let evidence = serde_json::json!({
            "readyState": "complete\\nsecret=session-token\\nblocker=script_http_403",
            "scriptCount": 2,
            "loadedResources": 0,
            "assetStatuses": [],
            "scriptHttp403Observed": false
        });
        let result = super::mistral_readiness_summary(&evidence);
        assert!(result.contains("document_ready=unknown"), "{result}");
        assert!(result.contains("blocker=undetermined"), "{result}");
        assert!(!result.contains("session-token"));
        assert!(!result.contains("complete\\nsecret"));
    }

    #[test]
    fn unavailable_evaluation_is_not_misreported_as_zero_script_elements() {
        let diagnostics = super::mistral_readiness_summary(&serde_json::Value::Null);
        assert!(diagnostics.contains("blocker=diagnostic_unavailable"));
        assert!(!diagnostics.contains("blocker=no_script_elements"));
    }

    #[tokio::test]
    async fn module_preloads_are_script_resources_but_stylesheet_links_are_not() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        // Data URLs keep this synthetic fixture independent of the network.
        let html = r#"<html><head>
            <link id="module" rel="modulepreload" href="data:text/javascript,export%20default%201">
            <link id="classic" rel="preload" as="script" href="data:text/javascript,void(0)">
            </head><body></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let stubbed = page.evaluate(
            r#"(() => {
                const links = ['module', 'classic'].map(id => document.getElementById(id).href);
                Object.defineProperty(performance, 'getEntriesByType', {
                    configurable: true,
                    value: () => [
                        ...links.map((name, index) => ({initiatorType: index === 0 ? 'other' : 'fetch', name, responseStatus: 403})),
                        {initiatorType: 'link', name: 'https://style.example.invalid/style.css', responseStatus: 403}
                    ]
                });
                return true;
            })()"#,
        );
        assert_eq!(stubbed.as_bool(), Some(true));
        let result = super::mistral_readiness_diagnostics(&page);
        assert!(result.contains("script_resource_entries=2"), "{result}");
        assert!(!result.contains("style.example.invalid"));
        assert!(!result.contains("blocker=script_http_403"));
        assert!(result.contains("script_asset_http_errors=not_observable"));
    }

    #[tokio::test]
    async fn script_src_with_fetch_initiator_is_script_evidence_but_css_is_not() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body><script>window.ready=true</script></body></html>")
            .await
            .unwrap();
        let installed = page.evaluate(
            r#"(() => {
                const retained = document.createElement('script');
                // Unsupported script type: never fetch or execute a live asset.
                retained.type = 'application/x-synthetic-fixture';
                Object.defineProperty(retained, 'src', {
                    configurable: true,
                    get: () => 'https://assets.example.invalid/app.bundle.js?token=private'
                });
                document.body.appendChild(retained);
                Object.defineProperty(performance, 'getEntriesByType', {
                    configurable: true,
                    value: () => [
                        {initiatorType: 'fetch',
                         name: 'https://assets.example.invalid/app.bundle.js?token=private',
                         responseStatus: 403},
                        {initiatorType: 'link',
                         name: 'https://style.example.invalid/app.css',
                         responseStatus: 403}
                    ]
                });
                return true;
            })()"#,
        );
        assert_eq!(installed.as_bool(), Some(true));
        let diagnostics = super::mistral_readiness_diagnostics(&page);
        assert!(
            diagnostics.contains("script_resource_entries=1"),
            "{diagnostics}"
        );
        assert!(
            diagnostics.contains("blocker=script_http_403"),
            "{diagnostics}"
        );
        assert!(
            diagnostics.contains("assets.example.invalid:403"),
            "{diagnostics}"
        );
        assert!(
            !diagnostics.contains("style.example.invalid"),
            "{diagnostics}"
        );
        assert!(!diagnostics.contains("private"), "{diagnostics}");
    }

    #[tokio::test]
    async fn late_script_403_survives_eight_entry_diagnostic_cap_without_url_leaks() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body><script>window.ready=true</script></body></html>")
            .await
            .unwrap();
        let stubbed = page.evaluate(
            r#"(() => {
                Object.defineProperty(performance, 'getEntriesByType', {
                    configurable: true,
                    value: () => Array.from({length:10}, (_, i) => ({
                        initiatorType: 'script',
                        name: 'https://cdn.example.invalid/private-asset-' + i + '.js?token=secret',
                        responseStatus: i === 9 ? 403 : 500
                    }))
                });
                return true;
            })()"#,
        );
        assert_eq!(stubbed.as_bool(), Some(true));
        let result = super::mistral_readiness_diagnostics(&page);
        assert!(result.contains("script_resource_entries=10"), "{result}");
        assert!(result.contains("blocker=script_http_403"), "{result}");
        assert!(result.contains("cdn.example.invalid:500"), "{result}");
        assert!(!result.contains("cdn.example.invalid:403"), "{result}");
        assert!(!result.contains("private-asset"));
        assert!(!result.contains("token"));
        assert!(!result.contains("secret"));
    }

    #[tokio::test]
    async fn generic_challenge_class_is_not_a_manual_verification_blocker() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><div class="challenge-history">Recent work</div><script>window.ready=true</script></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let ordinary = super::mistral_readiness_diagnostics(&page);
        assert!(
            !ordinary.contains("blocker=challenge_visible"),
            "{ordinary}"
        );

        page.evaluate(
            "document.body.insertAdjacentHTML('beforeend', '<p>Verify you are human</p>')",
        );
        let challenged = super::mistral_readiness_diagnostics(&page);
        assert!(
            challenged.contains("blocker=challenge_visible"),
            "{challenged}"
        );
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
        assert_eq!(
            super::MistralAssetBlockedError.to_string(),
            "Mistral composer blocked: JavaScript asset returned HTTP 403"
        );
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
