//! Offline provider fixture contracts. Fixtures are sanitized, deterministic
//! contract examples; they contain no captured account/session data.

const FIXTURES: &[(&str, &str, &str)] = &[
    (
        "qwen",
        "logged-out",
        include_str!("fixtures/qwen/logged-out.html"),
    ),
    ("qwen", "idle", include_str!("fixtures/qwen/idle.html")),
    (
        "qwen",
        "streaming",
        include_str!("fixtures/qwen/streaming.html"),
    ),
    ("qwen", "done", include_str!("fixtures/qwen/done.html")),
    (
        "metaai",
        "logged-out",
        include_str!("fixtures/metaai/logged-out.html"),
    ),
    (
        "metaai",
        "idle",
        include_str!("fixtures/metaai/idle.html"),
    ),
    (
        "metaai",
        "streaming",
        include_str!("fixtures/metaai/streaming.html"),
    ),
    (
        "metaai",
        "done",
        include_str!("fixtures/metaai/done.html"),
    ),
    (
        "claude",
        "logged-out",
        include_str!("fixtures/claude/logged-out.html"),
    ),
    (
        "claude",
        "idle",
        include_str!("fixtures/claude/idle.html"),
    ),
    (
        "claude",
        "streaming",
        include_str!("fixtures/claude/streaming.html"),
    ),
    (
        "claude",
        "done",
        include_str!("fixtures/claude/done.html"),
    ),
    (
        "chatgpt",
        "logged-out",
        include_str!("fixtures/chatgpt/logged-out.html"),
    ),
    (
        "chatgpt",
        "idle",
        include_str!("fixtures/chatgpt/idle.html"),
    ),
    (
        "chatgpt",
        "streaming",
        include_str!("fixtures/chatgpt/streaming.html"),
    ),
    (
        "chatgpt",
        "done",
        include_str!("fixtures/chatgpt/done.html"),
    ),
    (
        "gemini",
        "logged-out",
        include_str!("fixtures/gemini/logged-out.html"),
    ),
    (
        "gemini",
        "idle",
        include_str!("fixtures/gemini/idle.html"),
    ),
    (
        "gemini",
        "streaming",
        include_str!("fixtures/gemini/streaming.html"),
    ),
    (
        "gemini",
        "done",
        include_str!("fixtures/gemini/done.html"),
    ),
    (
        "grok",
        "logged-out",
        include_str!("fixtures/grok/logged-out.html"),
    ),
    ("grok", "idle", include_str!("fixtures/grok/idle.html")),
    (
        "grok",
        "streaming",
        include_str!("fixtures/grok/streaming.html"),
    ),
    ("grok", "done", include_str!("fixtures/grok/done.html")),
    (
        "mistral",
        "logged-out",
        include_str!("fixtures/mistral/logged-out.html"),
    ),
    (
        "mistral",
        "idle",
        include_str!("fixtures/mistral/idle.html"),
    ),
    (
        "mistral",
        "streaming",
        include_str!("fixtures/mistral/streaming.html"),
    ),
    (
        "mistral",
        "done",
        include_str!("fixtures/mistral/done.html"),
    ),
    (
        "perplexity",
        "logged-out",
        include_str!("fixtures/perplexity/logged-out.html"),
    ),
    (
        "perplexity",
        "idle",
        include_str!("fixtures/perplexity/idle.html"),
    ),
    (
        "perplexity",
        "streaming",
        include_str!("fixtures/perplexity/streaming.html"),
    ),
    (
        "perplexity",
        "done",
        include_str!("fixtures/perplexity/done.html"),
    ),
];

#[test]
fn every_provider_has_all_required_states() {
    for provider in [
        "qwen",
        "metaai",
        "claude",
        "chatgpt",
        "gemini",
        "grok",
        "mistral",
        "perplexity",
    ] {
        for state in ["logged-out", "idle", "streaming", "done"] {
            let (_, _, html) = FIXTURES
                .iter()
                .find(|(p, s, _)| *p == provider && *s == state)
                .unwrap_or_else(|| panic!("missing {provider}/{state} fixture"));
            assert!(html.contains(&format!(r#"data-fixture-provider="{provider}""#)));
            assert!(html.contains(&format!(r#"data-fixture-state="{state}""#)));
        }
    }
}

#[test]
fn fixtures_exercise_expected_ui_contract_states() {
    for (provider, state, html) in FIXTURES {
        match *state {
            "logged-out" => assert!(html.contains("data-contract-login"), "{provider}"),
            "idle" => {
                assert!(html.contains("data-contract-input"), "{provider}");
                assert!(html.contains("data-contract-send"), "{provider}");
                assert!(!html.contains("data-contract-response"), "{provider}");
            }
            "streaming" => {
                assert!(html.contains("data-contract-input"), "{provider}");
                assert!(html.contains("data-contract-send"), "{provider}");
                assert!(html.contains("data-contract-response"), "{provider}");
                assert!(html.contains("data-is-streaming=\"true\""), "{provider}");
            }
            "done" => {
                assert!(html.contains("data-contract-input"), "{provider}");
                assert!(html.contains("data-contract-send"), "{provider}");
                assert!(html.contains("data-contract-response"), "{provider}");
                assert!(!html.contains("data-is-streaming=\"true\""), "{provider}");
                assert!(html.contains("Complete fixture answer"), "{provider}");
            }
            other => panic!("unexpected fixture state: {other}"),
        }
    }
}

#[test]
fn fixtures_do_not_contain_credentials_or_personal_data() {
    let email = regex::Regex::new(r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b").unwrap();
    for (provider, state, html) in FIXTURES {
        let lower = html.to_ascii_lowercase();
        for forbidden in [
            "set-cookie",
            "authorization:",
            "bearer ",
            "access_token",
            "refresh_token",
            "sessionid",
            "api_key",
            "csrf_token",
        ] {
            assert!(
                !lower.contains(forbidden),
                "{provider}/{state} contains {forbidden}"
            );
        }
        assert!(
            !email.is_match(html),
            "{provider}/{state} contains an email address"
        );
    }
}
