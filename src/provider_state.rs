//! Per-gateway provider session lifecycle state.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

/// Typed provider lifecycle state. Keep its string representation at API boundaries.
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
    /// Earliest time a degraded provider may receive a half-open retry.
    pub retry_after: Option<SystemTime>,
}

const DEGRADED_RETRY_COOLDOWN: Duration = Duration::from_secs(15);

/// Cloneable session registry. Clones share one gateway's state; independent
/// registries are isolated, including in tests and embedded gateway instances.
#[derive(Clone, Debug, Default)]
pub struct ProviderSessionRegistry {
    states: Arc<Mutex<HashMap<String, ProviderSessionStatus>>>,
}

impl ProviderSessionRegistry {
    pub fn status(&self, provider: &str) -> ProviderSessionStatus {
        self.states
            .lock()
            .ok()
            .and_then(|map| map.get(provider).cloned())
            .unwrap_or(ProviderSessionStatus {
                state: ProviderSessionState::Unknown,
                last_error: None,
                retry_after: None,
            })
    }

    /// Mark a provider as requiring authentication after its session is unlinked.
    pub fn record_unlinked(&self, provider: &str) {
        if let Ok(mut map) = self.states.lock() {
            map.insert(
                provider.to_owned(),
                ProviderSessionStatus {
                    state: ProviderSessionState::NeedsLogin,
                    last_error: Some("provider session unlinked; re-login required".to_owned()),
                    retry_after: None,
                },
            );
        }
    }

    pub fn record_needs_login(&self, provider: &str) {
        self.record_error_state(provider, true, None);
    }

    pub fn record_probe_failure(&self, provider: &str) {
        self.record_error_state(
            provider,
            false,
            SystemTime::now().checked_add(DEGRADED_RETRY_COOLDOWN),
        );
    }

    pub fn record_success(&self, provider: &str) {
        if let Ok(mut map) = self.states.lock() {
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

    pub fn record_error(&self, provider: &str, error: &str) {
        let expired = indicates_expired_session(error);
        let retry_after = if expired {
            None
        } else {
            SystemTime::now().checked_add(DEGRADED_RETRY_COOLDOWN)
        };
        self.record_error_state(provider, expired, retry_after);
    }

    fn record_error_state(&self, provider: &str, expired: bool, retry_after: Option<SystemTime>) {
        if let Ok(mut map) = self.states.lock() {
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

    /// Non-mutating readiness check; it does not claim the half-open retry slot.
    pub fn is_available(&self, provider: &str) -> bool {
        let now = SystemTime::now();
        let Ok(map) = self.states.lock() else {
            return false;
        };
        let Some(current) = map.get(provider) else {
            return true;
        };
        match current.state {
            ProviderSessionState::NeedsLogin => false,
            ProviderSessionState::Degraded => current
                .retry_after
                .is_some_and(|retry_after| now >= retry_after),
            ProviderSessionState::Unknown | ProviderSessionState::Ready => true,
        }
    }

    /// Admit only one half-open retry after a degraded cooldown.
    pub fn is_routable(&self, provider: &str) -> bool {
        let now = SystemTime::now();
        let Ok(mut map) = self.states.lock() else {
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
                    current.retry_after = now.checked_add(DEGRADED_RETRY_COOLDOWN);
                    true
                } else {
                    false
                }
            }
            ProviderSessionState::Unknown | ProviderSessionState::Ready => true,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_expiry_is_typed_redacted_and_cleared_on_success() {
        let registry = ProviderSessionRegistry::default();
        registry.record_error(
            "provider",
            "session expired: cookie=secret; user=private@example.test",
        );
        let expired = registry.status("provider");
        assert_eq!(expired.state, ProviderSessionState::NeedsLogin);
        assert_eq!(
            expired.last_error.as_deref(),
            Some("provider session expired; re-login required")
        );
        assert_eq!(expired.retry_after, None);
        registry.record_success("provider");
        assert_eq!(
            registry.status("provider").state,
            ProviderSessionState::Ready
        );
        assert_eq!(registry.status("provider").last_error, None);
    }

    #[test]
    fn transient_errors_enter_cooldown_and_expired_sessions_never_half_open() {
        let registry = ProviderSessionRegistry::default();
        registry.record_error("transient", "temporary network timeout");
        assert_eq!(
            registry.status("transient").state,
            ProviderSessionState::Degraded
        );
        assert!(!registry.is_routable("transient"));
        registry.record_needs_login("expired");
        assert!(!registry.is_routable("expired"));
        assert!(!registry.is_available("expired"));
    }

    #[test]
    fn readiness_does_not_claim_half_open_and_only_one_request_can_claim_it() {
        let registry = ProviderSessionRegistry::default();
        registry.record_error_state("provider", false, Some(SystemTime::UNIX_EPOCH));
        assert!(registry.is_available("provider"));
        assert!(registry.is_available("provider"));
        assert!(registry.is_routable("provider"));
        assert!(!registry.is_routable("provider"));
    }

    #[test]
    fn cloned_registry_shares_state_but_independent_instances_are_isolated() {
        let first = ProviderSessionRegistry::default();
        let clone = first.clone();
        let second = ProviderSessionRegistry::default();
        first.record_success("provider");
        assert_eq!(clone.status("provider").state, ProviderSessionState::Ready);
        assert_eq!(
            second.status("provider").state,
            ProviderSessionState::Unknown
        );
    }

    #[test]
    fn unlinking_session_requires_reauthentication() {
        let registry = ProviderSessionRegistry::default();
        registry.record_success("provider");
        registry.record_unlinked("provider");
        assert_eq!(
            registry.status("provider").state,
            ProviderSessionState::NeedsLogin
        );
        assert!(!registry.is_routable("provider"));
    }

    #[test]
    fn state_string_conversion_is_stable() {
        assert_eq!(ProviderSessionState::Unknown.as_str(), "unknown");
        assert_eq!(ProviderSessionState::Ready.as_str(), "ready");
        assert_eq!(ProviderSessionState::NeedsLogin.as_str(), "needs_login");
        assert_eq!(ProviderSessionState::Degraded.as_str(), "degraded");
    }
}
