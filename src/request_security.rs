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
