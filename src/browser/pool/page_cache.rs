//! Bounded idle-page ownership and exclusive leases for mutable browser driving.
use crate::providers::Provider;
use anyhow::Result;
use obscura_core::{Browser, Page};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    ops::{Deref, DerefMut},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub(super) type SharedPages = Rc<RefCell<ProviderPageCache>>;

/// Keep the entire source BrowserContext alive with a staged provider Page.
/// In the pinned engine Page alone is not an owned credential context: its
/// cookies, origin storage and provider socket bridge belong to the Browser.
/// Field order intentionally drops Page before the optional Browser owner.
#[derive(Clone)]
pub(super) struct CachedProviderPage {
    page: Rc<Page>,
    browser_owner: Option<Rc<Browser>>,
}

impl std::ops::Deref for CachedProviderPage {
    type Target = Page;

    fn deref(&self) -> &Self::Target {
        &self.page
    }
}

pub(super) struct ProviderPageCache {
    pages: HashMap<Provider, CachedProviderPage>,
    bootstrap_403: HashMap<Provider, Arc<AtomicBool>>,
    order: VecDeque<Provider>,
    capacity: usize,
}

impl ProviderPageCache {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            pages: HashMap::new(),
            bootstrap_403: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }
    pub(super) fn insert(&mut self, provider: Provider, page: Rc<Page>) {
        self.insert_with_bootstrap_status(provider, page, None);
    }
    pub(super) fn insert_with_bootstrap_status(
        &mut self,
        provider: Provider,
        page: Rc<Page>,
        script_403: Option<Arc<AtomicBool>>,
    ) {
        self.insert_entry(
            provider,
            CachedProviderPage {
                page,
                browser_owner: None,
            },
            script_403,
        );
    }

    /// Ownership-only entry point for a previously verified staged context.
    /// This does not import or promote credentials; production activation is
    /// separately generation-fenced by #1463 before it may call this method.
    #[allow(dead_code)]
    pub(super) fn insert_owned(
        &mut self,
        provider: Provider,
        page: Page,
        browser_owner: Rc<Browser>,
        script_403: Option<Arc<AtomicBool>>,
    ) {
        self.insert_entry(
            provider,
            CachedProviderPage {
                page: Rc::new(page),
                browser_owner: Some(browser_owner),
            },
            script_403,
        );
    }

    fn insert_entry(
        &mut self,
        provider: Provider,
        entry: CachedProviderPage,
        script_403: Option<Arc<AtomicBool>>,
    ) {
        // Drop the old Page before its Browser owner; a socket/stream attached
        // to an evicted Page cannot outlive its staged credential context.
        self.remove(&provider);
        while self.pages.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.remove(&oldest);
            }
        }
        self.order.push_back(provider);
        self.pages.insert(provider, entry);
        if let Some(observed) = script_403 {
            self.bootstrap_403.insert(provider, observed);
        }
    }
    pub(super) fn remove(&mut self, provider: &Provider) -> Option<CachedProviderPage> {
        self.order.retain(|value| value != provider);
        self.bootstrap_403.remove(provider);
        self.pages.remove(provider)
    }
}
impl Deref for ProviderPageCache {
    type Target = HashMap<Provider, CachedProviderPage>;
    fn deref(&self) -> &Self::Target {
        &self.pages
    }
}

/// Remove the idle page while its provider lock is held. No RefCell borrow
/// crosses await, and Drop restores ownership on success, error or cancellation.
pub(super) struct PageLease {
    page: Option<Page>,
    browser_owner: Option<Rc<Browser>>,
    pages: SharedPages,
    provider: Provider,
    bootstrap_403: Option<Arc<AtomicBool>>,
}
impl PageLease {
    pub(super) fn take(pages: &SharedPages, provider: Provider) -> Result<Self> {
        let observed = pages.borrow().bootstrap_403.get(&provider).cloned();
        let cached = pages
            .borrow_mut()
            .remove(&provider)
            .ok_or_else(|| anyhow::anyhow!("provider page is not available"))?;
        let CachedProviderPage { page, browser_owner } = cached;
        match Rc::try_unwrap(page) {
            Ok(page) => Ok(Self {
                page: Some(page),
                browser_owner,
                pages: pages.clone(),
                provider,
                bootstrap_403: observed,
            }),
            Err(page) => {
                pages.borrow_mut().insert_entry(
                    provider,
                    CachedProviderPage { page, browser_owner },
                    observed,
                );
                anyhow::bail!("provider page is still borrowed")
            }
        }
    }
    /// Direct response-observer evidence: Mistral first-party script HTTP 403 or
    /// Meta AI first-party document HTTP 403. This stays page-scoped and is
    /// discarded on replacement or eviction, never across providers.
    pub(super) fn observed_bootstrap_403(&self) -> bool {
        self.bootstrap_403
            .as_ref()
            .is_some_and(|observed| observed.load(Ordering::Relaxed))
    }
    pub(super) fn clear_bootstrap_403_before_navigation(&self) {
        if let Some(observed) = &self.bootstrap_403 {
            observed.store(false, Ordering::Relaxed);
        }
    }
}
impl Deref for PageLease {
    type Target = Page;
    fn deref(&self) -> &Page {
        self.page.as_ref().expect("lease owns page until drop")
    }
}
impl DerefMut for PageLease {
    fn deref_mut(&mut self) -> &mut Page {
        self.page.as_mut().expect("lease owns page until drop")
    }
}
impl Drop for PageLease {
    fn drop(&mut self) {
        if let Some(page) = self.page.take() {
            self.pages.borrow_mut().insert_entry(
                self.provider,
                CachedProviderPage {
                    page: Rc::new(page),
                    browser_owner: self.browser_owner.take(),
                },
                self.bootstrap_403.take(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn script_error_evidence_follows_only_its_original_page_and_lease() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let pages = Rc::new(RefCell::new(ProviderPageCache::new(2)));
        let first = Rc::new(browser.new_page().await.unwrap());
        let evidence = Arc::new(AtomicBool::new(true));
        pages
            .borrow_mut()
            .insert_with_bootstrap_status(Provider::Mistral, first, Some(evidence));
        let lease = PageLease::take(&pages, Provider::Mistral).unwrap();
        assert!(lease.observed_bootstrap_403());
        lease.clear_bootstrap_403_before_navigation();
        assert!(!lease.observed_bootstrap_403());
        drop(lease);
        assert!(!PageLease::take(&pages, Provider::Mistral)
            .unwrap()
            .observed_bootstrap_403());

        // Replacing a browser page must discard the old network status.
        let fresh = Rc::new(browser.new_page().await.unwrap());
        pages.borrow_mut().insert(Provider::Mistral, fresh);
        let replaced = PageLease::take(&pages, Provider::Mistral).unwrap();
        assert!(!replaced.observed_bootstrap_403());
    }

    #[tokio::test]
    async fn script_error_evidence_does_not_survive_cache_eviction() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let pages = Rc::new(RefCell::new(ProviderPageCache::new(1)));
        let first = Rc::new(browser.new_page().await.unwrap());
        pages.borrow_mut().insert_with_bootstrap_status(
            Provider::Mistral,
            first,
            Some(Arc::new(AtomicBool::new(true))),
        );
        let second = Rc::new(browser.new_page().await.unwrap());
        pages.borrow_mut().insert(Provider::ChatGPT, second);
        let replacement = Rc::new(browser.new_page().await.unwrap());
        pages.borrow_mut().insert(Provider::Mistral, replacement);
        let lease = PageLease::take(&pages, Provider::Mistral).unwrap();
        assert!(!lease.observed_bootstrap_403());
    }

    #[tokio::test]
    async fn metaai_document_denial_evidence_is_page_scoped_and_cleared_on_navigation() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let pages = Rc::new(RefCell::new(ProviderPageCache::new(2)));
        let first = Rc::new(browser.new_page().await.unwrap());
        pages.borrow_mut().insert_with_bootstrap_status(
            Provider::MetaAI,
            first,
            Some(Arc::new(AtomicBool::new(true))),
        );
        let evidence = PageLease::take(&pages, Provider::MetaAI).unwrap();
        assert!(evidence.observed_bootstrap_403());
        evidence.clear_bootstrap_403_before_navigation();
        assert!(!evidence.observed_bootstrap_403());
        drop(evidence);
        let fresh = Rc::new(browser.new_page().await.unwrap());
        pages.borrow_mut().insert(Provider::MetaAI, fresh);
        let replacement = PageLease::take(&pages, Provider::MetaAI).unwrap();
        assert!(!replacement.observed_bootstrap_403());
    }

    #[tokio::test]
    async fn isolated_browser_owner_survives_lease_and_drops_on_eviction() {
        let cfg: crate::config::AppConfig =
            serde_yaml::from_str("obscura:\\n  stealth: false\\nproviders: []\\n").unwrap();
        let browser = Rc::new(super::super::browser_factory::build_disposable_browser(&cfg).unwrap());
        let weak = Rc::downgrade(&browser);
        browser
            .cookies()
            .set("SID=stage-only; Secure; Path=/", "https://chatgpt.com/")
            .unwrap();
        let mut page = super::super::browser_factory::new_provider_page(&browser, Provider::ChatGPT)
            .await
            .unwrap();
        page.goto("data:text/html,<body>staged owner</body>").await.unwrap();
        let cache = Rc::new(RefCell::new(ProviderPageCache::new(1)));
        cache.borrow_mut().insert_owned(Provider::ChatGPT, page, browser.clone(), None);
        drop(browser);
        assert!(weak.upgrade().is_some(), "cache must own the staged Browser");
        let mut lease = PageLease::take(&cache, Provider::ChatGPT).unwrap();
        assert!(weak.upgrade().is_some(), "leased Page retains its Browser");
        lease.goto("data:text/html,<body>leased</body>").await.unwrap();
        assert_eq!(lease.evaluate("document.body.textContent"), serde_json::json!("leased"));
        drop(lease); // Cooperative cancellation or normal exit restores the same owner.
        assert!(weak.upgrade().is_some());
        assert_eq!(
            cache.borrow().get(&Provider::ChatGPT).unwrap().evaluate("document.body.textContent"),
            serde_json::json!("leased")
        );
        let other = obscura_core::Browser::builder().stealth(false).build().unwrap();
        let other_page = other.new_page().await.unwrap();
        cache.borrow_mut().insert(Provider::MetaAI, Rc::new(other_page));
        assert!(weak.upgrade().is_none(), "eviction drops staged Browser and Page");
    }

    #[tokio::test]
    async fn borrowed_owned_page_fails_lease_without_losing_context_or_evidence() {
        let cfg: crate::config::AppConfig =
            serde_yaml::from_str("obscura:\\n  stealth: false\\nproviders: []\\n").unwrap();
        let browser = Rc::new(super::super::browser_factory::build_disposable_browser(&cfg).unwrap());
        let weak = Rc::downgrade(&browser);
        let page = super::super::browser_factory::new_provider_page(&browser, Provider::MetaAI)
            .await
            .unwrap();
        let cache = Rc::new(RefCell::new(ProviderPageCache::new(1)));
        cache.borrow_mut().insert_owned(
            Provider::MetaAI,
            page,
            browser.clone(),
            Some(Arc::new(AtomicBool::new(true))),
        );
        drop(browser);
        let borrowed = cache.borrow().get(&Provider::MetaAI).unwrap().clone();
        assert!(PageLease::take(&cache, Provider::MetaAI).is_err());
        assert!(cache.borrow().contains_key(&Provider::MetaAI));
        assert!(weak.upgrade().is_some());
        drop(borrowed);
        let lease = PageLease::take(&cache, Provider::MetaAI).unwrap();
        assert!(lease.observed_bootstrap_403());
        drop(lease);
        assert!(weak.upgrade().is_some());
        cache.borrow_mut().remove(&Provider::MetaAI);
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn eviction_releases_old_pages_and_lease_returns_after_cancellation() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let pages = Rc::new(RefCell::new(ProviderPageCache::new(2)));
        let first = Rc::new(browser.new_page().await.unwrap());
        let weak = Rc::downgrade(&first);
        pages.borrow_mut().insert(Provider::ChatGPT, first);
        let second = Rc::new(browser.new_page().await.unwrap());
        pages.borrow_mut().insert(Provider::Grok, second);
        let third = Rc::new(browser.new_page().await.unwrap());
        pages.borrow_mut().insert(Provider::Mistral, third);
        assert!(
            weak.upgrade().is_none(),
            "evicted pages must not remain alive"
        );
        assert_eq!(pages.borrow().len(), 2);
        let mut lease = PageLease::take(&pages, Provider::Grok).unwrap();
        lease
            .goto("data:text/html,<textarea></textarea>")
            .await
            .unwrap();
        assert!(!pages.borrow().contains_key(&Provider::Grok));
        drop(lease);
        assert!(pages
            .borrow()
            .get(&Provider::Grok)
            .unwrap()
            .query_selector("textarea")
            .is_some());
        assert_eq!(pages.borrow().len(), 2);
    }
}
