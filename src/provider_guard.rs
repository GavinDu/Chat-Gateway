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

/// A quota reservation is not an admitted request until its paced wait ends.
/// Dropping a canceled async future releases only its matching UTC-day slot.
/// The conservative next_start timestamp is retained to preserve spacing.
struct PendingRequestReservation {
    budgets: Arc<Mutex<HashMap<String, ProviderBudget>>>,
    provider: String,
    utc_day: u64,
    committed: bool,
}

impl Drop for PendingRequestReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Ok(mut budgets) = self.budgets.lock() {
            if let Some(budget) = budgets.get_mut(&self.provider) {
                if budget.utc_day == self.utc_day {
                    budget.daily_requests = budget.daily_requests.saturating_sub(1);
                }
            }
        }
    }
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
        // Scope the permit across await: an abort/timeout must not silently
        // spend a provider's daily request quota without admitting work.
        let mut pending = PendingRequestReservation {
            budgets: Arc::clone(&self.budgets),
            provider: provider.to_owned(),
            utc_day,
            committed: false,
        };
        tokio::time::sleep(wait + Duration::from_millis(jitter_ms.into())).await;
        {
            // A challenge detected while queued must still deny the request.
            // Recheck after sleep without holding the mutex across await.
            let mut budgets = self
                .budgets
                .lock()
                .map_err(|_| anyhow::anyhow!("provider pacing state is unavailable"))?;
            let entry = budgets
                .get_mut(provider)
                .ok_or_else(|| anyhow::anyhow!("provider pacing state is unavailable"))?;
            if let Some(reason) = entry.disabled_reason {
                anyhow::bail!("provider '{provider}' is disabled: {reason}");
            }
            // A long queued wait crossing UTC midnight must consume today's
            // cap, not an expired reservation on yesterday's budget.
            let today = current_utc_day();
            if entry.utc_day != today {
                entry.utc_day = today;
                entry.daily_requests = 0;
            }
            if pending.utc_day != today {
                if entry.daily_requests >= daily_cap {
                    anyhow::bail!("provider '{provider}' reached its daily request cap");
                }
                entry.daily_requests += 1;
                pending.utc_day = today;
            }
        }
        pending.committed = true;
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
    // Only a security *route* is challenge evidence. Conversation IDs like
    // /c/challenge or /chat/captcha must never trip a sticky provider breaker.
    // A CDN challenge is mounted under /cdn-cgi/challenge-platform; preserve
    // the known Google sign-in challenge route without scanning arbitrary paths.
    let mut segments = uri.path().trim_start_matches('/').split('/');
    let root = segments.next().unwrap_or_default().to_ascii_lowercase();
    let second = segments.next().unwrap_or_default().to_ascii_lowercase();
    matches!(
        root.as_str(),
        "captcha"
            | "challenge"
            | "checkpoint"
            | "verify-human"
            | "verify_you_are_human"
            | "challenge-platform"
    ) || (root == "cdn-cgi" && second == "challenge-platform")
        || (host == "accounts.google.com"
            && root == "v3"
            && second == "signin"
            && segments
                .next()
                .is_some_and(|segment| segment.eq_ignore_ascii_case("challenge")))
}

/// Only well-known, strictly authenticated *external* challenge endpoints
/// may disable an adapter while the browser is off the provider's own origin.
/// A foreign page with a generic /captcha path is not provider evidence.
pub(crate) fn is_known_external_challenge_url(url: &str) -> bool {
    let Ok(uri) = url.parse::<axum::http::Uri>() else {
        return false;
    };
    if uri.scheme_str() != Some("https") {
        return false;
    }
    let (Some(host), Some(authority)) = (uri.host(), uri.authority()) else {
        return false;
    };
    if host.is_empty() || host.ends_with("..") {
        return false;
    }
    if !authority.as_str().eq_ignore_ascii_case(host)
        && !authority
            .as_str()
            .eq_ignore_ascii_case(&format!("{host}:443"))
    {
        return false;
    }
    let normalized_host = host.trim_end_matches('.').to_ascii_lowercase();
    match normalized_host.as_str() {
        "challenges.cloudflare.com" | "challenge.cloudflare.com" => true,
        "accounts.google.com" => {
            let path = uri.path();
            path.eq_ignore_ascii_case("/v3/signin/challenge")
                || path
                    .to_ascii_lowercase()
                    .starts_with("/v3/signin/challenge/")
        }
        _ => false,
    }
}

/// Visible security text outside known chat-turn containers. A user or
/// assistant quoting challenge wording must never trip the sticky provider
/// circuit breaker. True verification overlays remain in the scan, and
/// explicit challenge URLs are always checked independently.
pub(crate) const NON_MESSAGE_CHALLENGE_TEXT_JS: &str = r#"(() => {
    if (!document.body) return '';
    // A chat draft or prior assistant reply quoting CAPTCHA wording is not a
    // security overlay. Use explicit message-role roots already recognized
    // by the provider adapters; do not exclude generic dialogs or page text.
    const messages = "[data-message-author-role], [data-message-role='assistant'], [data-message-role='user'], [data-role='assistant'], [data-role='user'], [data-testid='assistant-message'], [data-testid='user-message'], [data-testid*='assistant-response'], [data-testid*='conversation-turn']";
    const editors = "[contenteditable='true'], [contenteditable='plaintext-only'], [contenteditable=''], [role='textbox'], textarea, input";
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    let text = '';
    let visited = 0;
    while (walker.nextNode() && visited++ < 16000 && text.length < 12000) {
        const node = walker.currentNode;
        const parent = node.parentElement;
        if (!parent || parent.closest(messages) || parent.closest(editors) ||
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

    #[tokio::test]
    async fn timed_out_pacing_wait_refunds_only_the_unstarted_reservation() {
        let guard = ProviderGuard::default();
        guard
            .wait_for_request_slot("cancelled-pace", 250, 2)
            .await
            .unwrap();
        // The second call must queue behind the first's bounded interval.
        let outcome = tokio::time::timeout(
            Duration::from_millis(10),
            guard.wait_for_request_slot("cancelled-pace", 250, 2),
        )
        .await;
        assert!(outcome.is_err(), "second request should still be queued");
        let count = guard
            .budgets
            .lock()
            .unwrap()
            .get("cancelled-pace")
            .unwrap()
            .daily_requests;
        assert_eq!(count, 1, "only the admitted request consumes quota");
        // Reusing the refunded cap cannot bypass the provider's configured
        // limit; this third request must still pass the reserved paced start.
        guard
            .wait_for_request_slot("cancelled-pace", 250, 2)
            .await
            .unwrap();
        assert!(guard
            .wait_for_request_slot("cancelled-pace", 250, 2)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn challenge_during_queued_wait_rejects_and_refunds() {
        let guard = ProviderGuard::default();
        guard
            .wait_for_request_slot("late-challenge", 250, 3)
            .await
            .unwrap();
        let worker = guard.clone();
        let pending =
            tokio::spawn(
                async move { worker.wait_for_request_slot("late-challenge", 250, 3).await },
            );
        let mut reserved = false;
        for _ in 0..100 {
            reserved = guard
                .budgets
                .lock()
                .unwrap()
                .get("late-challenge")
                .is_some_and(|entry| entry.daily_requests == 2);
            if reserved {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(reserved, "pending request must reserve before challenge");
        guard.disable_provider("late-challenge", "challenge page detected");
        let result = tokio::time::timeout(Duration::from_secs(3), pending)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
        assert_eq!(
            guard
                .budgets
                .lock()
                .unwrap()
                .get("late-challenge")
                .unwrap()
                .daily_requests,
            1,
            "blocked queued work cannot consume admission quota"
        );
    }

    #[test]
    fn cancelled_old_day_does_not_refund_new_day_usage() {
        let guard = ProviderGuard::default();
        let current_day = current_utc_day();
        let old_day = current_day.saturating_sub(1);
        guard.budgets.lock().unwrap().insert(
            "day-boundary".to_owned(),
            ProviderBudget {
                next_start: Instant::now(),
                utc_day: old_day,
                daily_requests: 1,
                disabled_reason: None,
            },
        );
        let old = PendingRequestReservation {
            budgets: Arc::clone(&guard.budgets),
            provider: "day-boundary".to_owned(),
            utc_day: old_day,
            committed: false,
        };
        {
            let mut budgets = guard.budgets.lock().unwrap();
            let entry = budgets.get_mut("day-boundary").unwrap();
            entry.utc_day = current_day;
            entry.daily_requests = 2;
        }
        drop(old);
        assert_eq!(
            guard
                .budgets
                .lock()
                .unwrap()
                .get("day-boundary")
                .unwrap()
                .daily_requests,
            2
        );
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
        assert!(
            !super::is_challenge_page(&page),
            "chat-turn text is not a challenge"
        );
        page.evaluate("document.body.insertAdjacentHTML('beforeend', '<div role=dialog>Verify you are human to continue</div>')");
        assert!(
            super::is_challenge_page(&page),
            "visible verification dialog blocks"
        );
        page.evaluate("document.querySelector('[role=dialog]').hidden = true");
        assert!(
            !super::is_challenge_page(&page),
            "hidden challenge text is not a blocker"
        );
    }

    #[tokio::test]
    async fn editable_drafts_and_manus_meta_role_replies_are_not_challenge_overlays() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <div id="composer" contenteditable="true" role="textbox">
                <span>Verify you are human to continue</span>
            </div>
            <div id="rich-draft" contenteditable="plaintext-only">
                Complete the captcha
            </div>
            <div id="role-draft" role="textbox">Unusual traffic</div>
            <textarea id="native-draft">Security challenge</textarea>
            <div data-message-role="assistant" class="answer">
                A prior reply explains checking your browser
            </div>
            <div data-role="assistant">A security challenge is a verification step</div>
            <div data-role="user">Please verify you are human</div>
            <div data-testid="assistant-response-27">
                Confirm you are not a robot is a quotation
            </div>
            <div id="manual-dialog" role="dialog" hidden>
                Verify you are human
            </div>
        </body></html>"#;
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        assert!(
            page.evaluate(NON_MESSAGE_CHALLENGE_TEXT_JS)
                .as_str()
                .is_some(),
            "the static body scanner must evaluate successfully in pinned Obscura"
        );
        assert!(
            !is_challenge_page(&page),
            "visible user-authored draft and completed answer text are not security UI"
        );
        let before = crate::providers::common::response_probe(
            &page,
            &["[data-message-role='assistant']"],
            true,
        );
        assert!(!before.challenge, "polling must agree with the entry guard");

        page.evaluate(
            "document.querySelector('#composer span').textContent = 'Checking your browser now'",
        );
        assert!(
            !is_challenge_page(&page),
            "a changing rich-text draft must not persistently disable the provider"
        );
        assert!(
            !crate::providers::common::response_probe(
                &page,
                &["[data-message-role='assistant']"],
                true,
            )
            .challenge
        );

        // Pinned Obscura does not reliably reflow a previously hidden node
        // after a synthetic DOM attribute mutation. Use a freshly navigated,
        // initially visible security dialog to test the actual scanner.
        let challenge_html = html.replace(
            "id=\"manual-dialog\" role=\"dialog\" hidden",
            "id=\"manual-dialog\" role=\"dialog\" data-message-role=\"system\"",
        );
        assert_ne!(challenge_html, html, "challenge fixture must differ");
        let challenge_encoded: String = challenge_html
            .bytes()
            .map(|byte| format!("%{byte:02X}"))
            .collect();
        page.goto(&format!("data:text/html,{challenge_encoded}"))
            .await
            .unwrap();
        let security_text = page.evaluate(NON_MESSAGE_CHALLENGE_TEXT_JS);
        assert!(
            security_text
                .as_str()
                .is_some_and(|value| value.contains("Verify you are human")),
            "visible system manual verification must remain in the scanned body text"
        );
        assert!(
            is_challenge_page(&page),
            "visible verification dialog outside the draft/message still requires manual handling"
        );
        assert!(
            crate::providers::common::response_probe(
                &page,
                &["[data-message-role='assistant']"],
                true,
            )
            .challenge
        );
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        assert!(!is_challenge_page(&page), "hidden dialogs are excluded");
    }

    #[test]
    fn foreign_challenge_authority_must_be_exact_https_identity() {
        for url in [
            "https://challenges.cloudflare.com/",
            "https://challenge.cloudflare.com/captcha",
            "https://challenges.cloudflare.com:443/",
            "https://accounts.google.com/v3/signin/challenge",
            "https://accounts.google.com/v3/signin/challenge/continue",
        ] {
            assert!(
                is_known_external_challenge_url(url),
                "known external challenge must retain sticky protection: {url}"
            );
        }
        for url in [
            "https://attacker.invalid/captcha",
            "https://attacker.invalid/challenge",
            "https://other-provider.invalid/verify-human",
            "http://challenges.cloudflare.com/",
            "https://challenges.cloudflare.com:444/",
            "https://challenges.cloudflare.com:65536/",
            "https://x@challenges.cloudflare.com/",
            "https://challenges.cloudflare.com.attacker.invalid/",
            "https://challenges.cloudflare.com../",
            "https://accounts.google.com/v3/signin",
            "https://accounts.google.com/captcha",
            "https://accounts.google.com:444/v3/signin/challenge",
            "https://accounts.google.com.attacker.invalid/v3/signin/challenge",
            "data:text/html,<div>captcha</div>",
        ] {
            assert!(
                !is_known_external_challenge_url(url),
                "unverified challenge URL must not disable a provider: {url}"
            );
        }
    }

    #[test]
    fn challenge_url_detection_ignores_non_route_keywords() {
        for url in [
            "https://chatgpt.com/c/challenge",
            "https://grok.com/chat/captcha",
            "https://manus.im/app/checkpoint",
            "https://gemini.google.com/app/verify-human",
            "https://claude.ai/new/challenge",
            "https://chat.qwen.ai/chat/verify_you_are_human",
            "https://www.meta.ai/chat/challenge-platform",
            "https://www.perplexity.ai/search/checkpoint",
            "https://example.test/blog/cdn-cgi/challenge-platform",
            "https://challenges.cloudflare.com.attacker.invalid/chat/checkpoint",
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
            "https://accounts.google.com/v3/signin/challenge",
            "https://example.test/verify_you_are_human",
            "https://example.test/challenge-platform/start",
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
