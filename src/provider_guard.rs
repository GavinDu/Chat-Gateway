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

fn has_challenge_marker(url: &str, body: &str) -> bool {
    let url = url.to_ascii_lowercase();
    let body = body.to_ascii_lowercase();
    [
        "captcha",
        "challenge",
        "checkpoint",
        "verify-human",
        "verify_you_are_human",
    ]
    .iter()
    .any(|needle| url.contains(needle))
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
    let evaluation = page.evaluate("document.body ? document.body.innerText.slice(0, 12000) : ''");
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
