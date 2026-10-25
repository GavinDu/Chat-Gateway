//! Browser login delegate UI — the Rust-form successor to Go's dashboard
//! login flow (internal/dashboard/login.go + remote_browser.go).
//!
//! Go's version drove a headed Chromium over CDP: JPEG screenshot frames
//! plus relayed mouse/keyboard/text input. The public obscura API cannot
//! do that: it has no screenshot API (screenshot() lives only in the
//! inner obscura-browser crate, unreachable because obscura::Page wraps
//! its inner page in a pub(crate) field), no keyboard/mouse API, and no
//! mutation-observer subscription. So this UI is RELAY-ONLY by design:
//! the user sees the login page's URL, title, detected form fields and
//! buttons, and a sanitized text snapshot, and drives the page by
//! relayed actions (click-by-selector, type-into-selector, Enter,
//! navigate, settle) executed on the browser thread via evaluate().
//!
//! SECURITY: the gateway never presents its own username/password form
//! and never stores credentials. Relayed text is held in memory only for
//! the single action that types it into the provider page; it is never
//! logged (no tracing of relay text anywhere), never persisted, and
//! never echoed back in any response. What IS persisted, on Complete,
//! is exactly the existing session format (cookies + localStorage) in
//! sessions/<provider>.json — see session::save_session. Field *values*
//! are never collected: the view script returns field metadata
//! (tag/type/placeholder/label/name/selector) only.

use serde::{Deserialize, Serialize};

/// The dashboard page itself (single self-contained HTML file, no build
/// step). Served at GET /login.
pub const LOGIN_HTML: &str = include_str!("login_page.html");

/// One detected form field on the login page. Metadata only — values are
/// never read, so a password field's content can never leak through here.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LoginField {
    pub tag: String,
    #[serde(rename = "type")]
    pub input_type: String,
    pub placeholder: String,
    pub label: String,
    pub name: String,
    /// Best-effort unique selector for this field (#id, [name=], or
    /// [placeholder=]; falls back to the bare tag).
    pub selector: String,
}

/// One clickable button on the login page (text only; clicked by index
/// via querySelectorAll, since button text is not a selector).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LoginButton {
    pub text: String,
    pub index: usize,
}

/// Live view of one provider's login page.
#[derive(Serialize, Clone, Debug)]
pub struct LoginView {
    pub provider: String,
    pub active: bool,
    pub url: Option<String>,
    pub title: Option<String>,
    /// "idle" | "login_required" | "likely_logged_in" | "active_unknown"
    pub state: String,
    pub logged_in: Option<bool>,
    pub input_found: bool,
    pub fields: Vec<LoginField>,
    pub buttons: Vec<LoginButton>,
    /// Truncated visible text of the page (first 1500 chars) to orient
    /// the user. No input values are included (fields carry no values).
    pub snapshot: String,
}

impl LoginView {
    pub fn inactive(provider: &str) -> Self {
        Self {
            provider: provider.to_string(),
            active: false,
            url: None,
            title: None,
            state: "idle".to_string(),
            logged_in: None,
            input_found: false,
            fields: vec![],
            buttons: vec![],
            snapshot: String::new(),
        }
    }
}

/// Best-effort logged-in signal: NOT a guarantee. URL keyword match
/// (login/signin/sign-in/auth/...) means the provider bounced us to a
/// login screen; presence of the provider's chat input selector means
/// the app booted past login. Either signal alone can mislead (a
/// logged-out chat page may still render an input; a logged-in user may
/// sit on an /auth callback URL briefly), so the UI shows both facts
/// separately and labels the verdict best-effort.
pub fn login_state(is_login_url: bool, input_found: bool) -> (String, Option<bool>) {
    if is_login_url {
        ("login_required".to_string(), Some(false))
    } else if input_found {
        ("likely_logged_in".to_string(), Some(true))
    } else {
        ("active_unknown".to_string(), None)
    }
}

/// JS run on the login page to collect view data. One IIFE, fully
/// try/catch-guarded (evaluate() swallows JS exceptions as Null anyway,
/// so a failure here degrades to an empty view, never an error). Field
/// values are deliberately never read.
pub const VIEW_SCRIPT: &str = r#"(function(){ try {
  var fields=[]; var els=document.querySelectorAll('input, textarea, [contenteditable="true"]');
  for (var i=0;i<els.length && fields.length<24;i++){ var el=els[i]; var tag=(el.tagName||'').toLowerCase();
    var id=el.id||''; var nm=el.getAttribute('name')||''; var ph=el.getAttribute('placeholder')||''; var lb=el.getAttribute('aria-label')||'';
    var sel=''; if(id) sel='#'+id; else if(nm) sel=tag+'[name="'+nm.replace(/"/g,'')+'"]'; else if(ph) sel=tag+'[placeholder="'+ph.replace(/"/g,'')+'"]'; else sel=tag;
    fields.push({tag:tag, type: el.getAttribute('type')||'', placeholder: ph.slice(0,80), label: lb.slice(0,80), name: nm.slice(0,80), selector: sel}); }
  var btns=[]; var bs=document.querySelectorAll('button, [role="button"]');
  for (var j=0;j<bs.length && btns.length<24;j++){ var t=(bs[j].innerText||'').trim().replace(/\s+/g,' ').slice(0,60); if(t) btns.push({text:t, index:j}); }
  var snap=''; try{ snap=(document.body?document.body.innerText:'').slice(0,1500); }catch(e){}
  return {title: document.title||'', fields: fields, buttons: btns, snapshot: snap};
} catch(e){ return {title:'', fields:[], buttons:[], snapshot:''}; } })()"#;

/// Reject navigation targets that can reach local services or use URL
/// authority tricks. SSO may use a different public HTTPS hostname, so this
/// is a network-safety baseline rather than a provider-domain allowlist.
fn validate_navigation_url(url: &str) -> anyhow::Result<()> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| anyhow::anyhow!("navigate target must be an https URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty()
        || authority.contains('@')
        || authority.contains('[')
        || authority.contains(']')
    {
        anyhow::bail!(
            "navigate target must use a public DNS hostname without userinfo or IP literals"
        );
    }
    let host = if let Some((host, port)) = authority.rsplit_once(':') {
        if port != "443" || host.contains(':') {
            anyhow::bail!("navigate target may only use the default HTTPS port");
        }
        host
    } else {
        authority
    }
    .to_ascii_lowercase();

    if host.len() > 253
        || !host.contains('.')
        || host.ends_with('.')
        || host.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
    {
        anyhow::bail!("navigate target must use a valid public DNS hostname");
    }
    if host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".test")
        || host.ends_with(".invalid")
        || host.ends_with(".example")
    {
        anyhow::bail!("navigate target may not use a local or reserved hostname");
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        anyhow::bail!("navigate target may not use an IP literal");
    }
    Ok(())
}

/// Apply a provider-scoped destination allowlist to login-relay navigation.
/// Generic HTTPS validation alone would let a caller navigate to an attacker
/// site and type provider credentials there. Common identity-provider domains
/// are included for expected OAuth/SSO hops.
pub fn validate_navigation_url_for_provider(provider: &str, url: &str) -> anyhow::Result<()> {
    validate_navigation_url(url)?;
    let rest = url
        .strip_prefix("https://")
        .expect("generic validator accepted HTTPS");
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .strip_suffix(":443")
        .unwrap_or(authority)
        .to_ascii_lowercase();
    const COMMON_IDP: &[&str] = &[
        "accounts.google.com",
        "google.com",
        "googleusercontent.com",
        "apple.com",
        "icloud.com",
        "microsoftonline.com",
        "live.com",
        "facebook.com",
        "github.com",
    ];
    let provider_domains: &[&str] = match provider {
        "qwen" => &["qwen.ai", "aliyun.com", "alibabacloud.com"],
        "metaai" => &["meta.ai", "meta.com", "facebook.com"],
        "claude" => &["claude.ai", "anthropic.com"],
        "chatgpt" => &["chatgpt.com", "openai.com", "auth0.com"],
        "gemini" => &["gemini.google.com", "google.com"],
        "grok" => &["grok.com", "x.com", "twitter.com", "x.ai"],
        "mistral" => &["mistral.ai"],
        "perplexity" => &["perplexity.ai"],
        _ => anyhow::bail!("unknown provider for login navigation"),
    };
    let allowed = provider_domains
        .iter()
        .chain(COMMON_IDP.iter())
        .any(|base| host == *base || host.ends_with(&format!(".{base}")));
    if !allowed {
        anyhow::bail!("navigation target is outside the provider and SSO allowlists");
    }
    Ok(())
}

fn parse_view_items<T: serde::de::DeserializeOwned>(value: Option<&serde_json::Value>) -> Vec<T> {
    value
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value::<T>(item.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the VIEW_SCRIPT result into its parts. Malformed individual field or
/// button entries are skipped without discarding otherwise valid metadata.
pub fn parse_view(value: serde_json::Value) -> (String, Vec<LoginField>, Vec<LoginButton>, String) {
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let fields = parse_view_items(value.get("fields"));
    let buttons = parse_view_items(value.get("buttons"));
    let snapshot = value
        .get("snapshot")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    (title, fields, buttons, snapshot)
}

/// Relay actions the UI can ask the browser thread to perform on a login
/// page. Text is never logged; see the module docs.
#[derive(Debug, Clone)]
pub enum LoginAction {
    /// Click the element matching a selector.
    Click { selector: String },
    /// Click the nth button in document order (from the view's list).
    ClickButton { index: usize },
    /// Type text into the element matching a selector (human_type).
    Type { selector: String, text: String },
    /// Dispatch Enter on the element matching a selector.
    Enter { selector: String },
    /// Drop the page and open a fresh one at an https URL (SSO hops).
    Navigate { url: String },
}

impl LoginAction {
    /// Validate an action decoded from the UI request. Bounds mirror Go's
    /// remote-input validation (4096-byte text cap, normalized points
    /// have no equivalent here since there is no coordinate input).
    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            LoginAction::Click { selector } | LoginAction::Enter { selector } => {
                if selector.trim().is_empty() || selector.len() > 500 {
                    anyhow::bail!("selector must be 1..=500 non-blank chars");
                }
                Ok(())
            }
            LoginAction::ClickButton { .. } => Ok(()),
            LoginAction::Type { selector, text } => {
                if selector.trim().is_empty() || selector.len() > 500 {
                    anyhow::bail!("selector must be 1..=500 non-blank chars");
                }
                if text.is_empty() || text.len() > 4096 {
                    anyhow::bail!("text must be 1..=4096 bytes");
                }
                Ok(())
            }
            LoginAction::Navigate { url } => validate_navigation_url(url),
        }
    }
}

#[cfg(test)]
mod login_action_validation_tests {
    use super::*;

    #[test]
    fn rejects_empty_and_whitespace_only_selectors_for_click_enter_and_type() {
        for selector in ["", " ", "\t", "\n", " \t "] {
            assert!(LoginAction::Click {
                selector: selector.to_string()
            }
            .validate()
            .is_err());
            assert!(LoginAction::Enter {
                selector: selector.to_string()
            }
            .validate()
            .is_err());
            assert!(LoginAction::Type {
                selector: selector.to_string(),
                text: "value".to_string(),
            }
            .validate()
            .is_err());
        }
    }

    #[test]
    fn accepts_non_blank_selectors_with_surrounding_whitespace() {
        assert!(LoginAction::Click {
            selector: " #submit ".to_string()
        }
        .validate()
        .is_ok());
    }
}

#[cfg(test)]
mod navigation_validation_tests {
    use super::*;

    #[test]
    fn provider_navigation_allows_own_domain_and_common_sso() {
        assert!(validate_navigation_url_for_provider("chatgpt", "https://chatgpt.com/").is_ok());
        assert!(validate_navigation_url_for_provider(
            "chatgpt",
            "https://accounts.google.com/o/oauth2/"
        )
        .is_ok());
        assert!(validate_navigation_url_for_provider(
            "claude",
            "https://login.microsoftonline.com/"
        )
        .is_ok());
    }

    #[test]
    fn provider_navigation_rejects_other_provider_and_arbitrary_domains() {
        assert!(validate_navigation_url_for_provider("chatgpt", "https://claude.ai/").is_err());
        assert!(
            validate_navigation_url_for_provider("chatgpt", "https://attacker.example.org/")
                .is_err()
        );
        assert!(validate_navigation_url_for_provider("unknown", "https://chatgpt.com/").is_err());
    }

    #[test]
    fn accepts_public_https_sso_host() {
        assert!(validate_navigation_url("https://accounts.example.org/oauth/callback").is_ok());
        assert!(validate_navigation_url("https://login.example.org:443/").is_ok());
    }

    #[test]
    fn rejects_non_https_and_local_targets() {
        assert!(validate_navigation_url("http://example.com").is_err());
        assert!(validate_navigation_url("https://localhost/").is_err());
        assert!(validate_navigation_url("https://service.local/").is_err());
        assert!(validate_navigation_url("https://127.0.0.1/").is_err());
        assert!(validate_navigation_url("https://192.168.1.20/").is_err());
        assert!(validate_navigation_url("https://example.com:8443/").is_err());
        assert!(validate_navigation_url("https://user@example.com/").is_err());
    }
}

#[cfg(test)]
mod parse_view_tests {
    use super::*;

    #[test]
    fn keeps_valid_entries_when_other_entries_are_malformed() {
        let value = serde_json::json!({
            "title": "Sign in",
            "fields": [
                {
                    "tag": "input",
                    "type": "email",
                    "placeholder": "Email",
                    "label": "Email",
                    "name": "email",
                    "selector": "#email"
                },
                { "tag": "input", "selector": "#broken" }
            ],
            "buttons": [
                { "text": "Continue", "index": 0 },
                { "text": 42, "index": "invalid" }
            ],
            "snapshot": "Sign in to continue"
        });

        let (title, fields, buttons, snapshot) = parse_view(value);
        assert_eq!(title, "Sign in");
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].selector, "#email");
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0].text, "Continue");
        assert_eq!(snapshot, "Sign in to continue");
    }

    #[test]
    fn unexpected_collection_shapes_degrade_to_empty_collections() {
        let value = serde_json::json!({
            "fields": { "unexpected": true },
            "buttons": "unexpected"
        });
        let (_, fields, buttons, _) = parse_view(value);
        assert!(fields.is_empty());
        assert!(buttons.is_empty());
    }
}

#[cfg(test)]
mod login_action_size_limit_tests {
    use super::*;

    #[test]
    fn accepts_selector_and_text_at_documented_limits() {
        let selector = "x".repeat(500);
        let text = "x".repeat(4096);
        assert!(LoginAction::Click {
            selector: selector.clone()
        }
        .validate()
        .is_ok());
        assert!(LoginAction::Enter {
            selector: selector.clone()
        }
        .validate()
        .is_ok());
        assert!(LoginAction::Type { selector, text }.validate().is_ok());
    }

    #[test]
    fn rejects_selector_and_text_over_documented_limits() {
        let selector = "x".repeat(501);
        let text = "x".repeat(4097);
        assert!(LoginAction::Click {
            selector: selector.clone()
        }
        .validate()
        .is_err());
        assert!(LoginAction::Enter {
            selector: selector.clone()
        }
        .validate()
        .is_err());
        assert!(LoginAction::Type {
            selector: selector.clone(),
            text: "value".to_string(),
        }
        .validate()
        .is_err());
        assert!(LoginAction::Type {
            selector: "#input".to_string(),
            text,
        }
        .validate()
        .is_err());
    }

    #[test]
    fn rejects_empty_type_payload_even_when_selector_is_valid() {
        assert!(LoginAction::Type {
            selector: "#input".to_string(),
            text: String::new(),
        }
        .validate()
        .is_err());
    }
}
