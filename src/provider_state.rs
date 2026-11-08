//! Per-gateway provider session lifecycle state.
use crate::chat_backend::{GatewayError, GatewayErrorKind};
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

    /// An operator reset allows the next request to probe the provider again;
    /// it does not claim that a browser session has been authenticated.
    pub fn reset_for_retry(&self, provider: &str) -> anyhow::Result<()> {
        self.states
            .lock()
            .map_err(|_| anyhow::anyhow!("provider state unavailable"))?
            .remove(provider);
        Ok(())
    }

    /// New credentials permit a login retry, but do not erase a failed chat
    /// merely because the extension rotated unrelated provider cookies.
    pub fn record_session_import(&self, provider: &str) {
        if let Ok(mut map) = self.states.lock() {
            if map
                .get(provider)
                .is_some_and(|status| status.state == ProviderSessionState::Degraded)
            {
                return;
            }
            map.remove(provider);
        }
    }

    /// A visible composer cannot clear a failed chat attempt or explicit unlink.
    pub fn record_probe_success(&self, provider: &str) {
        if let Ok(mut map) = self.states.lock() {
            let status = map
                .entry(provider.to_owned())
                .or_insert(ProviderSessionStatus {
                    state: ProviderSessionState::Ready,
                    last_error: None,
                    retry_after: None,
                });
            if status.state == ProviderSessionState::Unknown {
                status.state = ProviderSessionState::Ready;
            }
        }
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

    /// Classify canonical backend errors by type, never by upstream text.
    /// Busy/caller/memory errors cannot make an otherwise healthy linked
    /// provider appear logged out. Only a typed authentication failure can
    /// force re-login; other provider failures enter a bounded cooldown.
    pub fn record_gateway_error(&self, provider: &str, error: &GatewayError) {
        match error.kind {
            GatewayErrorKind::SessionExpired | GatewayErrorKind::Challenge => {
                self.record_needs_login(provider);
            }
            GatewayErrorKind::InvalidRequest
            | GatewayErrorKind::ModelNotFound
            | GatewayErrorKind::ProviderBusy
            | GatewayErrorKind::Memory => {}
            GatewayErrorKind::Timeout
            | GatewayErrorKind::ProviderFailure
            | GatewayErrorKind::BackendUnavailable => self.record_probe_failure(provider),
        }
    }

    /// Legacy untyped diagnostics must never force a provider into NeedsLogin.
    /// Provider-controlled text is untrusted; only a typed backend error may
    /// assert that authentication expired or a challenge was detected.
    pub fn record_error(&self, provider: &str, _error: &str) {
        self.record_probe_failure(provider);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_expiry_is_typed_redacted_and_cleared_on_success() {
        let registry = ProviderSessionRegistry::default();
        registry.record_gateway_error(
            "provider",
            &GatewayError::session_expired("cookie=secret; user=private@example.test"),
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
    fn untyped_provider_message_cannot_force_reauthentication() {
        let registry = ProviderSessionRegistry::default();
        registry.record_error(
            "provider",
            "session expired, please sign in; cookie=secret; user=private@example.test",
        );
        let status = registry.status("provider");
        assert_eq!(status.state, ProviderSessionState::Degraded);
        assert!(status.retry_after.is_some());
        assert_eq!(
            status.last_error.as_deref(),
            Some("provider request failed; retry may succeed")
        );
    }

    #[test]
    fn typed_errors_never_infer_auth_expiry_from_untrusted_messages() {
        let registry = ProviderSessionRegistry::default();
        registry.record_gateway_error(
            "false-expiry",
            &GatewayError::provider_failure("upstream said session expired: cookie=secret-value"),
        );
        let degraded = registry.status("false-expiry");
        assert_eq!(degraded.state, ProviderSessionState::Degraded);
        assert!(degraded.retry_after.is_some());
        assert!(!degraded.last_error.unwrap().contains("secret-value"));

        registry.record_gateway_error(
            "auth-expired",
            &GatewayError::session_expired("arbitrary sensitive upstream text"),
        );
        let expired = registry.status("auth-expired");
        assert_eq!(expired.state, ProviderSessionState::NeedsLogin);
        assert_eq!(expired.retry_after, None);

        registry.record_gateway_error(
            "challenge",
            &GatewayError::challenge("challenge from provider"),
        );
        assert_eq!(
            registry.status("challenge").state,
            ProviderSessionState::NeedsLogin
        );
    }

    #[test]
    fn busy_caller_and_memory_errors_do_not_degrade_a_healthy_session() {
        let registry = ProviderSessionRegistry::default();
        let provider = "healthy";
        registry.record_success(provider);
        for error in [
            GatewayError::provider_busy("busy"),
            GatewayError::typed(GatewayErrorKind::InvalidRequest, "bad request", false),
            GatewayError::typed(GatewayErrorKind::ModelNotFound, "unknown model", false),
            GatewayError::typed(GatewayErrorKind::Memory, "store unavailable", false),
        ] {
            registry.record_gateway_error(provider, &error);
            assert_eq!(registry.status(provider).state, ProviderSessionState::Ready);
        }
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
    fn admin_retry_reset_does_not_falsely_claim_authenticated_session() {
        let registry = ProviderSessionRegistry::default();
        registry.record_needs_login("provider");
        assert!(!registry.is_routable("provider"));
        registry.reset_for_retry("provider").unwrap();
        assert_eq!(
            registry.status("provider").state,
            ProviderSessionState::Unknown
        );
        assert!(registry.is_routable("provider"));
        assert!(registry.status("provider").last_error.is_none());
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

#[cfg(test)]
mod probe_outcome_tests {
    use super::*;
    #[test]
    fn composer_probe_cannot_erase_chat_failure_or_explicit_unlink() {
        let registry = ProviderSessionRegistry::default();
        registry.record_gateway_error(
            "chatgpt",
            &GatewayError::typed(GatewayErrorKind::ProviderFailure, "synthetic", true),
        );
        let failed = registry.status("chatgpt");
        registry.record_probe_success("chatgpt");
        registry.record_session_import("chatgpt");
        assert_eq!(registry.status("chatgpt"), failed);
        registry.record_unlinked("grok");
        registry.record_probe_success("grok");
        assert_eq!(
            registry.status("grok").state,
            ProviderSessionState::NeedsLogin
        );
        registry.record_success("chatgpt");
        assert_eq!(
            registry.status("chatgpt").state,
            ProviderSessionState::Ready
        );
    }
}
