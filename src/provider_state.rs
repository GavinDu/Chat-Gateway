//! In-process provider session state derived from actual send outcomes.
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

/// Typed provider lifecycle state. Keep the string representation at the
/// observability/API boundary rather than using strings for internal logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderSessionState {
    Unknown,
    Ready,
    NeedsLogin,
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
}

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
        })
}

pub fn record_success(provider: &str) {
    if let Ok(mut map) = states().lock() {
        map.insert(
            provider.to_owned(),
            ProviderSessionStatus {
                state: ProviderSessionState::Ready,
                last_error: None,
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
    if !indicates_expired_session(error) {
        return;
    }
    if let Ok(mut map) = states().lock() {
        map.insert(
            provider.to_owned(),
            ProviderSessionStatus {
                state: ProviderSessionState::NeedsLogin,
                last_error: Some("provider session expired; re-login required".to_owned()),
            },
        );
    }
}

pub fn is_routable(provider: &str) -> bool {
    status(provider).state != ProviderSessionState::NeedsLogin
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
    fn ignores_unrelated_authorization_and_network_errors() {
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
                ProviderSessionState::Unknown,
                "{message}"
            );
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
        assert_eq!(status("test-provider-unknown").state, ProviderSessionState::Unknown);
    }

    #[test]
    fn only_expired_sessions_enter_needs_login() {
        let provider = "test-provider-session-state";
        record_error(provider, "temporary network timeout");
        assert_eq!(status(provider).state, ProviderSessionState::Unknown);
        record_error(provider, "session_expired: login required");
        assert_eq!(status(provider).state, ProviderSessionState::NeedsLogin);
        assert!(!is_routable(provider));
        record_success(provider);
        assert_eq!(status(provider).state, ProviderSessionState::Ready);
        assert!(is_routable(provider));
    }

    #[test]
    fn state_string_conversion_is_stable_for_metrics_and_health() {
        assert_eq!(ProviderSessionState::Unknown.as_str(), "unknown");
        assert_eq!(ProviderSessionState::Ready.as_str(), "ready");
        assert_eq!(ProviderSessionState::NeedsLogin.as_str(), "needs_login");
        assert_eq!(ProviderSessionState::Degraded.as_str(), "degraded");
    }
}
