//! In-process provider session state derived from actual send outcomes.
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime},
};

/// Typed provider lifecycle state. Keep the string representation at the
/// observability/API boundary rather than using strings for internal logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderSessionState {
    Unknown,
    Ready,
    NeedsLogin,
    #[allow(dead_code)] // Used when transient failures gain explicit state transitions.
    Degraded,
}

impl ProviderSessionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Ready => "ready",
            Self::NeedsLogin => "needs_login",
            Self::Degraded => "degraded",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderSessionStatus {
    pub state: ProviderSessionState,
    pub last_error: Option<String>,
    /// Earliest time a degraded provider may receive a half-open retry.
    /// Expired sessions are never re-admitted by timeout alone.
    pub retry_after: Option<SystemTime>,
}

const DEGRADED_RETRY_COOLDOWN: Duration = Duration::from_secs(15);

static STATES: OnceLock<Mutex<HashMap<String, ProviderSessionStatus>>> = OnceLock::new();
fn states() -> &'static Mutex<HashMap<String, ProviderSessionStatus>> {
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn status(provider: &str) -> ProviderSessionStatus {
    states()
        .lock()
        .ok()
        .and_then(|map| map.get(provider).cloned())
        .unwrap_or(ProviderSessionStatus {
            state: ProviderSessionState::Unknown,
            last_error: None,
            retry_after: None,
        })
}

pub fn record_success(provider: &str) {
    if let Ok(mut map) = states().lock() {
        map.insert(
            provider.to_owned(),
            ProviderSessionStatus {
                state: ProviderSessionState::Ready,
                last_error: None,
                retry_after: None,
            },
        );
    }
}

fn indicates_expired_session(error: &str) -> bool {
    let normalized = error
        .to_ascii_lowercase()
        .replace(['_', '-', ':', '.'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    [
        "session expired",
        "session has expired",
        "login required",
        "please log in",
        "please sign in",
        "sign in to continue",
        "authentication required",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

pub fn record_error(provider: &str, error: &str) {
    let expired = indicates_expired_session(error);
    let retry_after = if expired {
        None
    } else {
        SystemTime::now().checked_add(DEGRADED_RETRY_COOLDOWN)
    };
    record_error_state(provider, expired, retry_after);
}

fn record_error_state(provider: &str, expired: bool, retry_after: Option<SystemTime>) {
    if let Ok(mut map) = states().lock() {
        map.insert(
            provider.to_owned(),
            ProviderSessionStatus {
                state: if expired {
                    ProviderSessionState::NeedsLogin
                } else {
                    ProviderSessionState::Degraded
                },
                // Never retain upstream error text: it can contain credentials or PII.
                last_error: Some(if expired {
                    "provider session expired; re-login required".to_owned()
                } else {
                    "provider request failed; retry may succeed".to_owned()
                }),
                retry_after,
            },
        );
    }
}

/// Degraded providers are excluded during cooldown, then re-admitted for a
/// half-open retry. A failed retry starts a fresh cooldown. Expired sessions
/// require successful re-authentication and are never re-admitted by timeout.
pub fn is_routable(provider: &str) -> bool {
    let now = SystemTime::now();
    let Ok(mut map) = states().lock() else {
        return false;
    };
    let Some(current) = map.get_mut(provider) else {
        return true;
    };
    match current.state {
        ProviderSessionState::NeedsLogin => false,
        ProviderSessionState::Degraded => {
            if current
                .retry_after
                .is_some_and(|retry_after| now >= retry_after)
            {
                // Atomically claim the half-open slot so concurrent requests
                // cannot all hammer a provider at the end of the cooldown.
                current.retry_after = now.checked_add(DEGRADED_RETRY_COOLDOWN);
                true
            } else {
                false
            }
        }
        ProviderSessionState::Unknown | ProviderSessionState::Ready => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_common_session_expiry_wording() {
        for (index, message) in [
            "Session expired",
            "Your session has expired.",
            "login-required",
            "Please sign in to continue",
            "Authentication required",
        ]
        .iter()
        .enumerate()
        {
            let provider = format!("test-expiry-wording-{index}");
            record_error(&provider, message);
            assert_eq!(
                status(&provider).state,
                ProviderSessionState::NeedsLogin,
                "{message}"
            );
        }
    }

    #[test]
    fn records_retryable_failures_as_degraded_without_blocking_routing() {
        for (index, message) in [
            "temporary network timeout",
            "request unauthorized by policy",
            "rate limit exceeded",
        ]
        .iter()
        .enumerate()
        {
            let provider = format!("test-non-expiry-{index}");
            record_error(&provider, message);
            assert_eq!(
                status(&provider).state,
                ProviderSessionState::Degraded,
                "{message}"
            );
            assert!(
                !is_routable(&provider),
                "{message} should enter the degraded cooldown"
            );
            let retry_after = status(&provider).retry_after.unwrap();
            record_error_state(&provider, false, Some(SystemTime::UNIX_EPOCH));
            assert!(
                is_routable(&provider),
                "{message} should be half-open after cooldown"
            );
            assert!(
                !is_routable(&provider),
                "{message} must have only one half-open request"
            );
            assert!(status(&provider).retry_after.unwrap() > SystemTime::now());
            assert!(retry_after > SystemTime::now());
        }
    }

    #[test]
    // Session diagnostics are user-visible state; never retain raw cookies or identity data.
    fn session_expiry_details_are_redacted_and_success_clears_them() {
        let provider = "test-provider-redacted-session-error";
        record_error(
            provider,
            "session expired: cookie=secret-session-token; user=private@example.test",
        );

        let expired = status(provider);
        assert_eq!(expired.state, ProviderSessionState::NeedsLogin);
        let safe_error = expired
            .last_error
            .expect("expiry state should explain remediation");
        assert_eq!(safe_error, "provider session expired; re-login required");
        assert!(!safe_error.contains("secret-session-token"));
        assert!(!safe_error.contains("private@example.test"));

        record_success(provider);
        let ready = status(provider);
        assert_eq!(ready.state, ProviderSessionState::Ready);
        assert_eq!(ready.last_error, None);
    }

    #[test]
    fn unknown_provider_is_routable_until_checked() {
        assert!(is_routable("test-provider-unknown"));
        assert_eq!(
            status("test-provider-unknown").state,
            ProviderSessionState::Unknown
        );
    }

    #[test]
    fn only_expired_sessions_enter_needs_login() {
        let provider = "test-provider-session-state";
        record_error(provider, "temporary network timeout");
        assert_eq!(status(provider).state, ProviderSessionState::Degraded);
        assert!(!is_routable(provider));
        record_error(provider, "session_expired: login required");
        assert_eq!(status(provider).state, ProviderSessionState::NeedsLogin);
        assert!(!is_routable(provider));
        record_success(provider);
        assert_eq!(status(provider).state, ProviderSessionState::Ready);
        assert!(is_routable(provider));
        assert_eq!(status(provider).retry_after, None);
    }

    #[test]
    fn half_open_retry_is_claimed_by_only_one_concurrent_request() {
        let provider = "test-provider-half-open-race";
        record_error_state(provider, false, Some(SystemTime::UNIX_EPOCH));

        let workers = (0..16)
            .map(|_| std::thread::spawn(|| is_routable(provider)))
            .collect::<Vec<_>>();
        let admitted = workers
            .into_iter()
            .map(|worker| worker.join().expect("routing worker should finish"))
            .filter(|admitted| *admitted)
            .count();

        assert_eq!(
            admitted, 1,
            "only one concurrent request may claim the half-open slot"
        );
    }

    #[test]
    fn state_string_conversion_is_stable_for_metrics_and_health() {
        assert_eq!(ProviderSessionState::Unknown.as_str(), "unknown");
        assert_eq!(ProviderSessionState::Ready.as_str(), "ready");
        assert_eq!(ProviderSessionState::NeedsLogin.as_str(), "needs_login");
        assert_eq!(ProviderSessionState::Degraded.as_str(), "degraded");
    }
}
