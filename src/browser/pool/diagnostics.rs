//! Typed provider session probes shared by worker startup and status checks.
//! Missing page controls are inconclusive, not proof of expired credentials.
use super::pool_policy;
use crate::providers::{self, Provider};
use anyhow::Result;

/// Typed outcome for a probe that confirms the provider has redirected to login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderSessionExpiredError;

impl std::fmt::Display for ProviderSessionExpiredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("provider session requires login")
    }
}

impl std::error::Error for ProviderSessionExpiredError {}

/// Verify that a warmed provider page still looks like an authenticated chat page.
/// Call only while holding that provider page's lock so probes never race a send.
pub(super) fn verify_session_page(
    _provider: Provider,
    page: &obscura_core::Page,
    adapter: &dyn providers::SiteAdapter,
) -> Result<bool> {
    let is_login_page = providers::common::is_login_page(page);
    let input_found = providers::common::visible_composer_exists(
        page,
        &adapter.effective_selectors().input_refs(),
    );
    classify_session_probe(is_login_page, input_found)
}

fn classify_session_probe(is_login_page: bool, input_found: bool) -> Result<bool> {
    if pool_policy::session_probe_requires_login(is_login_page, input_found) {
        return Err(anyhow::Error::new(ProviderSessionExpiredError));
    }
    // Missing chat controls can mean an unloaded page, a provider redesign,
    // or a challenge screen. It is not proof that cookies have expired.
    // Preserve the saved session and report the probe as inconclusive.
    Ok(pool_policy::session_probe_page_healthy(
        is_login_page,
        input_found,
    ))
}

/// A login page may still be displaying an authentication redirect, a challenge,
/// or an unloaded shell. Never persist and promote that page to a warmed session
/// merely because the user pressed Complete.
pub(super) fn require_login_completion_ready(probe: Result<bool>) -> Result<()> {
    if !probe? {
        anyhow::bail!(
            "provider login is not ready: chat composer unavailable; finish authentication or retry after page load"
        );
    }
    Ok(())
}

#[cfg(test)]
mod inconclusive_session_probe_tests {
    use super::{
        classify_session_probe, require_login_completion_ready, ProviderSessionExpiredError,
    };

    #[test]
    fn missing_chat_input_is_inconclusive_not_an_expired_session() {
        assert!(!classify_session_probe(false, false).unwrap());
        assert!(classify_session_probe(false, true).unwrap());
        let expired = classify_session_probe(true, false).unwrap_err();
        assert!(expired
            .downcast_ref::<ProviderSessionExpiredError>()
            .is_some());
    }
    #[test]
    fn login_complete_requires_proven_composer_readiness() {
        assert!(require_login_completion_ready(Ok(true)).is_ok());
        let unknown = require_login_completion_ready(Ok(false)).unwrap_err();
        assert!(unknown.to_string().contains("chat composer unavailable"));
        let expired =
            require_login_completion_ready(Err(anyhow::Error::new(ProviderSessionExpiredError)))
                .unwrap_err();
        assert!(expired
            .downcast_ref::<ProviderSessionExpiredError>()
            .is_some());
    }
}
