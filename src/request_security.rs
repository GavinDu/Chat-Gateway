use axum::http::HeaderMap;

/// Validate the Origin/Host pair used by local login and relay endpoints.
/// Requiring an explicit loopback authority prevents DNS-rebinding hostnames
/// from passing a simple same-origin comparison.
pub(crate) fn login_origin_is_same_authority(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        // CLI and local automation clients commonly omit Origin. The
        // Sec-Fetch-Site check in the middleware still rejects cross-site calls.
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };

    // Comparing Origin only to the request Host is insufficient: DNS rebinding
    // can make an attacker's hostname resolve to loopback, causing both values
    // to match. Accept only explicit loopback authorities.
    authority.eq_ignore_ascii_case(host) && is_loopback_authority(host)
}

pub(crate) fn is_loopback_authority(authority: &str) -> bool {
    if authority.contains('@') || authority.contains('/') || authority.contains('\\') {
        return false;
    }
    let (host, port) = if authority.starts_with('[') {
        let Some((host, rest)) = authority.split_once(']') else {
            return false;
        };
        let port = if rest.is_empty() {
            None
        } else if let Some(port) = rest.strip_prefix(':') {
            Some(port)
        } else {
            return false;
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => (host, Some(port)),
            Some(_) => return false,
            None => (authority, None),
        }
    };
    if let Some(port) = port {
        if port.parse::<u16>().ok().filter(|port| *port != 0).is_none() {
            return false;
        }
    }
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "[::1" | "[::1]"
    )
}


/// Parse the explicit public HTTPS origin for opt-in reverse-proxy deployments.
/// The proxy must strip incoming forwarded headers and set X-Forwarded-Proto itself.
pub(crate) fn remote_gateway_origin() -> anyhow::Result<Option<(String, String)>> {
    let Ok(value) = std::env::var("CHAT_GATEWAY_REMOTE_ORIGIN") else {
        return Ok(None);
    };
    let value = value.trim();
    let Some((scheme, remainder)) = value.split_once("://") else {
        anyhow::bail!("CHAT_GATEWAY_REMOTE_ORIGIN must be an HTTPS origin");
    };
    let authority = remainder.strip_suffix('/').unwrap_or(remainder);
    if scheme != "https" || authority.is_empty() || authority.contains('/') {
        anyhow::bail!("CHAT_GATEWAY_REMOTE_ORIGIN must be an HTTPS origin without a path");
    }
    if authority.is_empty()
        || authority.contains('@')
        || authority.contains('?')
        || authority.contains('#')
        || authority.contains('\\')
        || authority.chars().any(char::is_whitespace)
    {
        anyhow::bail!("CHAT_GATEWAY_REMOTE_ORIGIN has an invalid authority");
    }
    Ok(Some((format!("https://{authority}"), authority.to_ascii_lowercase())))
}

/// Remote mode is opt-in and requires an explicitly trusted TLS-terminating proxy,
/// an API credential hash, and a configured extension identity.
pub(crate) fn validate_remote_gateway_config() -> anyhow::Result<bool> {
    let Some((_origin, _authority)) = remote_gateway_origin()? else {
        return Ok(false);
    };
    if std::env::var("CHAT_GATEWAY_TRUSTED_PROXY").as_deref() != Ok("1") {
        anyhow::bail!("remote mode requires CHAT_GATEWAY_TRUSTED_PROXY=1");
    }
    let api_key = std::env::var("CHAT_GATEWAY_API_KEY")
        .map_err(|_| anyhow::anyhow!("remote mode requires CHAT_GATEWAY_API_KEY"))?;
    if api_key.len() < 32 || api_key.bytes().any(|byte| byte.is_ascii_whitespace()) {
        anyhow::bail!("CHAT_GATEWAY_API_KEY must contain at least 32 non-whitespace characters");
    }
    let extension_id = std::env::var("CHAT_GATEWAY_EXTENSION_ID")
        .map_err(|_| anyhow::anyhow!("remote mode requires CHAT_GATEWAY_EXTENSION_ID"))?;
    if extension_id.len() != 32 || !extension_id.bytes().all(|byte| (b'a'..=b'p').contains(&byte)) {
        anyhow::bail!("CHAT_GATEWAY_EXTENSION_ID must be a 32-character Chrome extension ID");
    }
    Ok(true)
}

pub(crate) fn remote_request_is_allowed(
    headers: &HeaderMap,
    remote_authority: &str,
    relay_origin_allowed: bool,
) -> bool {
    let host_matches = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| host.eq_ignore_ascii_case(remote_authority));
    let proxy_reports_https = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"));
    if !host_matches || !proxy_reports_https {
        return false;
    }
    if relay_origin_allowed {
        return true;
    }
    match headers.get(axum::http::header::ORIGIN) {
        None => true,
        Some(origin) => origin.to_str().is_ok_and(|origin| {
            remote_gateway_origin()
                .ok()
                .flatten()
                .is_some_and(|(expected, _)| origin == expected)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_matching_local_origin_and_host() {
        let mut headers = HeaderMap::new();
        headers.insert("origin", "http://127.0.0.1:8080".parse().unwrap());
        headers.insert("host", "127.0.0.1:8080".parse().unwrap());
        assert!(login_origin_is_same_authority(&headers));
    }

    #[test]
    fn rejects_origin_with_different_authority() {
        let mut headers = HeaderMap::new();
        headers.insert("origin", "https://evil.example".parse().unwrap());
        headers.insert("host", "127.0.0.1:8080".parse().unwrap());
        assert!(!login_origin_is_same_authority(&headers));
    }

    #[test]
    fn rejects_non_loopback_host_authority() {
        assert!(!is_loopback_authority("attacker.example:8080"));
        assert!(!is_loopback_authority("localhost.attacker.example:8080"));
    }

    #[test]
    fn rejects_dns_rebinding_hostname_even_when_origin_matches_host() {
        let mut headers = HeaderMap::new();
        headers.insert("origin", "http://attacker.example:8080".parse().unwrap());
        headers.insert("host", "attacker.example:8080".parse().unwrap());
        assert!(!login_origin_is_same_authority(&headers));
    }

    #[test]
    fn accepts_only_explicit_loopback_authorities() {
        assert!(is_loopback_authority("127.0.0.1:8080"));
        assert!(is_loopback_authority("localhost:8080"));
        assert!(is_loopback_authority("[::1]:8080"));
        assert!(!is_loopback_authority("localhost.attacker.example:8080"));
        assert!(!is_loopback_authority("127.0.0.2:8080"));
        assert!(!is_loopback_authority("localhost:0"));
    }

    #[test]
    fn allows_originless_local_automation_clients() {
        assert!(login_origin_is_same_authority(&HeaderMap::new()));
    }

    #[test]
    fn accepts_only_valid_loopback_authorities_and_ports() {
        for authority in [
            "localhost",
            "localhost:8080",
            "127.0.0.1",
            "127.0.0.1:8080",
            "[::1]",
            "[::1]:8080",
        ] {
            assert!(
                is_loopback_authority(authority),
                "expected loopback authority to be accepted: {authority}"
            );
        }

        for authority in [
            "localhost:",
            "localhost:0",
            "localhost:65536",
            "localhost:not-a-port",
            "localhost:8080/path",
            "user@localhost:8080",
            "localhost\\@attacker.example:8080",
            "[::1",
            "[::1]junk:8080",
            "[::1]:0",
            "[::1]:65536",
            "127.0.0.2:8080",
            "localhost.attacker.example:8080",
        ] {
            assert!(
                !is_loopback_authority(authority),
                "expected unsafe authority to be rejected: {authority}"
            );
        }
    }
}
