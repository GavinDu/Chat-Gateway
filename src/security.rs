use axum::http::HeaderMap;

/// Accept only an Origin whose authority exactly matches an explicit loopback Host.
/// DNS names are rejected even when Origin and Host match, preventing DNS rebinding.
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
    authority.eq_ignore_ascii_case(host) && is_loopback_authority(host)
}

/// Validate a Host/Origin authority without accepting userinfo, paths, DNS names,
/// malformed ports, or non-loopback addresses.
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

#[cfg(test)]
mod tests {
    use super::{is_loopback_authority, login_origin_is_same_authority};
    use axum::http::{header, HeaderMap, HeaderValue};

    #[test]
    fn accepts_only_valid_loopback_authorities_and_ports() {
        for authority in [
            "localhost", "localhost:8080", "127.0.0.1", "127.0.0.1:8080", "[::1]", "[::1]:8080",
        ] {
            assert!(is_loopback_authority(authority), "expected loopback authority: {authority}");
        }
        for authority in [
            "localhost:", "localhost:0", "localhost:65536", "localhost:not-a-port",
            "localhost:8080/path", "user@localhost:8080", "localhost\\\\@attacker.example:8080",
            "[::1", "[::1]junk:8080", "[::1]:0", "[::1]:65536", "127.0.0.2:8080",
            "localhost.attacker.example:8080",
        ] {
            assert!(!is_loopback_authority(authority), "unsafe authority accepted: {authority}");
        }
    }

    #[test]
    fn origin_must_match_explicit_loopback_host_and_http_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8080"));
        headers.insert(header::ORIGIN, HeaderValue::from_static("http://127.0.0.1:8080"));
        assert!(login_origin_is_same_authority(&headers));

        headers.insert(header::ORIGIN, HeaderValue::from_static("https://attacker.example"));
        assert!(!login_origin_is_same_authority(&headers));
        headers.insert(header::ORIGIN, HeaderValue::from_static("http://attacker.example:8080"));
        headers.insert(header::HOST, HeaderValue::from_static("attacker.example:8080"));
        assert!(!login_origin_is_same_authority(&headers));
        headers.insert(header::ORIGIN, HeaderValue::from_static("file://127.0.0.1:8080"));
        assert!(!login_origin_is_same_authority(&headers));
    }

    #[test]
    fn missing_origin_is_allowed_for_local_automation_clients() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("localhost:8080"));
        assert!(login_origin_is_same_authority(&headers));
    }
}
