//! In-process provider session state derived from actual send outcomes.
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderSessionStatus {
    pub state: &'static str,
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
            state: "unknown",
            last_error: None,
        })
}

pub fn record_success(provider: &str) {
    if let Ok(mut map) = states().lock() {
        map.insert(
            provider.to_owned(),
            ProviderSessionStatus {
                state: "ready",
                last_error: None,
            },
        );
    }
}

pub fn record_error(provider: &str, error: &str) {
    if !error.to_ascii_lowercase().contains("session_expired") {
        return;
    }
    if let Ok(mut map) = states().lock() {
        map.insert(
            provider.to_owned(),
            ProviderSessionStatus {
                state: "needs_login",
                last_error: Some("provider session expired; re-login required".to_owned()),
            },
        );
    }
}

pub fn is_routable(provider: &str) -> bool {
    status(provider).state != "needs_login"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_provider_is_routable_until_checked() {
        assert!(is_routable("test-provider-unknown"));
        assert_eq!(status("test-provider-unknown").state, "unknown");
    }

    #[test]
    fn only_expired_sessions_enter_needs_login() {
        let provider = "test-provider-session-state";
        record_error(provider, "temporary network timeout");
        assert_eq!(status(provider).state, "unknown");
        record_error(provider, "session_expired: login required");
        assert_eq!(status(provider).state, "needs_login");
        assert!(!is_routable(provider));
        record_success(provider);
        assert_eq!(status(provider).state, "ready");
        assert!(is_routable(provider));
    }
}
