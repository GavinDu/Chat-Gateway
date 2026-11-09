//! Dedicated interactive-login page lifecycle and view projection.
use super::LoginActiveInfo;
use crate::{
    login::{self, LoginView},
    providers::{self, Provider},
};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

/// Bookkeeping for one active login page. last_seen drives the
/// abandoned-login sweep (Go swept after 35s of no client contact for
/// waiting jobs; here the UI polls every 5-8s, so a 30-minute idle
/// threshold fits a user working through a slow SSO flow).
pub(super) struct LoginMeta {
    pub(super) last_seen: Instant,
}

const LOGIN_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Drop login pages idle longer than LOGIN_IDLE_TIMEOUT (Go's
/// abandoned-job sweep, with a threshold fitted to the UI's 5-8s
/// polling instead of Go's 35s — a user mid-SSO must not lose the page
/// between polls).
pub(super) fn sweep_login_pages(
    login_pages: &Rc<RefCell<HashMap<Provider, Rc<obscura_core::Page>>>>,
    login_meta: &Rc<RefCell<HashMap<Provider, LoginMeta>>>,
) {
    let now = Instant::now();
    let stale: Vec<Provider> = login_meta
        .borrow()
        .iter()
        .filter(|(_, m)| now.duration_since(m.last_seen) > LOGIN_IDLE_TIMEOUT)
        .map(|(p, _)| *p)
        .collect();
    for p in stale {
        login_pages.borrow_mut().remove(&p);
        login_meta.borrow_mut().remove(&p);
    }
}

pub(super) fn touch_login_meta(
    login_meta: &Rc<RefCell<HashMap<Provider, LoginMeta>>>,
    provider: Provider,
) {
    if let Some(m) = login_meta.borrow_mut().get_mut(&provider) {
        m.last_seen = Instant::now();
    }
}

pub(super) fn build_login_view(
    provider: Provider,
    page: &obscura_core::Page,
    registry: &Rc<HashMap<Provider, Arc<dyn providers::SiteAdapter>>>,
) -> LoginView {
    let url = page.url();
    let is_login = providers::common::is_login_page(page);
    // A challenge can coexist with an old composer in a client-side app.
    // Never report that stale editor as ready while manual verification is
    // required; this observation is not proof of expired credentials.
    let challenge = crate::provider_guard::is_challenge_page(page);
    let input_found = !challenge
        && registry
            .get(&provider)
            .map(|a| {
                // Use configured selectors, but do not report hidden/disabled
                // SPA placeholders as usable composer controls.
                providers::common::visible_composer_exists(
                    page,
                    &a.effective_selectors().input_refs(),
                )
            })
            .unwrap_or(false);
    let (state, logged_in) = if challenge {
        ("challenge_required".to_owned(), None)
    } else {
        login::login_state(is_login, input_found)
    };
    let (title, fields, buttons, snapshot) = login::parse_view(page.evaluate(login::VIEW_SCRIPT));
    LoginView {
        provider: provider.name().to_string(),
        active: true,
        url: Some(url),
        title: Some(title),
        state,
        logged_in,
        input_found,
        fields,
        buttons,
        snapshot,
    }
}

pub(super) fn login_active_info(
    provider: Provider,
    page: &obscura_core::Page,
    registry: &Rc<HashMap<Provider, Arc<dyn providers::SiteAdapter>>>,
) -> LoginActiveInfo {
    let view = build_login_view(provider, page, registry);
    LoginActiveInfo {
        provider: provider.name().to_string(),
        url: view.url.unwrap_or_default(),
        title: view.title.unwrap_or_default(),
        logged_in: view.logged_in,
        input_found: view.input_found,
        state: view.state,
    }
}

#[cfg(test)]
mod login_effective_selectors_tests {
    use super::*;

    #[tokio::test]
    async fn visible_composer_on_challenge_page_is_not_reported_ready() {
        let cfg: crate::config::AppConfig = serde_yaml::from_str(
            r#"
providers:
  - name: gemini
    selectors:
      input:
        - "textarea#custom-gemini-composer"
"#,
        )
        .unwrap();
        let registry = Rc::new(providers::build_registry(&cfg));
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <p>Please verify you are human to continue.</p>
            <textarea id="custom-gemini-composer"></textarea>
        </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let view = build_login_view(Provider::Gemini, &page, &registry);
        assert_eq!(view.state, "challenge_required");
        assert!(!view.input_found);
        assert_eq!(view.logged_in, None);
        let status = login_active_info(Provider::Gemini, &page, &registry);
        assert_eq!(status.state, "challenge_required");
        assert!(!status.input_found);
    }

    #[tokio::test]
    async fn custom_composer_selector_is_visible_to_login_status_without_auth_claim() {
        let cfg: crate::config::AppConfig = serde_yaml::from_str(
            r#"
providers:
  - name: gemini
    selectors:
      input:
        - "textarea#custom-gemini-composer"
"#,
        )
        .unwrap();
        let registry = Rc::new(providers::build_registry(&cfg));
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body><textarea id='custom-gemini-composer'></textarea></body></html>")
            .await
            .unwrap();

        let view = build_login_view(Provider::Gemini, &page, &registry);
        assert!(view.input_found, "custom composer should be discovered");
        assert_eq!(view.state, "composer_available");
        assert_eq!(view.logged_in, None, "composer alone cannot prove login");
        let status = login_active_info(Provider::Gemini, &page, &registry);
        assert!(status.input_found);
        assert_eq!(status.logged_in, None);
    }
}
