//! Provider thread reopening and safe landing-page navigation guards.
//!
//! Compare canonical provider URLs and require a loaded composer before
//! reusing a persisted thread or accepting a fresh provider landing route.
use super::pool_policy;
use crate::providers;

/// A saved thread URL is not enough to prove that its composer has loaded.
/// In particular, an SPA can publish the new address before replacing the
/// previous page. Wait for both the exact saved thread identity and the
/// provider's input before allowing a delta-only follow-up.
pub(super) fn provider_thread_navigation_ready(
    actual: &str,
    target: &str,
    page: &obscura_core::Page,
    adapter: &dyn providers::SiteAdapter,
) -> bool {
    let composer_ready = adapter
        .selector_set()
        .input
        .iter()
        .any(|selector| page.query_selector(selector).is_some());
    thread_navigation_snapshot_ready(actual, target, composer_ready)
}

pub(super) fn thread_navigation_snapshot_ready(actual: &str, target: &str, composer_ready: bool) -> bool {
    composer_ready && pool_policy::provider_thread_navigation_matches(actual, target)
}

/// Landing pages can canonicalize the provider root to an app route. Accept
/// that redirect only on the same origin and only after the adapter composer
/// is present; thread URLs use the stricter path comparison above.
pub(super) fn provider_landing_navigation_ready(
    actual: &str,
    target: &str,
    page: &obscura_core::Page,
    adapter: &dyn providers::SiteAdapter,
) -> bool {
    fn origin(value: &str) -> Option<String> {
        let without_fragment = value.split('#').next()?;
        let without_query = without_fragment.split('?').next()?;
        let (scheme, rest) = without_query.split_once("://")?;
        let authority = rest.split('/').next()?;
        if scheme.is_empty() || authority.is_empty() {
            return None;
        }
        Some(format!(
            "{}://{}",
            scheme.to_ascii_lowercase(),
            authority.to_ascii_lowercase()
        ))
    }

    if origin(actual) != origin(target)
        || !pool_policy::landing_is_not_previous_thread(adapter.name(), actual)
    {
        // A provider's home page can redirect to an old remote thread while
        // still displaying a composer. Do not replay unrelated gateway
        // history into that page: require a genuinely fresh landing route.
        return false;
    }
    adapter
        .selector_set()
        .input
        .iter()
        .any(|selector| page.query_selector(selector).is_some())
}

#[cfg(test)]
mod provider_reopen_composer_tests {
    use super::thread_navigation_snapshot_ready;

    #[test]
    fn saved_thread_reopen_needs_matching_path_and_ready_composer() {
        assert!(!thread_navigation_snapshot_ready(
            "https://chatgpt.com/c/saved",
            "https://chatgpt.com/c/saved",
            false,
        ));
        assert!(!thread_navigation_snapshot_ready(
            "https://chatgpt.com/c/old",
            "https://chatgpt.com/c/saved",
            true,
        ));
        assert!(thread_navigation_snapshot_ready(
            "https://chatgpt.com/c/saved/?source=redirect",
            "https://chatgpt.com/c/saved",
            true,
        ));
    }
}
