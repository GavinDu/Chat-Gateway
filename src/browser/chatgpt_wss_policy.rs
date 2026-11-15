//! Fail-closed policy primitives for a possible first-party ChatGPT WSS bridge.
//!
//! This module does not open sockets or attach authentication. A future bridge
//! MUST connect to the vetted DNS answer (not resolve the hostname a second
//! time), recheck page/generation ownership, and never print the raw URL.

use axum::http::Uri;
use std::{net::IpAddr, net::Ipv4Addr};

const MAX_TARGET_BYTES: usize = 8192;

/// The error is intentionally free of URL, query, credential or DNS contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidChatgptWssTarget;

impl std::fmt::Display for InvalidChatgptWssTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatGPT WebSocket target rejected by first-party policy")
    }
}

impl std::error::Error for InvalidChatgptWssTarget {}

/// A parsed target. Deliberately no derived Debug: raw queries may be secret.
pub struct ValidatedChatgptWssTarget {
    uri: Uri,
    host: String,
}

impl std::fmt::Debug for ValidatedChatgptWssTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatedChatgptWssTarget")
            .field("host", &self.host)
            .field("query_present", &self.uri.query().is_some())
            .finish()
    }
}

impl ValidatedChatgptWssTarget {
    pub fn parse(value: &str) -> Result<Self, InvalidChatgptWssTarget> {
        if value.len() > MAX_TARGET_BYTES || value.contains('#') {
            return Err(InvalidChatgptWssTarget);
        }
        let uri: Uri = value.parse().map_err(|_| InvalidChatgptWssTarget)?;
        let authority = uri.authority().ok_or(InvalidChatgptWssTarget)?;
        if uri.scheme_str() != Some("wss")
            || authority.as_str().contains('@')
            || authority.port_u16().is_some_and(|port| port != 443)
        {
            return Err(InvalidChatgptWssTarget);
        }
        let host = uri
            .host()
            .ok_or(InvalidChatgptWssTarget)?
            .to_ascii_lowercase();
        if !valid_chatgpt_host(&host) {
            return Err(InvalidChatgptWssTarget);
        }
        Ok(Self { uri, host })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// Caller must not log the Uri; it may contain session-scoped query data.
    pub fn uri(&self) -> &Uri {
        &self.uri
    }
}

fn valid_chatgpt_host(host: &str) -> bool {
    if host.len() > 253 || !(host == "chatgpt.com" || host.ends_with(".chatgpt.com")) {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

/// Conservative global-unicast test for DNS answers before opening a socket.
/// Revalidation MUST be coupled with an IP-pinned network connection; DNS
/// checks alone do not prevent a hostname being rebound at connect time.
pub fn is_public_socket_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => public_v4(ipv4),
        IpAddr::V6(ipv6) => {
            if let Some(v4) = ipv6.to_ipv4_mapped() {
                return public_v4(v4);
            }
            let segments = ipv6.segments();
            (segments[0] & 0xe000) == 0x2000
                && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
                && !(segments[0] == 0x2001 && segments[1] == 0)
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (18..=19).contains(&b))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

/// One private answer alongside public answers must reject the whole result.
/// An empty DNS response is not authorization to connect.
pub fn resolution_is_public(answers: &[IpAddr]) -> bool {
    !answers.is_empty() && answers.iter().copied().all(is_public_socket_address)
}

#[cfg(test)]
mod tests {
    use super::{
        is_public_socket_address, resolution_is_public, ValidatedChatgptWssTarget as Target,
    };
    use std::net::IpAddr;

    fn ip(address: &str) -> IpAddr {
        address.parse().unwrap()
    }

    #[test]
    fn allows_only_chatgpt_wss_first_party_targets() {
        for target in [
            "wss://chatgpt.com/socket",
            "wss://chatgpt.com:443/socket?nonce=private",
            "wss://events.chatgpt.com/ws?token=should-not-be-logged",
            "wss://a.b.chatgpt.com/",
        ] {
            let allowed = Target::parse(target).unwrap();
            assert!(allowed.host().ends_with("chatgpt.com"));
            assert_eq!(allowed.uri().scheme_str(), Some("wss"));
            assert!(!format!("{allowed:?}").contains("private"));
            assert!(!format!("{allowed:?}").contains("should-not-be-logged"));
        }
    }

    #[test]
    fn rejects_scheme_credentials_spoofed_suffix_port_fragment_and_invalid_labels() {
        for target in [
            "ws://chatgpt.com/socket",
            "https://chatgpt.com/socket",
            "wss://user:secret@chatgpt.com/socket",
            "wss://chatgpt.com:444/socket",
            "wss://chatgpt.com.evil.invalid/",
            "wss://evilchatgpt.com/",
            "wss://a..chatgpt.com/",
            "wss://-a.chatgpt.com/",
            "wss://a-.chatgpt.com/",
            "wss://chatgpt.com./socket",
            "wss://127.0.0.1/ws",
            "wss://[::1]/ws",
            "wss://chatgpt.com/path#fragment",
            "wss://chatgpt.com:65536/",
            "wss://example.invalid/",
            "wss:///missing-host",
        ] {
            let error = Target::parse(target).unwrap_err();
            assert_eq!(
                error.to_string(),
                "ChatGPT WebSocket target rejected by first-party policy"
            );
            assert!(!error.to_string().contains(target));
        }
        assert!(Target::parse(&format!(
            "wss://chatgpt.com/?{}",
            "x".repeat(8192)
        ))
        .is_err());
    }

    #[test]
    fn public_dns_filter_rejects_local_reserved_and_mixed_answers() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.5.1",
            "192.168.1.1",
            "192.0.2.8",
            "198.18.0.1",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public_socket_address(ip(address)), "{address}");
        }
        for address in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public_socket_address(ip(address)), "{address}");
        }
        assert!(!resolution_is_public(&[]));
        assert!(resolution_is_public(&[ip("8.8.8.8"), ip("1.1.1.1")]));
        assert!(!resolution_is_public(&[
            ip("8.8.8.8"),
            ip("192.168.1.8"),
        ]));
    }
}
