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

fn chatgpt_endpoint_kind(value: &str) -> &'static str {
    // Reuse the already linked HTTP URI parser; no new client dependency.
    let value = value.split('#').next().unwrap_or_default();
    let Ok(url) = value.parse::<axum::http::Uri>() else {
        return "other";
    };
    let Some(authority) = url.authority() else {
        return "other";
    };
    let Some(host) = url.host() else {
        return "other";
    };
    let host = host.to_ascii_lowercase();
    let authority = authority.as_str().to_ascii_lowercase();
    if url.scheme_str() != Some("https")
        || !matches!(host.as_str(), "chatgpt.com" | "www.chatgpt.com")
        || (authority != host && authority != format!("{host}:443"))
    {
        return "other";
    }
    let path = url.path();
    match path {
        "/api/auth/session" => "auth_session",
        "/backend-api/me" => "account",
        "/backend-api/conversation" | "/backend-api/f/conversation" => "conversation_submit",
        "/backend-api/conversation/prepare" | "/backend-api/f/conversation/prepare" => {
            "conversation_prepare"
        }
        "/backend-api/conversation/resume" | "/backend-api/f/conversation/resume" => {
            "conversation_resume"
        }
        "/.well-known/chat-gateway-websocket" => "websocket_bridge",
        _ => {
            if path.ends_with(".js") || path.ends_with(".mjs") {
                "static_asset"
            } else {
                "first_party_other"
            }
        }
    }
}

fn method_kind(method: &str) -> &'static str {
    match method {
        "GET" => "get",
        "POST" => "post",
        _ => "other",
    }
}

fn content_type_kind(value: Option<&str>) -> &'static str {
    let Some(value) = value else {
        return "absent";
    };
    let mime = value.split(';').next().unwrap_or_default().trim();
    if mime.eq_ignore_ascii_case("text/event-stream") {
        "event_stream"
    } else if mime.eq_ignore_ascii_case("application/json") {
        "json"
    } else if mime.eq_ignore_ascii_case("text/html") {
        "html"
    } else if mime.eq_ignore_ascii_case("text/javascript")
        || mime.eq_ignore_ascii_case("application/javascript")
    {
        "javascript"
    } else {
        "other"
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

    // Dispatch events are bounded per page. Endpoint labels are a closed
    // vocabulary: paths/queries/IDs and header values never leave the callback.
    // These events cannot pair concurrent requests or prove remote delivery.
    if verbose_enabled && provider == Provider::ChatGPT {
        let dispatched = Arc::new(AtomicUsize::new(0));
        page.on_request(Arc::new(move |info| {
            if !matches!(
                info.resource_type,
                obscura_core::ResourceType::Fetch | obscura_core::ResourceType::Xhr
            ) {
                return;
            }
            let Ok(previous) =
                dispatched.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    (count < 256).then_some(count.saturating_add(1))
                })
            else {
                return;
            };
            tracing::debug!(
                provider = "chatgpt",
                scope = "page_request",
                observation_sequence = previous + 1,
                observation_limit = 256,
                endpoint_kind = chatgpt_endpoint_kind(info.url.as_str()),
                method_kind = method_kind(&info.method),
                "ChatGPT request dispatched (fixed metadata only)"
            );
        }));
    }
    let provider_name = provider.name();
    let script_errors_seen = Arc::new(AtomicUsize::new(0));
    let metaai_documents_seen = Arc::new(AtomicUsize::new(0));
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
            scope = "page_response",
            request_endpoint_kind = if provider == Provider::ChatGPT { chatgpt_endpoint_kind(info.url.as_str()) } else { "not_applicable" },
            response_endpoint_kind = if provider == Provider::ChatGPT { chatgpt_endpoint_kind(response.url.as_str()) } else { "not_applicable" },
            method_kind = method_kind(&info.method),
            content_type_kind = content_type_kind(response.content_type()),
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

#[cfg(test)]
mod chatgpt_network_metadata_tests {
    use super::*;
    #[test]
    fn diagnostics_classify_protocol_roles_without_exporting_secrets() {
        for (path, expected) in [
            ("/api/auth/session", "auth_session"),
            ("/backend-api/me", "account"),
            ("/backend-api/conversation", "conversation_submit"),
            ("/backend-api/f/conversation", "conversation_submit"),
            ("/backend-api/conversation/prepare", "conversation_prepare"),
            (
                "/backend-api/f/conversation/prepare",
                "conversation_prepare",
            ),
            ("/backend-api/conversation/resume", "conversation_resume"),
            ("/backend-api/f/conversation/resume", "conversation_resume"),
            ("/.well-known/chat-gateway-websocket", "websocket_bridge"),
            ("/assets/fixture.js", "static_asset"),
            ("/assets/fixture.mjs", "static_asset"),
            ("/c/synthetic-private-thread", "first_party_other"),
            (
                "/backend-api/conversation/synthetic-private-thread",
                "first_party_other",
            ),
            ("/backend-api/me/extra", "first_party_other"),
        ] {
            for origin in ["https://chatgpt.com", "https://www.chatgpt.com:443"] {
                let url = format!("{origin}{path}?token=synthetic-private#private");
                assert_eq!(chatgpt_endpoint_kind(&url), expected);
                assert!(!chatgpt_endpoint_kind(&url).contains("private"));
            }
        }
    }
    #[test]
    fn foreign_insecure_credentialed_and_lookalike_urls_are_other() {
        for value in [
            "http://chatgpt.com/backend-api/me",
            "https://chatgpt.com:8443/backend-api/me",
            "https://chatgpt.com.evil.invalid/backend-api/me",
            "https://other.invalid/backend-api/me",
            "https://chatgpt.com@evil.invalid/backend-api/me",
            "https://synthetic:private@chatgpt.com/backend-api/me",
            "https://ws.chatgpt.com/backend-api/me",
            "https://meta.ai/backend-api/me",
            "data:text/plain,synthetic-private",
            "https://127.0.0.1/backend-api/me",
        ] {
            assert_eq!(chatgpt_endpoint_kind(value), "other");
        }
    }
    #[test]
    fn arbitrary_header_values_and_methods_never_become_diagnostic_labels() {
        for (value, expected) in [
            (None, "absent"),
            (Some(" Text/Event-Stream; charset=utf-8"), "event_stream"),
            (Some("application/json; private=synthetic"), "json"),
            (Some("text/html"), "html"),
            (Some("application/javascript"), "javascript"),
            (Some("text/javascript"), "javascript"),
            (Some("synthetic-private"), "other"),
            (Some("application/json-synthetic"), "other"),
            (Some(""), "other"),
        ] {
            assert_eq!(content_type_kind(value), expected);
        }
        assert_eq!(method_kind("GET"), "get");
        assert_eq!(method_kind("POST"), "post");
        assert_eq!(method_kind("SYNTHETIC-PRIVATE"), "other");
    }
}
