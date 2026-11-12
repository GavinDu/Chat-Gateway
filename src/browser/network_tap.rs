use crate::providers::Provider;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

/// Metadata-only response observation. Obscura currently exposes buffered
/// response bodies, so this deliberately does not claim incremental SSE data.
fn provider_network_tap_provider_supported(provider: Provider) -> bool {
    matches!(provider, Provider::ChatGPT | Provider::Claude)
}

fn provider_network_tap_flag_enabled(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

const MAX_PROVIDER_NETWORK_TAP_REPORTED_BYTES: usize = 1_048_576;
const MAX_MISTRAL_SCRIPT_FAILURE_LOGS_PER_PAGE: usize = 8;

/// Script modulepreloads can arrive as Other/Fetch rather than Script.
/// Examine pathname suffixes privately, never include paths or query data in
/// logs or labels. Non-script API/CSS requests must not become JS evidence.
fn is_javascript_asset_path(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, extension)| {
        extension.eq_ignore_ascii_case("js")
            || extension.eq_ignore_ascii_case("mjs")
            || extension.eq_ignore_ascii_case("cjs")
    })
}

/// Use observed Obscura responses (not unavailable cross-origin Resource
/// Timing statuses) to distinguish actual JavaScript HTTP failures.
fn is_mistral_script_failure(
    provider: Provider,
    resource_type: obscura_core::ResourceType,
    status: u16,
    javascript_asset_path: bool,
) -> bool {
    provider == Provider::Mistral
        && (400..600).contains(&status)
        && (resource_type == obscura_core::ResourceType::Script
            || (javascript_asset_path
                && matches!(
                    resource_type,
                    obscura_core::ResourceType::Other | obscura_core::ResourceType::Fetch
                )))
}

fn safe_hostname(host: &str) -> &str {
    if !host.is_empty()
        && host.len() <= 128
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        host
    } else {
        "unknown"
    }
}

fn provider_network_tap_reported_bytes(body_len: usize) -> usize {
    body_len.min(MAX_PROVIDER_NETWORK_TAP_REPORTED_BYTES)
}

fn provider_network_tap_enabled(provider: Provider) -> bool {
    provider_network_tap_provider_supported(provider)
        && std::env::var("CHAT_GATEWAY_PROVIDER_NETWORK_TAP")
            .map(|value| provider_network_tap_flag_enabled(&value))
            .unwrap_or(false)
}

pub(super) fn attach_provider_network_tap(
    page: &mut obscura_core::Page,
    provider: Provider,
) -> Option<Arc<AtomicBool>> {
    let verbose_enabled = provider_network_tap_enabled(provider);
    // Script HTTP failures on the Mistral provider are security-safe default
    // diagnostics. The existing Fetch/XHR tap remains opt-in for other sites.
    if !verbose_enabled && provider != Provider::Mistral {
        return None;
    }

    let provider_name = provider.name();
    let script_errors_seen = Arc::new(AtomicUsize::new(0));
    let mistral_403_seen = Arc::new(AtomicBool::new(false));
    let observed_403 = Arc::clone(&mistral_403_seen);
    page.on_response(Arc::new(move |info, response| {
        let javascript_asset_path = is_javascript_asset_path(info.url.path())
            || is_javascript_asset_path(response.url.path())
            || response
                .redirected_from
                .iter()
                .any(|earlier| is_javascript_asset_path(earlier.path()));
        if is_mistral_script_failure(
            provider,
            info.resource_type,
            response.status,
            javascript_asset_path,
        ) {
            // Record even after the eight-entry log cap. This per-page flag
            // contains no hostname, URL, cookies, or response body.
            if response.status == 403 {
                observed_403.store(true, Ordering::Relaxed);
            }
            if script_errors_seen.fetch_add(1, Ordering::Relaxed)
                < MAX_MISTRAL_SCRIPT_FAILURE_LOGS_PER_PAGE
            {
                let host = safe_hostname(response.url.host_str().unwrap_or("unknown"));
                tracing::warn!(
                    provider = provider_name,
                    host,
                    status = response.status,
                    redirect_hops = response.redirected_from.len().min(8),
                    "Mistral script HTTP failure observed (host/status only)"
                );
            }
        }
        if !verbose_enabled || !matches!(
            info.resource_type,
            obscura_core::ResourceType::Fetch | obscura_core::ResourceType::Xhr
        ) {
            return;
        }
        // Host-only correlation avoids leaking paths, query strings, cookies,
        // authorization headers, prompts, or response body content.
        let host = safe_hostname(response.url.host_str().unwrap_or("unknown"));
        tracing::debug!(
            provider = provider_name,
            host,
            resource_type = ?info.resource_type,
            status = response.status,
            response_bytes_observed = provider_network_tap_reported_bytes(response.body.len()),
            response_bytes_truncated = response.body.len() > MAX_PROVIDER_NETWORK_TAP_REPORTED_BYTES,
            "provider network response observed (metadata only)"
        );
    }));
    (provider == Provider::Mistral).then_some(mistral_403_seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mistral_script_failure_filter_is_exact_and_status_bounded() {
        use obscura_core::ResourceType;
        assert!(is_mistral_script_failure(
            Provider::Mistral,
            ResourceType::Script,
            403,
            false,
        ));
        assert!(is_mistral_script_failure(
            Provider::Mistral,
            ResourceType::Script,
            502,
            false,
        ));
        assert!(!is_mistral_script_failure(
            Provider::Mistral,
            ResourceType::Script,
            200,
            true,
        ));
        assert!(is_mistral_script_failure(
            Provider::Mistral,
            ResourceType::Other,
            403,
            true,
        ));
        assert!(is_mistral_script_failure(
            Provider::Mistral,
            ResourceType::Fetch,
            403,
            true,
        ));
        assert!(!is_mistral_script_failure(
            Provider::Mistral,
            ResourceType::Fetch,
            403,
            false,
        ));
        assert!(!is_mistral_script_failure(
            Provider::Mistral,
            ResourceType::Xhr,
            403,
            true,
        ));
        assert!(!is_mistral_script_failure(
            Provider::ChatGPT,
            ResourceType::Script,
            403,
            true,
        ));
        for path in [
            "/assets/app.js",
            "/assets/app.mjs",
            "/assets/app.cjs",
            "/assets/APP.JS",
        ] {
            assert!(is_javascript_asset_path(path), "{path}");
        }
        for path in [
            "/api/chat",
            "/assets/app.js.map",
            "/assets/styles.css",
            "/assets/app.js.txt",
        ] {
            assert!(!is_javascript_asset_path(path), "{path}");
        }
        assert_eq!(safe_hostname("cdn.mistral.ai"), "cdn.mistral.ai");
        assert_eq!(
            safe_hostname("cdn.mistral.ai/secret?token=private"),
            "unknown"
        );
        assert_eq!(safe_hostname("a".repeat(129).as_str()), "unknown");
    }

    #[test]
    fn metadata_tap_flag_requires_an_explicit_truthy_value() {
        for value in ["1", "true", "TRUE", " yes "] {
            assert!(provider_network_tap_flag_enabled(value), "{value:?}");
        }
        for value in ["", "0", "false", "no", "enabled", "true please"] {
            assert!(!provider_network_tap_flag_enabled(value), "{value:?}");
        }
    }

    #[test]
    fn metadata_tap_byte_count_is_bounded() {
        assert_eq!(provider_network_tap_reported_bytes(0), 0);
        assert_eq!(provider_network_tap_reported_bytes(4096), 4096);
        assert_eq!(
            provider_network_tap_reported_bytes(usize::MAX),
            MAX_PROVIDER_NETWORK_TAP_REPORTED_BYTES
        );
    }

    #[test]
    fn metadata_tap_is_scoped_to_supported_providers() {
        assert!(provider_network_tap_provider_supported(Provider::ChatGPT));
        assert!(provider_network_tap_provider_supported(Provider::Claude));
        assert!(!provider_network_tap_provider_supported(Provider::Gemini));
    }
}
