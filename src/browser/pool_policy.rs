//! Pure URL and provider-session policy helpers used by the browser pool.
//! Kept separate from worker commands so security-sensitive decisions are easy to test.

use crate::login;

/// A persisted provider thread URL must remain on the provider's configured
/// origin. This prevents stale or corrupted thread metadata from navigating
/// the authenticated browser page to an unrelated site.
pub(super) fn provider_thread_url_is_same_origin(candidate: &str, provider_home: &str) -> bool {
    fn origin(value: &str) -> Option<String> {
        let without_fragment = value.split('#').next()?;
        let without_query = without_fragment.split('?').next()?;
        let (scheme, rest) = without_query.split_once("://")?;
        let authority = rest.split('/').next()?;
        if scheme.is_empty() || authority.is_empty() || authority.contains('@') {
            return None;
        }
        Some(format!(
            "{}://{}",
            scheme.to_ascii_lowercase(),
            authority.to_ascii_lowercase()
        ))
    }

    matches!(
        (origin(candidate), origin(provider_home)),
        (Some(candidate), Some(home)) if candidate == home
    )
}

/// Replaying into a provider's *previous* remote conversation would mix two
/// distinct gateway conversations. Some sites redirect their home page into
/// the last active thread; observing a composer on that page is insufficient
/// evidence that a fresh conversation was opened.
///
/// Only match the two URL-addressable adapters with verified thread path
/// conventions. Unknown layouts must be verified live before adding rules.
pub(super) fn landing_is_not_previous_thread(provider_name: &str, actual: &str) -> bool {
    let Ok(uri) = actual.parse::<axum::http::Uri>() else {
        return false;
    };
    if uri.scheme_str() != Some("https") || uri.authority().is_none() {
        return false;
    }
    match provider_name {
        "chatgpt" => !uri.path().starts_with("/c/"),
        "claude" => !uri.path().starts_with("/chat/"),
        _ => true,
    }
}

/// Compare provider thread URLs without requiring byte-for-byte URL equality.
/// Providers may add a trailing slash, tracking query, or fragment during a
/// redirect; origin and path still identify the same conversation.
pub(super) fn provider_thread_navigation_matches(actual: &str, target: &str) -> bool {
    fn origin_and_path(value: &str) -> Option<(String, String)> {
        let without_fragment = value.split('#').next()?;
        let without_query = without_fragment.split('?').next()?;
        let (scheme, rest) = without_query.split_once("://")?;
        let (authority, path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, format!("/{path}")),
            None => (rest, "/".to_owned()),
        };
        if authority.is_empty() || scheme.is_empty() {
            return None;
        }
        let normalized_path = if path.len() > 1 {
            path.trim_end_matches('/').to_owned()
        } else {
            path
        };
        Some((
            format!(
                "{}://{}",
                scheme.to_ascii_lowercase(),
                authority.to_ascii_lowercase()
            ),
            normalized_path,
        ))
    }

    matches!(
        (origin_and_path(actual), origin_and_path(target)),
        (Some((actual_origin, actual_path)), Some((target_origin, target_path)))
            if actual_origin == target_origin && actual_path == target_path
    )
}

#[cfg(test)]
mod provider_thread_navigation_tests {
    use super::{provider_thread_navigation_matches, provider_thread_url_is_same_origin};

    #[test]
    fn fresh_landing_rejects_chatgpt_and_claude_existing_thread_routes() {
        use super::landing_is_not_previous_thread;

        assert!(!landing_is_not_previous_thread(
            "chatgpt", "https://chatgpt.com/c/thread-123?model=gpt",
        ));
        assert!(!landing_is_not_previous_thread(
            "claude", "https://claude.ai/chat/thread-456#message",
        ));
        assert!(landing_is_not_previous_thread("chatgpt", "https://chatgpt.com/"));
        assert!(landing_is_not_previous_thread("claude", "https://claude.ai/new"));
        assert!(!landing_is_not_previous_thread("chatgpt", "broken URL"));
    }

    #[test]
    fn thread_navigation_accepts_trailing_slash_query_and_fragment_redirects() {
        assert!(provider_thread_navigation_matches(
            "https://chatgpt.com/c/thread-123/?utm_source=redirect#latest",
            "https://chatgpt.com/c/thread-123"
        ));
    }

    #[test]
    fn thread_navigation_rejects_wrong_origin_or_thread_path() {
        assert!(!provider_thread_navigation_matches(
            "https://evil.example/c/thread-123",
            "https://chatgpt.com/c/thread-123"
        ));
        assert!(!provider_thread_navigation_matches(
            "https://chatgpt.com/c/thread-456",
            "https://chatgpt.com/c/thread-123"
        ));
    }

    #[test]
    fn saved_thread_url_must_match_the_configured_provider_origin() {
        assert!(provider_thread_url_is_same_origin(
            "https://chatgpt.com/c/thread-1?utm_source=redirect",
            "https://chatgpt.com/"
        ));
        assert!(!provider_thread_url_is_same_origin(
            "https://chatgpt.com.attacker.example/c/thread-1",
            "https://chatgpt.com/"
        ));
        assert!(!provider_thread_url_is_same_origin(
            "https://chatgpt.com@attacker.example/c/thread-1",
            "https://chatgpt.com/"
        ));
        assert!(!provider_thread_url_is_same_origin(
            "https://claude.ai/thread-1",
            "https://chatgpt.com/"
        ));
    }

    #[test]
    fn thread_navigation_rejects_malformed_urls() {
        assert!(!provider_thread_navigation_matches(
            "not-a-url",
            "https://chatgpt.com/c/thread-123"
        ));
    }
}

pub(super) fn session_probe_requires_login(is_login_page: bool, input_found: bool) -> bool {
    login::login_state(is_login_page, input_found).0 == "login_required"
}

pub(super) fn session_probe_page_healthy(is_login_page: bool, input_found: bool) -> bool {
    let (_, logged_in) = login::login_state(is_login_page, input_found);
    logged_in == Some(true)
}

#[cfg(test)]
mod session_probe_tests {
    use super::{session_probe_page_healthy, session_probe_requires_login};

    #[test]
    fn probe_distinguishes_expired_login_page_from_unwarmed_page() {
        assert!(session_probe_requires_login(true, true));
        assert!(!session_probe_page_healthy(false, false));
        assert!(session_probe_page_healthy(false, true));
    }

    #[test]
    fn probe_requires_chat_input_and_not_a_login_page() {
        assert!(session_probe_page_healthy(false, true));
        assert!(!session_probe_page_healthy(true, true));
        assert!(!session_probe_page_healthy(false, false));
    }
}
