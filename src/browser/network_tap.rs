use crate::providers::Provider;
use std::sync::Arc;

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

fn provider_network_tap_reported_bytes(body_len: usize) -> usize {
    body_len.min(MAX_PROVIDER_NETWORK_TAP_REPORTED_BYTES)
}

fn provider_network_tap_enabled(provider: Provider) -> bool {
    provider_network_tap_provider_supported(provider)
        && std::env::var("CHAT_GATEWAY_PROVIDER_NETWORK_TAP")
            .map(|value| provider_network_tap_flag_enabled(&value))
            .unwrap_or(false)
}

pub(super) fn attach_provider_network_tap(page: &mut obscura_core::Page, provider: Provider) {
    if !provider_network_tap_enabled(provider) {
        return;
    }

    let provider_name = provider.name();
    page.on_response(Arc::new(move |info, response| {
        if !matches!(
            info.resource_type,
            obscura_core::ResourceType::Fetch | obscura_core::ResourceType::Xhr
        ) {
            return;
        }
        // Host-only correlation avoids leaking paths, query strings, cookies,
        // authorization headers, prompts, or response body content.
        let host = response.url.host_str().unwrap_or("unknown");
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
