//! Pure URL and provider-session policy helpers used by the browser pool.
//! Kept separate from worker commands so security-sensitive decisions are easy to test.

use crate::login;

/// A persisted provider thread URL must remain on the provider's configured
/// origin. This prevents stale or corrupted thread metadata from navigating
/// the authenticated browser page to an unrelated site.
/// Only absolute HTTPS URLs with unambiguous authorities may identify
/// authenticated provider pages. Browsers can interpret backslashes or
/// percent-escaped hostnames differently from a textual origin comparison.
fn verified_https_provider_uri(value: &str) -> Option<axum::http::Uri> {
    let without_fragment = value.split('#').next()?;
    if without_fragment.contains('\\') {
        return None;
    }
    let uri = without_fragment.parse::<axum::http::Uri>().ok()?;
    let authority = uri.authority()?;
    if uri.scheme_str() != Some("https")
        || authority
            .as_str()
            .bytes()
            .any(|byte| matches!(byte, b'@' | b'%' | b'\\'))
    {
        return None;
    }
    Some(uri)
}

/// A persisted provider thread URL must remain on the provider's configured
/// HTTPS origin, without ambiguous userinfo or encoded authority spellings.
pub(super) fn provider_thread_url_is_same_origin(candidate: &str, provider_home: &str) -> bool {
    let (Some(candidate), Some(home)) = (
        verified_https_provider_uri(candidate),
        verified_https_provider_uri(provider_home),
    ) else {
        return false;
    };
    candidate
        .authority()
        .expect("verified URI has an authority")
        .as_str()
        .eq_ignore_ascii_case(
            home.authority()
                .expect("verified URI has an authority")
                .as_str(),
        )
}

/// A provider landing URL with a conversation selector is not a fresh
/// composer, even when its path is the homepage. This check is deliberately
/// provider-agnostic: an unknown query-based redirect must not mix the
/// gateway's replay with an existing remote conversation.
fn landing_query_selects_existing_thread(uri: &axum::http::Uri) -> bool {
    uri.query().is_some_and(|query| {
        query.split('&').any(|parameter| {
            let name = parameter.split('=').next().unwrap_or_default();
            let name = name.to_ascii_lowercase().replace("%5f", "_");
            // Reject ambiguous encoded keys instead of guessing whether
            // they select a remote conversation.
            name.contains('%')
                || matches!(
                    name.as_str(),
                    "conversation"
                        | "conversation_id"
                        | "conversationid"
                        | "thread"
                        | "thread_id"
                        | "threadid"
                        | "chat_id"
                        | "chatid"
                )
        })
    })
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
    if landing_query_selects_existing_thread(&uri) {
        return false;
    }
    match provider_name {
        // ChatGPT custom-GPT threads may be nested under /g/<gpt>/c/<id>.
        // A home-page redirect to one of these is still a previous remote
        // conversation and must not receive a fresh gateway conversation.
        "chatgpt" => {
            let path = uri.path();
            !path.starts_with("/c/") && !(path.starts_with("/g/") && path.contains("/c/"))
        }
        "claude" => !uri.path().starts_with("/chat/"),
        // Providers without a verified URL-addressable reopen implementation
        // must replay into a demonstrably fresh composer. In particular,
        // their home routes can redirect into previous conversations.
        "gemini" => !uri.path().starts_with("/app/"),
        "grok" => {
            !uri.path().starts_with("/c/")
                && !uri.path().starts_with("/chat/")
                && !uri.query().is_some_and(|query| {
                    query
                        .split('&')
                        .any(|parameter| parameter.starts_with("conversation="))
                })
        }
        "qwen" => !uri.path().starts_with("/c/"),
        "mistral" => !uri.path().starts_with("/chat/"),
        "perplexity" => !uri.path().starts_with("/search/") && !uri.path().starts_with("/page/"),
        // There is no fixture-verified Meta AI conversation URL convention;
        // accept only the configured homepage instead of trusting an unknown
        // redirect into a potentially stale remote thread.
        "metaai" => uri.path() == "/",
        _ => false,
    }
}

/// Compare provider thread URLs without requiring byte-for-byte URL equality.
/// Providers may add a trailing slash, tracking query, or fragment during a
/// redirect; origin and path still identify the same conversation.
pub(super) fn provider_thread_navigation_matches(actual: &str, target: &str) -> bool {
    let (Some(actual), Some(target)) = (
        verified_https_provider_uri(actual),
        verified_https_provider_uri(target),
    ) else {
        return false;
    };
    // A redirect with a query-selected conversation can point at an entirely
    // different remote thread even when its origin/path match the saved URL.
    // Tracking queries remain allowed, but ambiguous thread selectors fail closed.
    if landing_query_selects_existing_thread(&actual)
        || landing_query_selects_existing_thread(&target)
    {
        return false;
    }

    let actual_path = actual.path().trim_end_matches('/');
    let target_path = target.path().trim_end_matches('/');
    let actual_path = if actual_path.is_empty() {
        "/"
    } else {
        actual_path
    };
    let target_path = if target_path.is_empty() {
        "/"
    } else {
        target_path
    };
    actual
        .authority()
        .expect("verified URI has an authority")
        .as_str()
        .eq_ignore_ascii_case(
            target
                .authority()
                .expect("verified URI has an authority")
                .as_str(),
        )
        && actual_path == target_path
}

/// Fresh sends must reload the provider's landing page, even if its URL is
/// already the configured home: an SPA can retain previous conversation state
/// behind that same URL. Only a verified, addressable saved thread may reuse
/// the currently loaded document without a navigation.
pub(super) fn provider_send_requires_navigation(
    current_url: &str,
    target_url: &str,
    reopening_saved_thread: bool,
) -> bool {
    !reopening_saved_thread || current_url != target_url
}

/// Before reusing a document after a same-URL fresh-chat navigation, require
/// a *different* document load timestamp. The address and composer may look
/// unchanged while the previous SPA document is still active.
pub(super) fn fresh_document_navigation_confirmed(
    initial_time_origin: f64,
    observed_time_origin: Option<f64>,
) -> bool {
    initial_time_origin.is_finite()
        && observed_time_origin.is_some_and(|time| time.is_finite() && time != initial_time_origin)
}

#[cfg(test)]
mod provider_thread_navigation_tests {
    use super::{provider_thread_navigation_matches, provider_thread_url_is_same_origin};

    #[test]
    fn fresh_landing_rejects_chatgpt_and_claude_existing_thread_routes() {
        use super::landing_is_not_previous_thread;

        assert!(!landing_is_not_previous_thread(
            "chatgpt",
            "https://chatgpt.com/c/thread-123?model=gpt",
        ));
        assert!(!landing_is_not_previous_thread(
            "claude",
            "https://claude.ai/chat/thread-456#message",
        ));
        assert!(!landing_is_not_previous_thread(
            "chatgpt",
            "https://chatgpt.com/g/g-my-gpt/c/previous-thread?model=gpt#latest",
        ));
        assert!(!landing_is_not_previous_thread(
            "chatgpt",
            "https://chatgpt.com/g/g-my-gpt/c/previous-thread/",
        ));
        assert!(landing_is_not_previous_thread(
            "chatgpt",
            "https://chatgpt.com/g/g-my-gpt",
        ));
        assert!(landing_is_not_previous_thread(
            "chatgpt",
            "https://chatgpt.com/"
        ));
        assert!(landing_is_not_previous_thread(
            "claude",
            "https://claude.ai/new"
        ));
        assert!(!landing_is_not_previous_thread("chatgpt", "broken URL"));
    }

    #[test]
    fn replay_only_providers_reject_known_existing_thread_routes() {
        use super::landing_is_not_previous_thread;

        for (provider, previous_thread) in [
            ("gemini", "https://gemini.google.com/app/abc123"),
            ("grok", "https://grok.com/c/abc123"),
            ("grok", "https://grok.com/chat/abc123"),
            ("grok", "https://grok.com/?conversation=abc123&src=recent"),
            ("qwen", "https://chat.qwen.ai/c/abc123"),
            ("mistral", "https://chat.mistral.ai/chat/abc123"),
            ("perplexity", "https://www.perplexity.ai/search/abc123"),
            ("perplexity", "https://www.perplexity.ai/page/abc123"),
            ("metaai", "https://www.meta.ai/chat/abc123"),
        ] {
            assert!(
                !landing_is_not_previous_thread(provider, previous_thread),
                "{provider} must not replay into an existing thread: {previous_thread}"
            );
        }
    }

    #[test]
    fn query_based_thread_redirects_are_never_fresh_conversations() {
        use super::landing_is_not_previous_thread;

        for (provider, url) in [
            ("grok", "https://grok.com/?conversation_id=old-thread"),
            ("grok", "https://grok.com/?Conversation=old-thread"),
            (
                "gemini",
                "https://gemini.google.com/app?thread_id=old-thread",
            ),
            ("chatgpt", "https://chatgpt.com/?chatId=old-thread"),
            (
                "claude",
                "https://claude.ai/new?conversation%5Fid=old-thread",
            ),
            ("qwen", "https://chat.qwen.ai/?thread=old-thread"),
            ("mistral", "https://chat.mistral.ai/?chat_id=old-thread"),
            (
                "perplexity",
                "https://www.perplexity.ai/?thread%69d=old-thread",
            ),
        ] {
            assert!(
                !landing_is_not_previous_thread(provider, url),
                "{provider} must not replay into a query-selected thread: {url}"
            );
        }
        for (provider, url) in [
            ("grok", "https://grok.com/?source=home"),
            ("chatgpt", "https://chatgpt.com/?model=auto"),
            ("claude", "https://claude.ai/new?utm_source=app"),
        ] {
            assert!(landing_is_not_previous_thread(provider, url));
        }
    }

    #[test]
    fn replay_only_providers_accept_fresh_landing_routes() {
        use super::landing_is_not_previous_thread;

        for (provider, landing) in [
            ("gemini", "https://gemini.google.com/app"),
            ("grok", "https://grok.com/"),
            ("qwen", "https://chat.qwen.ai/"),
            ("mistral", "https://chat.mistral.ai/"),
            ("perplexity", "https://www.perplexity.ai/"),
            ("metaai", "https://www.meta.ai/"),
        ] {
            assert!(
                landing_is_not_previous_thread(provider, landing),
                "{provider} should allow its fresh landing route: {landing}"
            );
        }
        assert!(!landing_is_not_previous_thread(
            "unknown",
            "https://unknown.example/"
        ));
    }

    #[test]
    fn fresh_send_always_navigates_even_on_provider_home_url() {
        use super::provider_send_requires_navigation;

        assert!(provider_send_requires_navigation(
            "https://chatgpt.com/",
            "https://chatgpt.com/",
            false,
        ));
        assert!(provider_send_requires_navigation(
            "https://claude.ai/new",
            "https://claude.ai/new",
            false,
        ));
        assert!(provider_send_requires_navigation(
            "https://chatgpt.com/c/old",
            "https://chatgpt.com/",
            false,
        ));
    }

    #[test]
    fn addressable_saved_thread_only_skips_navigation_on_exact_loaded_url() {
        use super::provider_send_requires_navigation;

        assert!(!provider_send_requires_navigation(
            "https://chatgpt.com/c/saved",
            "https://chatgpt.com/c/saved",
            true,
        ));
        assert!(provider_send_requires_navigation(
            "https://chatgpt.com/c/other",
            "https://chatgpt.com/c/saved",
            true,
        ));
    }

    #[test]
    fn same_url_fresh_navigation_requires_a_distinct_document_time_origin() {
        use super::fresh_document_navigation_confirmed;

        assert!(!fresh_document_navigation_confirmed(1000.5, Some(1000.5)));
        assert!(!fresh_document_navigation_confirmed(1000.5, None));
        assert!(!fresh_document_navigation_confirmed(1000.5, Some(f64::NAN)));
        assert!(!fresh_document_navigation_confirmed(f64::NAN, Some(2000.5)));
        assert!(fresh_document_navigation_confirmed(1000.5, Some(2000.5)));
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

    #[test]
    fn thread_navigation_rejects_query_selected_thread_redirects() {
        for actual in [
            "https://chatgpt.com/c/thread-123?conversation_id=different",
            "https://chatgpt.com/c/thread-123?thread%69d=different",
            "https://chatgpt.com/c/thread-123?chatId=different",
        ] {
            assert!(!provider_thread_navigation_matches(
                actual,
                "https://chatgpt.com/c/thread-123"
            ));
        }
        assert!(!provider_thread_navigation_matches(
            "https://chatgpt.com/c/thread-123",
            "https://chatgpt.com/c/thread-123?conversation=ambiguous"
        ));
    }

    #[test]
    fn thread_urls_reject_ambiguous_authorities_and_non_https_schemes() {
        for bad in [
            "http://chatgpt.com/c/thread-123",
            "https://chatgpt.com@evil.example/c/thread-123",
            "https://chatgpt%2ecom/c/thread-123",
            "https://chatgpt.com\\@evil.example/c/thread-123",
        ] {
            assert!(!provider_thread_url_is_same_origin(
                bad,
                "https://chatgpt.com/"
            ));
            assert!(!provider_thread_navigation_matches(
                bad,
                "https://chatgpt.com/c/thread-123"
            ));
        }
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
