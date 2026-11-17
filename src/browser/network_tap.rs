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
const MAX_MISTRAL_SCRIPT_FAILURE_LOGS_PER_PAGE: usize = 1;
const MAX_META_AI_DOCUMENT_LOGS_PER_PAGE: usize = 8;

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

/// Only a first-party Mistral script 403 can classify the composer as
/// provider-asset blocked. Optional third-party analytics failures are not
/// enough to establish that the editor's own JavaScript is unavailable.
fn is_first_party_mistral_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "mistral.ai" || host.ends_with(".mistral.ai")
}

/// Only first-party Meta AI *document* response statuses are logged.
/// An HTTPS response from an unrelated redirect host must never be classified
/// as Meta AI content; host checks deliberately reject lookalike suffixes.
fn is_metaai_document_response(
    provider: Provider,
    resource_type: obscura_core::ResourceType,
    host: &str,
) -> bool {
    provider == Provider::MetaAI
        && resource_type == obscura_core::ResourceType::Document
        && matches!(
            host.to_ascii_lowercase().as_str(),
            "meta.ai" | "www.meta.ai"
        )
}

/// First-party Meta AI document HTTP 403 is a bootstrap-access denial,
/// not proof of a login expiry. Do not classify CSS/scripts or lookalike hosts.
fn is_metaai_document_denial(
    provider: Provider,
    resource_type: obscura_core::ResourceType,
    host: &str,
    status: u16,
) -> bool {
    status == 403 && is_metaai_document_response(provider, resource_type, host)
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
    if !verbose_enabled && !matches!(provider, Provider::Mistral | Provider::MetaAI) {
        return None;
    }

    let provider_name = provider.name();
    let script_errors_seen = Arc::new(AtomicUsize::new(0));
    let metaai_documents_seen = Arc::new(AtomicUsize::new(0));
    let metaai_graphql_seen = Arc::new(AtomicUsize::new(0));
    let bootstrap_403_seen = Arc::new(AtomicBool::new(false));
    let observed_403 = Arc::clone(&bootstrap_403_seen);
    let metaai_seen = Arc::clone(&metaai_documents_seen);
    page.on_response(Arc::new(move |info, response| {
        if is_metaai_document_response(
            provider,
            info.resource_type,
            response.url.host_str().unwrap_or(""),
        ) {
            // Preserve the directly observed first-party document status even
            // after bounded log cap. Never retain URL paths, query or body.
            if is_metaai_document_denial(
                provider,
                info.resource_type,
                response.url.host_str().unwrap_or(""),
                response.status,
            ) {
                observed_403.store(true, Ordering::Relaxed);
            }
            if metaai_seen.fetch_add(1, Ordering::Relaxed) < MAX_META_AI_DOCUMENT_LOGS_PER_PAGE {
                let status = response.status;
                if status >= 400 {
                    tracing::warn!(
                        provider = "metaai",
                        resource_type = "document",
                        status,
                        response_bytes_observed = provider_network_tap_reported_bytes(response.body.len()),
                        redirect_hops = response.redirected_from.len().min(8),
                        "Meta AI document HTTP response observed (metadata only)"
                    );
                } else {
                    tracing::info!(
                        provider = "metaai",
                        resource_type = "document",
                        status,
                        response_bytes_observed = provider_network_tap_reported_bytes(response.body.len()),
                        redirect_hops = response.redirected_from.len().min(8),
                        "Meta AI document HTTP response observed (metadata only)"
                    );
                }
            }
        }
        // Meta's application reports GraphQL 403 from its JavaScript stack;
        // this is not evidence that a script or document download was denied.
        // Observe the exact first-party endpoint without reading/logging body,
        // request variables, tokens, paths, viewer identity or cookie values.
        if provider == Provider::MetaAI
            && metaai_graphql_response(info.url.as_str(), response.url.as_str())
            && metaai_graphql_seen.fetch_add(1, Ordering::Relaxed) < 8
        {
            tracing::info!(provider = "metaai", stage = "application_graphql",
                status = response.status, "Meta AI application response observed (status only)");
        }
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
            // Record even after the one-entry log cap. This per-page flag
            // contains no hostname, URL, cookies, or response body.
            if response.status == 403
                && response
                    .url
                    .host_str()
                    .is_some_and(is_first_party_mistral_host)
            {
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
    matches!(provider, Provider::Mistral | Provider::MetaAI).then_some(bootstrap_403_seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_ai_only_logs_first_party_document_response_metadata() {
        use obscura_core::ResourceType;
        for host in ["meta.ai", "www.meta.ai", "WWW.META.AI"] {
            assert!(is_metaai_document_response(
                Provider::MetaAI,
                ResourceType::Document,
                host
            ));
        }
        for host in [
            "meta.ai.attacker.invalid",
            "fake-meta.ai",
            "login.meta.ai",
            "",
        ] {
            assert!(!is_metaai_document_response(
                Provider::MetaAI,
                ResourceType::Document,
                host
            ));
        }
        for kind in [
            ResourceType::Script,
            ResourceType::Fetch,
            ResourceType::Other,
        ] {
            assert!(!is_metaai_document_response(
                Provider::MetaAI,
                kind,
                "www.meta.ai"
            ));
        }
        assert!(!is_metaai_document_response(
            Provider::ChatGPT,
            ResourceType::Document,
            "www.meta.ai"
        ));
    }

    #[test]
    fn metaai_document_denial_requires_observed_first_party_http_403() {
        use obscura_core::ResourceType;
        for host in ["meta.ai", "www.meta.ai", "WWW.META.AI"] {
            assert!(is_metaai_document_denial(
                Provider::MetaAI,
                ResourceType::Document,
                host,
                403,
            ));
        }
        for host in ["meta.ai.attacker.invalid", "login.meta.ai", "fake-meta.ai"] {
            assert!(!is_metaai_document_denial(
                Provider::MetaAI,
                ResourceType::Document,
                host,
                403,
            ));
        }
        for status in [200, 302, 401, 404, 500] {
            assert!(!is_metaai_document_denial(
                Provider::MetaAI,
                ResourceType::Document,
                "www.meta.ai",
                status,
            ));
        }
        assert!(!is_metaai_document_denial(
            Provider::MetaAI,
            ResourceType::Script,
            "www.meta.ai",
            403,
        ));
        assert!(!is_metaai_document_denial(
            Provider::ChatGPT,
            ResourceType::Document,
            "www.meta.ai",
            403,
        ));
    }

    #[test]
    fn only_first_party_mistral_script_failure_can_mark_blocker() {
        for host in ["chat.mistral.ai", "assets.mistral.ai", "mistral.ai"] {
            assert!(is_first_party_mistral_host(host));
        }
        for host in [
            "evil-mistral.ai",
            "mistral.ai.example.com",
            "static.cloudflareinsights.com",
        ] {
            assert!(!is_first_party_mistral_host(host));
        }
    }

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

/// Both the dispatched request and its final response must be the exact
/// first-party HTTPS endpoint. Redirected unrelated responses are not proof.
fn metaai_graphql_response(request: &str, response: &str) -> bool {
    [request, response].into_iter().all(|value| {
        let Ok(uri) = value.parse::<axum::http::Uri>() else {
            return false;
        };
        uri.scheme_str() == Some("https")
            && uri
                .authority()
                .is_some_and(|a| !a.as_str().contains('@') && a.port_u16().is_none_or(|p| p == 443))
            && matches!(uri.host(), Some("meta.ai" | "www.meta.ai"))
            && uri.path() == "/api/graphql"
    })
}

#[cfg(test)]
mod meta_graphql_tests {
    use super::metaai_graphql_response;
    #[test]
    fn exact_https_endpoint_only_and_redirects_do_not_inherit_classification() {
        let endpoint = "https://www.meta.ai/api/graphql";
        assert!(metaai_graphql_response(endpoint, endpoint));
        assert!(metaai_graphql_response(
            endpoint,
            "https://meta.ai/api/graphql?private=not-logged"
        ));
        for bad in [
            "http://www.meta.ai/api/graphql",
            "https://www.meta.ai:8443/api/graphql",
            "https://www.meta.ai.attacker.test/api/graphql",
            "https://www.meta.ai/_next/script.js",
            "https://www.meta.ai/api/graphql-other",
            "https://user@www.meta.ai/api/graphql",
        ] {
            assert!(!metaai_graphql_response(endpoint, bad));
            assert!(!metaai_graphql_response(bad, endpoint));
        }
    }
}
