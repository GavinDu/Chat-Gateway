//! Embedded browser construction with existing storage and stealth defaults.
use crate::{config::AppConfig, session};
use anyhow::{anyhow, bail, Result};
use std::{rc::Rc, time::Duration};

pub(super) fn build_browser(cfg: &AppConfig) -> Result<obscura_core::Browser> {
    build_browser_with_storage(cfg, true)
}

/// Create a *separate* pinned Obscura Browser for a future candidate replay.
/// The published Browser::new_page() shares one BrowserContext cookie jar,
/// localStorage and IndexedDB with every other page on that Browser. A second
/// Browser (without storage_dir) owns fresh independent context state instead.
/// This factory does not import cookies, navigate or promote the candidate;
/// the live worker still owns the existing persistent Browser until #1385.
#[allow(dead_code)] // The staged-activation caller is a separate, gated issue (#1385).
pub(super) fn build_disposable_browser(cfg: &AppConfig) -> Result<obscura_core::Browser> {
    build_browser_with_storage(cfg, false)
}

fn build_browser_with_storage(cfg: &AppConfig, persistent: bool) -> Result<obscura_core::Browser> {
    let mut builder = obscura_core::Browser::builder().stealth(cfg.obscura.stealth);
    if let Some(user_agent) = cfg
        .obscura
        .user_agent
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        builder = builder.user_agent(user_agent.to_owned());
    }
    // Never pass either the explicit Obscura directory or the legacy Chrome
    // profile fallback to an isolated staging Browser. Even a failed candidate
    // must not read or write the live user's profile or cookie persistence.
    if persistent {
        let storage = cfg
            .obscura
            .storage_dir
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                if cfg.server.chrome_profile_dir.is_empty() {
                    None
                } else {
                    Some(cfg.server.chrome_profile_dir.clone())
                }
            });
        if let Some(dir) = storage {
            builder = builder.storage_dir(dir);
        }
    }
    Ok(builder.build()?)
}

/// Own a disposable candidate's page and cookie/context owner together.
/// The pinned V8-backed Page is !Send, so the pair must stay on the browser
/// worker's LocalSet. Field order drops the Page (and its bridge) first,
/// before the Browser and its independent cookie/localStorage context.
/// This is a staging *primitive*, never proof of imported account readiness.
#[must_use]
#[allow(dead_code)] // Production admission/atomic promotion is tracked in #1385.
pub(super) struct DisposableProviderStage {
    page: obscura_core::Page,
    browser: Rc<obscura_core::Browser>,
}

#[allow(dead_code)] // Only future gated RefreshSession can use these borrows.
impl DisposableProviderStage {
    pub(super) fn browser(&self) -> &obscura_core::Browser {
        &self.browser
    }

    pub(super) fn page_mut(&mut self) -> &mut obscura_core::Page {
        &mut self.page
    }

    /// Transfer ownership of the verified candidate as a single unit only
    /// after the production generation/consent gate in #1463 succeeds.
    pub(super) fn into_owned_parts(self) -> (obscura_core::Page, Rc<obscura_core::Browser>) {
        (self.page, self.browser)
    }
}

/// Allocate a new provider page only after the independent, nonpersistent
/// context exists. No navigation, credential import or side effects on the
/// live Browser occur. Do not promote only `page` without `browser`.
#[allow(dead_code)]
pub(super) async fn new_disposable_provider_stage(
    cfg: &AppConfig,
    provider: crate::providers::Provider,
) -> Result<DisposableProviderStage> {
    let browser = Rc::new(build_disposable_browser(cfg)?);
    let page = new_provider_page(&browser, provider).await?;
    Ok(DisposableProviderStage { page, browser })
}

/// Prepare a saved snapshot **only in a private, no-disk Browser**. This has
/// no reference to the live Browser, cached Page, auth registry or vault writer,
/// so a failed cookie setter, page navigation or cancellation cannot mutate
/// live credentials. Returned Page/browser is an unverified candidate, NEVER
/// a verified account or an authorization to promote it into production.
#[allow(dead_code)] // The generation-fenced RefreshSession caller is #1463.
pub(super) async fn stage_provider_saved_snapshot(
    cfg: &AppConfig,
    provider: crate::providers::Provider,
    snapshot: &session::SessionFile,
) -> Result<DisposableProviderStage> {
    if !matches!(
        provider,
        crate::providers::Provider::ChatGPT | crate::providers::Provider::MetaAI
    ) {
        bail!("candidate_provider_not_in_recovery_scope");
    }
    // Keep source-owned validation before *any* Page allocation or navigation.
    // Discard untrusted error Display; this boundary never logs cookie metadata.
    session::preflight_cookie_restore(snapshot, provider.name())
        .map_err(|_| anyhow!("candidate_cookie_preflight_rejected"))?;
    let mut staged = tokio::time::timeout(
        Duration::from_secs(10),
        new_disposable_provider_stage(cfg, provider),
    )
    .await
    .map_err(|_| anyhow!("candidate_browser_creation_timeout"))?
    .map_err(|_| anyhow!("candidate_browser_creation_failed"))?;
    // Same canonical cookie → navigate → origin storage → reload path as the
    // live worker, but the Browser is the isolated candidate, never shared.
    super::PageSession::open(
        staged.browser.as_ref(),
        &mut staged.page,
        provider,
        Some(snapshot),
    )
    .await
    .map_err(|_| anyhow!("candidate_warmup_failed"))?;
    Ok(staged)
}

/// Apply page-level compatibility before any provider script executes.
pub(super) async fn new_page(browser: &obscura_core::Browser) -> Result<obscura_core::Page> {
    let mut page = browser.new_page().await?;
    page.add_preload_script(include_str!("iframe_sandbox.js"));
    page.add_preload_script(include_str!("window_compat.js"));
    Ok(page)
}

/// Attach provider-scoped compatibility before any provider navigation.
/// Unrelated providers retain the original embedded browser transport.
pub(super) async fn new_provider_page(
    browser: &obscura_core::Browser,
    provider: crate::providers::Provider,
) -> Result<obscura_core::Page> {
    let mut page = new_page(browser).await?;
    if provider == crate::providers::Provider::ChatGPT {
        crate::browser::chatgpt_websocket::attach(browser, &mut page);
    } else if provider == crate::providers::Provider::MetaAI {
        page.add_preload_script(include_str!("metaai_response.js"));
        crate::browser::chatgpt_websocket::attach_metaai(browser, &mut page);
    }
    Ok(page)
}

#[cfg(test)]
mod compatibility_tests {

    #[tokio::test]
    async fn provider_scripts_can_assign_iframe_sandbox_before_and_after_navigation() {
        let html = r#"<!doctype html><script>
        'use strict';
        const frame = document.createElement('iframe');
        const original = frame.sandbox;
        try {
          frame.sandbox = 'allow-scripts';
          globalThis.sandboxContract = original === frame.sandbox &&
            frame.getAttribute('sandbox') === 'allow-scripts' &&
            frame.sandbox.contains('allow-scripts') &&
            !frame.sandbox.contains('allow-same-origin');
        } catch { globalThis.sandboxContract = false; }
        </script>"#;
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        let url = format!("data:text/html,{encoded}");
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = super::new_page(&browser).await.unwrap();
        for _ in 0..2 {
            page.goto(&url).await.unwrap();
            assert_eq!(
                page.evaluate("globalThis.sandboxContract"),
                serde_json::json!(true)
            );
        }
    }

    #[tokio::test]
    async fn window_origin_is_available_before_first_script_and_after_navigation() {
        let html = r#"<!doctype html><script>
        'use strict';
        globalThis.originCompat = typeof window.origin === 'string' &&
          window.origin === new URL(window.location.href).origin;
        </script>"#;
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        let url = format!("data:text/html,{encoded}");
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = super::new_page(&browser).await.unwrap();
        for _ in 0..2 {
            page.goto(&url).await.unwrap();
            assert_eq!(
                page.evaluate("globalThis.originCompat"),
                serde_json::json!(true),
                "the page must expose window.origin before app scripts execute"
            );
        }
    }
}

#[cfg(test)]
mod disposable_context_tests {
    use super::*;

    fn cfg(explicit_dir: &str, legacy_dir: &str) -> AppConfig {
        serde_yaml::from_str(&format!(
            "server:\n  chrome_profile_dir: '{legacy_dir}'\nobscura:\n  stealth: false\n  user_agent: 'Gateway Synthetic UA/1.0'\n  storage_dir: '{explicit_dir}'\nproviders: []\n"
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn disposable_browsers_never_inherit_or_mutate_live_cookie_jar() {
        let cfg: AppConfig =
            serde_yaml::from_str("obscura:\n  stealth: false\nproviders: []\n").unwrap();
        let live = build_browser(&cfg).unwrap();
        live.cookies()
            .set(
                "SID=fixture-live; Secure; HttpOnly; Path=/",
                "https://chatgpt.com/",
            )
            .unwrap();

        let candidate = build_disposable_browser(&cfg).unwrap();
        assert!(candidate.cookies().get_all().is_empty());
        candidate
            .cookies()
            .set(
                "SID=fixture-candidate; Secure; HttpOnly; Path=/",
                "https://chatgpt.com/",
            )
            .unwrap();
        candidate
            .cookies()
            .set(
                "SID=fixture-meta; Secure; HttpOnly; Path=/",
                "https://meta.ai/",
            )
            .unwrap();

        // Browser::new_page() is intentionally used through the same factory
        // preloads, but pages from *different* Browsers cannot share the jar.
        let mut candidate_page = new_page(&candidate).await.unwrap();
        let mut live_page = new_page(&live).await.unwrap();
        let offline = "data:text/html,%3Chtml%3E%3Cbody%3Efixture%3C%2Fbody%3E%3C%2Fhtml%3E";
        candidate_page.goto(offline).await.unwrap();
        live_page.goto(offline).await.unwrap();
        assert_eq!(
            candidate_page.evaluate("document.body.textContent"),
            serde_json::json!("fixture")
        );
        assert_eq!(
            live_page.evaluate("document.body.textContent"),
            serde_json::json!("fixture")
        );

        let live_cookies = live.cookies().get_all();
        assert_eq!(live_cookies.len(), 1);
        assert_eq!(live_cookies[0].value, "fixture-live");
        assert_eq!(candidate.cookies().get_all().len(), 2);
        drop(candidate_page);
        drop(candidate);
        assert_eq!(live.cookies().get_all().len(), 1);
        assert_eq!(live.cookies().get_all()[0].value, "fixture-live");

        let cold_candidate = build_disposable_browser(&cfg).unwrap();
        assert!(cold_candidate.cookies().get_all().is_empty());
    }

    #[tokio::test]
    async fn disposable_provider_stage_owns_browser_and_page_until_drop() {
        let cfg: AppConfig =
            serde_yaml::from_str("obscura:\n  stealth: false\nproviders: []\n").unwrap();
        let live = build_browser(&cfg).unwrap();
        live.cookies()
            .set("SID=fixture-live; Secure; Path=/", "https://chatgpt.com/")
            .unwrap();

        let mut staged = new_disposable_provider_stage(&cfg, crate::providers::Provider::MetaAI)
            .await
            .unwrap();
        assert!(staged.browser().cookies().get_all().is_empty());
        staged
            .browser()
            .cookies()
            .set(
                "SID=fixture-candidate; Secure; Path=/",
                "https://chatgpt.com/",
            )
            .unwrap();
        staged
            .page_mut()
            .goto("data:text/html,%3Chtml%3E%3Cbody%3Eisolated%3C%2Fbody%3E%3C%2Fhtml%3E")
            .await
            .unwrap();
        assert_eq!(
            staged.page_mut().evaluate("document.body.textContent"),
            serde_json::json!("isolated")
        );

        // An abandoned candidate owns no part of the previous live jar.
        drop(staged);
        let cookies = live.cookies().get_all();
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].value, "fixture-live");

        let cold = new_disposable_provider_stage(&cfg, crate::providers::Provider::MetaAI)
            .await
            .unwrap();
        assert!(cold.browser().cookies().get_all().is_empty());
    }

    #[tokio::test]
    async fn staging_does_not_read_or_write_either_configured_profile_directory() {
        let root = std::env::temp_dir().join(format!(
            "chat-gateway-disposable-context-{}",
            uuid::Uuid::new_v4()
        ));
        let explicit = root.join("obscura");
        let fallback = root.join("chrome");
        let cfg = cfg(explicit.to_str().unwrap(), fallback.to_str().unwrap());
        {
            let candidate = build_disposable_browser(&cfg).unwrap();
            let mut page = new_page(&candidate).await.unwrap();
            page.goto("data:text/html,%3Chtml%3E%3C%2Fhtml%3E")
                .await
                .unwrap();
            assert_eq!(
                page.evaluate("navigator.userAgent"),
                serde_json::json!("Gateway Synthetic UA/1.0"),
            );
            candidate
                .cookies()
                .set("SID=fixture; Secure; Path=/", "https://chatgpt.com/")
                .unwrap();
            assert_eq!(candidate.cookies().get_all().len(), 1);
        }
        assert!(
            !explicit.exists(),
            "staging must not touch obscura.storage_dir"
        );
        assert!(
            !fallback.exists(),
            "staging must not touch chrome_profile_dir"
        );
        assert!(
            !root.exists(),
            "disposable context must not leave a profile"
        );
    }
}

#[cfg(test)]
mod staged_snapshot_preflight_tests {
    use super::*;
    use crate::providers::Provider;
    use serde_json::json;

    fn config() -> AppConfig {
        serde_yaml::from_str("obscura:\n  stealth: false\nproviders: []\n").unwrap()
    }

    fn cookie_with_invalid_same_site() -> session::SessionFile {
        serde_json::from_value(json!({
            "ts": "fixture",
            "local_storage": {},
            "cookies": [{
                "name": "SID",
                "value": "private-stage-fixture-do-not-log",
                "domain": ".chatgpt.com",
                "path": "/",
                "secure": true,
                "http_only": true,
                "same_site": "Invalid"
            }]
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn malformed_snapshot_rejected_before_new_context_or_live_jar_mutation() {
        let cfg = config();
        let live = build_browser(&cfg).unwrap();
        live.cookies()
            .set("SID=live-value; Secure; Path=/", "https://chatgpt.com/")
            .unwrap();
        let snapshot = cookie_with_invalid_same_site();
        let error = match stage_provider_saved_snapshot(&cfg, Provider::ChatGPT, &snapshot).await {
            Ok(_) => panic!("invalid cookie must not stage"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "candidate_cookie_preflight_rejected");
        assert!(!error.to_string().contains("private-stage-fixture"));
        let actual = live.cookies().get_all();
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].value, "live-value");
    }

    #[tokio::test]
    async fn deferred_provider_rejected_without_creating_or_loading_browser_context() {
        let cfg = config();
        let snapshot = cookie_with_invalid_same_site();
        for provider in [Provider::Grok, Provider::Gemini, Provider::Manus] {
            let error = match stage_provider_saved_snapshot(&cfg, provider, &snapshot).await {
                Ok(_) => panic!("deferred provider must not stage"),
                Err(error) => error,
            };
            assert_eq!(
                error.to_string(),
                "candidate_provider_not_in_recovery_scope"
            );
        }
    }

    #[tokio::test]
    async fn private_stage_transfer_preserves_same_browser_owner_as_page() {
        let cfg = config();
        let stage = new_disposable_provider_stage(&cfg, Provider::MetaAI)
            .await
            .unwrap();
        let (mut page, browser) = stage.into_owned_parts();
        browser
            .cookies()
            .set("SID=candidate-only; Secure; Path=/", "https://meta.ai/")
            .unwrap();
        page.goto("data:text/html,<body>staged</body>")
            .await
            .unwrap();
        assert_eq!(page.evaluate("document.body.textContent"), json!("staged"));
        assert_eq!(browser.cookies().get_all().len(), 1);
        let weak = Rc::downgrade(&browser);
        drop(page);
        drop(browser);
        assert!(weak.upgrade().is_none(), "candidate owner must be released");
    }
}

#[cfg(test)]
mod engine_regression_tests {
    #[tokio::test]
    async fn srcdoc_initialization_reenters_dom_without_aborting_provider_worker() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,%3Chtml%3E%3Cbody%3E%3C/body%3E%3C/html%3E")
            .await
            .unwrap();
        page.evaluate(r#"document.body.innerHTML = '<iframe srcdoc="<p>frame</p>"></iframe>';"#);
        page.settle(50).await;
        assert_eq!(
            page.evaluate("document.querySelector('iframe').contentDocument.body.textContent")
                .as_str(),
            Some("frame")
        );
        page.evaluate(
            r#"(() => {
                document.body.innerHTML = '';
                const frame = document.createElement('iframe');
                frame.srcdoc = '<body><span>inner</span></body>';
                document.body.appendChild(frame);
            })()"#,
        );
        page.settle(50).await;
        assert_eq!(
            page.evaluate(
                r#"(() => {
                    const span = document.querySelector('iframe').contentDocument.querySelector('span');
                    span.textContent = 'touched';
                    return span.textContent;
                })()"#,
            )
            .as_str(),
            Some("touched")
        );
    }

    #[tokio::test]
    async fn pinned_meta_response_body_keeps_the_original_consumed_stream() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<body>response contract fixture</body>")
            .await
            .unwrap();
        // Permit only this data-origin fixture; production script stays
        // exact-HTTPS Meta AI scoped and leaves native Response untouched.
        let script = include_str!("metaai_response.js")
            .replace(
                "location.protocol !== 'https:'",
                "location.protocol !== 'data:'",
            )
            .replace("['meta.ai', 'www.meta.ai']", "['']");
        page.evaluate(&script);
        let result = page.evaluate(
            r#"(() => {
                const response = new Response('synthetic body');
                const stream = response.body;
                stream.getReader();
                return {same: response.body === stream, used: response.bodyUsed};
            })()"#,
        );
        assert_eq!(result["same"].as_bool(), Some(true));
        assert_eq!(result["used"].as_bool(), Some(true));
    }
}
