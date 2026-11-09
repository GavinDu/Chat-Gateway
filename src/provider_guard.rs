//! Provider request pacing and fail-closed challenge-page circuit breaker.
use obscura_core::Page;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
struct ProviderBudget {
    next_start: Instant,
    utc_day: u64,
    daily_requests: u32,
    disabled_reason: Option<&'static str>,
}

/// Provider-scoped runtime guard. Clones share state within one gateway
/// instance; separate gateway instances no longer share process-global budgets.
#[derive(Clone, Debug, Default)]
pub struct ProviderGuard {
    budgets: Arc<Mutex<HashMap<String, ProviderBudget>>>,
}

impl ProviderGuard {
    /// Reserve a paced request slot. Enforces a UTC-day cap before sending.
    pub async fn wait_for_request_slot(
        &self,
        provider: &str,
        min_interval_ms: u64,
        daily_cap: u32,
    ) -> anyhow::Result<()> {
        if min_interval_ms == 0 || daily_cap == 0 {
            anyhow::bail!("provider pacing interval and daily cap must be greater than zero");
        }
        let now = Instant::now();
        let utc_day = current_utc_day();
        let (wait, jitter_ms) = {
            let mut map = self
                .budgets
                .lock()
                .map_err(|_| anyhow::anyhow!("provider pacing state is unavailable"))?;
            let entry = map.entry(provider.to_owned()).or_insert(ProviderBudget {
                next_start: now,
                utc_day,
                daily_requests: 0,
                disabled_reason: None,
            });
            if let Some(reason) = entry.disabled_reason {
                anyhow::bail!("provider '{provider}' is disabled: {reason}");
            }
            if entry.utc_day != utc_day {
                entry.utc_day = utc_day;
                entry.daily_requests = 0;
            }
            if entry.daily_requests >= daily_cap {
                anyhow::bail!("provider '{provider}' reached its daily request cap");
            }
            entry.daily_requests += 1;
            let scheduled = entry.next_start.max(now);
            // Reserve the full jitter window so the actual start-to-start gap
            // can never be shorter than the configured minimum interval.
            entry.next_start =
                scheduled + Duration::from_millis(min_interval_ms.saturating_add(250));
            let wait = scheduled.saturating_duration_since(now);
            let jitter_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_millis()
                % 251;
            (wait, jitter_ms)
        };
        tokio::time::sleep(wait + Duration::from_millis(jitter_ms.into())).await;
        Ok(())
    }

    pub fn disable_provider(&self, provider: &str, reason: &'static str) {
        if let Ok(mut map) = self.budgets.lock() {
            let entry = map.entry(provider.to_owned()).or_insert(ProviderBudget {
                next_start: Instant::now(),
                utc_day: current_utc_day(),
                daily_requests: 0,
                disabled_reason: None,
            });
            entry.disabled_reason = Some(reason);
        }
    }

    /// Explicit owner reset of the challenge circuit breaker. The pacing and
    /// daily-request budgets are deliberately retained so a reset cannot
    /// bypass provider rate limits.
    pub fn reset_challenge(&self, provider: &str) -> anyhow::Result<()> {
        let mut budgets = self
            .budgets
            .lock()
            .map_err(|_| anyhow::anyhow!("provider guard unavailable"))?;
        if let Some(budget) = budgets.get_mut(provider) {
            budget.disabled_reason = None;
        }
        Ok(())
    }

    pub fn provider_disabled_reason(&self, provider: &str) -> Option<&'static str> {
        self.budgets
            .lock()
            .ok()?
            .get(provider)
            .and_then(|entry| entry.disabled_reason)
    }
}

fn current_utc_day() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400
}

/// Only a challenge host or URL path is evidence of a challenge route.
/// Query parameters (including user prompts, redirect targets or conversation
/// IDs) must never trip the sticky provider-wide challenge circuit breaker.
pub(crate) fn is_challenge_url(url: &str) -> bool {
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
        "challenges.cloudflare.com" | "challenge.cloudflare.com"
    ) {
        return true;
    }
    uri.path().split('/').any(|segment| {
        matches!(
            segment.to_ascii_lowercase().as_str(),
            "captcha"
                | "challenge"
                | "checkpoint"
                | "verify-human"
                | "verify_you_are_human"
                | "challenge-platform"
        )
    })
}

/// Visible security text outside known chat-turn containers. A user or
/// assistant quoting challenge wording must never trip the sticky provider
/// circuit breaker. True verification overlays remain in the scan, and
/// explicit challenge URLs are always checked independently.
pub(crate) const NON_MESSAGE_CHALLENGE_TEXT_JS: &str = r#"(() => {
    if (!document.body) return '';
    const messages = "[data-message-author-role], [data-testid='assistant-message'], [data-testid='user-message'], [data-testid*='conversation-turn']";
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    let text = '';
    let visited = 0;
    while (walker.nextNode() && visited++ < 16000 && text.length < 12000) {
        const node = walker.currentNode;
        const parent = node.parentElement;
        if (!parent || parent.closest(messages) ||
            parent.closest('[hidden], [inert], [aria-hidden="true"]') ||
            !parent.getClientRects().length ||
            window.getComputedStyle(parent).visibility === 'hidden') continue;
        const part = node.textContent || '';
        text += part.slice(0, 12000 - text.length) + ' ';
    }
    return text;
})()"#;

fn has_challenge_marker(url: &str, body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    is_challenge_url(url)
        || [
            "verify you are human",
            "confirm you are not a robot",
            "unusual traffic",
            "complete the captcha",
            "security challenge",
            "checking your browser",
        ]
        .iter()
        .any(|needle| body.contains(needle))
}

/// Detect challenge pages without attempting to solve or bypass them.
pub fn is_challenge_page(page: &Page) -> bool {
    let url = page.url();
    let evaluation = page.evaluate(NON_MESSAGE_CHALLENGE_TEXT_JS);
    let body = evaluation.as_str().unwrap_or("");
    has_challenge_marker(&url, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_zero_budget_values() {
        let guard = ProviderGuard::default();
        assert!(guard
            .wait_for_request_slot("test-zero-interval", 0, 1)
            .await
            .is_err());
        assert!(guard
            .wait_for_request_slot("test-zero-cap", 1, 0)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn enforces_daily_cap_per_guard_instance() {
        let guard = ProviderGuard::default();
        let provider = "test-daily-cap";
        guard.wait_for_request_slot(provider, 1, 1).await.unwrap();
        assert!(guard.wait_for_request_slot(provider, 1, 1).await.is_err());
    }

    #[tokio::test]
    async fn cloned_guard_shares_budget_but_separate_guard_does_not() {
        let first = ProviderGuard::default();
        let clone = first.clone();
        let second = ProviderGuard::default();
        first
            .wait_for_request_slot("shared-budget", 1, 1)
            .await
            .unwrap();
        assert!(clone
            .wait_for_request_slot("shared-budget", 1, 1)
            .await
            .is_err());
        assert!(second
            .wait_for_request_slot("shared-budget", 1, 1)
            .await
            .is_ok());
    }

    #[test]
    fn challenge_detector_matches_security_prompts_but_not_normal_chat_pages() {
        assert!(has_challenge_marker("https://example.test/captcha", ""));
        assert!(has_challenge_marker(
            "https://example.test/",
            "Please verify you are human to continue"
        ));
        assert!(has_challenge_marker(
            "https://example.test/",
            "Unusual traffic detected"
        ));
        assert!(!has_challenge_marker(
            "https://example.test/chat",
            "Welcome back. How can I help?"
        ));
    }

    #[tokio::test]
    async fn quoted_security_phrase_in_chat_does_not_block_but_visible_overlay_does() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <main>
              <div data-message-author-role="user">Explain the phrase verify you are human</div>
              <div data-message-author-role="assistant">A security challenge is a verification step</div>
              <div data-testid="conversation-turn-42">Checking your browser is a quoted instruction</div>
              <textarea id="composer"></textarea>
            </main>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        assert!(!super::is_challenge_page(&page), "chat-turn text is not a challenge");
        page.evaluate("document.body.insertAdjacentHTML('beforeend', '<div role=dialog>Verify you are human to continue</div>')");
        assert!(super::is_challenge_page(&page), "visible verification dialog blocks");
        page.evaluate("document.querySelector('[role=dialog]').hidden = true");
        assert!(!super::is_challenge_page(&page), "hidden challenge text is not a blocker");
    }

    #[test]
    fn challenge_url_detection_ignores_non_route_keywords() {
        for url in [
            "https://chatgpt.com/c/challenge-essay",
            "https://gemini.google.com/app?q=security%20challenge",
            "https://grok.com/chat?next=https%3A%2F%2Fexample.test%2Fcaptcha",
            "https://challenges.cloudflare.com.attacker.invalid/",
            "data:text/html,<div>challenge</div>",
        ] {
            assert!(!is_challenge_url(url), "false challenge redirect: {url}");
            assert!(
                !has_challenge_marker(url, ""),
                "false challenge page: {url}"
            );
        }
        for url in [
            "https://example.test/cdn-cgi/challenge-platform/h/g/",
            "https://example.test/verify-human",
            "https://example.test/checkpoint?next=/chat",
            "https://challenges.cloudflare.com/",
        ] {
            assert!(is_challenge_url(url), "missed challenge route: {url}");
        }
    }

    #[tokio::test]
    async fn admin_challenge_reset_keeps_daily_request_quota() {
        let guard = ProviderGuard::default();
        guard.disable_provider("provider", "challenge page detected");
        assert!(guard.provider_disabled_reason("provider").is_some());
        guard.reset_challenge("provider").unwrap();
        assert_eq!(guard.provider_disabled_reason("provider"), None);
        guard.wait_for_request_slot("provider", 1, 1).await.unwrap();
        assert!(guard.wait_for_request_slot("provider", 1, 1).await.is_err());
        guard.reset_challenge("provider").unwrap();
        assert!(guard.wait_for_request_slot("provider", 1, 1).await.is_err());
    }

    #[test]
    fn challenge_kill_switch_is_sticky_and_instance_scoped() {
        let first = ProviderGuard::default();
        let second = ProviderGuard::default();
        assert_eq!(first.provider_disabled_reason("provider-a"), None);
        first.disable_provider("provider-a", "challenge page detected");
        assert_eq!(
            first.provider_disabled_reason("provider-a"),
            Some("challenge page detected")
        );
        assert_eq!(second.provider_disabled_reason("provider-a"), None);
    }
}
