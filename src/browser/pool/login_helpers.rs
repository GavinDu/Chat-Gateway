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
    let input_found = registry
        .get(&provider)
        .map(|a| {
            a.selector_set()
                .input
                .iter()
                .any(|s| page.query_selector(s).is_some())
        })
        .unwrap_or(false);
    let view_data = page.evaluate(login::VIEW_SCRIPT);
    // Public composers may coexist with sign-in controls. They establish
    // readiness, not account authentication; failed view evaluation is unknown.
    let auth_controls_present = view_data
        .get("auth_controls_present")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let (state, logged_in) = login::login_state(is_login, input_found, auth_controls_present);
    let (title, fields, buttons, snapshot) = login::parse_view(view_data);
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
