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

/// A page with a visible textarea is not proof of provider readiness when
/// redirects or unrelated tabs have moved it away from the provider origin.
/// Keep this origin check separate from authentication proof: a matching origin
/// alone still requires a visible composer and never proves a valid login.
fn expected_provider_chat_origin(provider: Provider, url: &str) -> bool {
    let Ok(uri) = url.parse::<axum::http::Uri>() else {
        return false;
    };
    if uri.scheme_str() != Some("https")
        || uri.authority().is_none_or(|authority| {
            authority.as_str().contains('@') || authority.port_u16().is_some_and(|port| port != 443)
        })
    {
        return false;
    }
    let Some(host) = uri.host() else {
        return false;
    };
    let Ok(expected) = provider.login_url().parse::<axum::http::Uri>() else {
        return false;
    };
    let Some(expected_host) = expected.host() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let expected_host = expected_host.to_ascii_lowercase();
    let root = expected_host.strip_prefix("www.").unwrap_or(&expected_host);
    // Only apex hostnames gain a www alias. Do not admit arbitrary subdomains
    // such as www.gemini.google.com or attacker.gemini.google.com.
    let canonical = host == expected_host
        || (expected_host.starts_with("www.") && host == root)
        || (root.matches('.').count() == 1 && host == format!("www.{root}"));
    if canonical {
        return true;
    }
    // Grok has two supported real chat origins. Generic x.com pages are not
    // Grok chat, even when they happen to contain a matching textarea.
    provider == Provider::Grok
        && matches!(host.as_str(), "x.com" | "www.x.com")
        && (uri.path() == "/i/grok" || uri.path().starts_with("/i/grok/"))
}

/// Verify that a warmed provider page still looks like an authenticated chat page.
/// Call only while holding that provider page's lock so probes never race a send.
pub(super) fn verify_session_page(
    provider: Provider,
    page: &obscura_core::Page,
    adapter: &dyn providers::SiteAdapter,
) -> Result<bool> {
    let url = page.url();
    let is_login_page = providers::common::is_login_url(&url);
    let input_found = expected_provider_chat_origin(provider, &url)
        && providers::common::visible_composer_exists(
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
    fn matching_provider_origin_is_necessary_for_chat_composer_readiness() {
        use crate::providers::Provider;
        for provider in Provider::all() {
            assert!(
                super::expected_provider_chat_origin(provider, provider.login_url()),
                "expected landing URL must be accepted for {}",
                provider.name()
            );
        }
        for url in [
            "https://gemini.google.com/app",
            "https://gemini.google.com:443/app",
            "https://chatgpt.com/c/a123",
            "https://www.chatgpt.com/c/a123",
            "https://meta.ai/",
            "https://www.meta.ai/",
        ] {
            let provider = if url.contains("gemini") {
                Provider::Gemini
            } else if url.contains("meta.ai") {
                Provider::MetaAI
            } else {
                Provider::ChatGPT
            };
            assert!(super::expected_provider_chat_origin(provider, url), "{url}");
        }
        for url in [
            "http://gemini.google.com/app",
            "https://gemini.google.com:8443/app",
            "https://gemini.google.com.attacker.invalid/app",
            "https://accounts.google.com/v3/signin",
            "https://www.gemini.google.com/app",
            "https://not-gemini.example/app",
            "data:text/html,<textarea></textarea>",
        ] {
            assert!(
                !super::expected_provider_chat_origin(Provider::Gemini, url),
                "{url}"
            );
        }
        // A foreign page with a visible composer is never a connected session.
        assert!(!classify_session_probe(
            false,
            super::expected_provider_chat_origin(Provider::Gemini, "https://example.test/chat")
        )
        .unwrap());
    }

    #[test]
    fn grok_secondary_chat_route_is_allowed_without_accepting_other_x_pages() {
        use crate::providers::Provider;
        for url in [
            "https://x.com/i/grok",
            "https://www.x.com/i/grok/thread/123",
        ] {
            assert!(
                super::expected_provider_chat_origin(Provider::Grok, url),
                "{url}"
            );
        }
        for url in [
            "https://x.com/home",
            "https://x.com/i/grokish",
            "https://x.com/i/grok-evil",
            "https://x.com.attacker.invalid/i/grok",
            "http://x.com/i/grok",
        ] {
            assert!(
                !super::expected_provider_chat_origin(Provider::Grok, url),
                "{url}"
            );
        }
    }

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
