//! Immutable, compile-time release provenance. No runtime Git lookup: an updated
//! checkout must never relabel an older installed binary.
use chrono::SecondsFormat;

fn safe_source_sha(value: Option<&str>) -> &str {
    value
        .filter(|sha| {
            sha.len() == 40
                && sha
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        .unwrap_or("unknown")
}

fn safe_build_timestamp(value: Option<&str>) -> &str {
    value
        .filter(|timestamp| {
            timestamp.len() == 20
                && timestamp.ends_with('Z')
                && chrono::DateTime::parse_from_rfc3339(timestamp).is_ok_and(|parsed| {
                    parsed.to_rfc3339_opts(SecondsFormat::Secs, true) == *timestamp
                })
        })
        .unwrap_or("unknown")
}

pub(crate) fn build_identity() -> serde_json::Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "source_sha": safe_source_sha(option_env!("CHAT_GATEWAY_SOURCE_SHA")),
        "build_timestamp_utc": safe_build_timestamp(option_env!("CHAT_GATEWAY_BUILD_TIMESTAMP")),
    })
}

#[cfg(test)]
mod tests {
    use super::{build_identity, safe_build_timestamp, safe_source_sha};

    #[test]
    fn valid_pinned_build_sha_is_not_derived_from_runtime_checkout() {
        let baked = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(safe_source_sha(Some(baked)), baked);
        // No read from the filesystem, runtime environment, or Git repository
        // is involved in identity(): only option_env! at compile time.
        assert!(build_identity()["version"].as_str().is_some());
    }

    #[test]
    fn unknown_or_untrusted_sha_is_explicitly_unknown() {
        for input in [
            None,
            Some(""),
            Some("unknown"),
            Some("abcdef"),
            Some("0123456789abcdef0123456789abcdef0123456G"),
            Some("0123456789ABCDEF0123456789ABCDEF01234567"),
            Some("0123456789abcdef0123456789abcdef01234567\n"),
        ] {
            assert_eq!(safe_source_sha(input), "unknown");
        }
    }

    #[test]
    fn timestamp_is_strict_normalized_utc_seconds() {
        assert_eq!(
            safe_build_timestamp(Some("2026-11-17T05:12:24Z")),
            "2026-11-17T05:12:24Z"
        );
        for input in [
            None,
            Some("unknown"),
            Some("2026-11-17T00:12:24-05:00"),
            Some("2026-11-17T05:12:24.000Z"),
            Some("2026-02-30T05:12:24Z"),
            Some("2026-11-17T05:12:24Z\r\nsecret"),
        ] {
            assert_eq!(safe_build_timestamp(input), "unknown");
        }
    }

    #[test]
    fn health_build_identity_uses_only_bounded_public_fields() {
        let identity = build_identity();
        assert_eq!(identity.as_object().unwrap().len(), 3);
        assert!(identity["version"].is_string());
        assert!(identity["source_sha"].is_string());
        assert!(identity["build_timestamp_utc"].is_string());
    }
}
