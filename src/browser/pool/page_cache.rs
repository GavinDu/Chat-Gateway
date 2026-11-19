//! Bounded idle-page ownership and exclusive leases for mutable browser driving.
use crate::providers::Provider;
use anyhow::Result;
use obscura_core::{Browser, Page};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
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
    // A lease may be cancelled after logout or page replacement. Only the
    // exact entry generation it took may be restored on Drop.
    epochs: HashMap<Provider, u64>,
    next_epoch: u64,
    // Non-secret worker-local tombstones: an evicted isolated Page must never
    // fall back into the shared Browser when a provider is requested again.
    // Full same-generation private rehydration is the separate #1471 gate.
    isolated_once: HashSet<Provider>,
}

impl ProviderPageCache {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            pages: HashMap::new(),
            bootstrap_403: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
            epochs: HashMap::new(),
            next_epoch: 0,
            isolated_once: HashSet::new(),
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
        if matches!(provider, Provider::ChatGPT | Provider::MetaAI) {
            self.isolated_once.insert(provider);
        }
        self.insert_entry(
            provider,
            CachedProviderPage {
                page: Rc::new(page),
                browser_owner: Some(browser_owner),
            },
            script_403,
        );
    }

    /// Shared-jar replay is unsafe once this worker has admitted an isolated
    /// context, even when its one-Page LRU has since evicted the Page. Do not
    /// mistake a cold cache for permission to rewrite the global cookie jar.
    pub(super) fn require_legacy_shared_replay_safe(&self, provider: Provider) -> Result<()> {
        if self.isolated_once.contains(&provider) {
            anyhow::bail!("isolated_context_rehydration_required");
        }
        Ok(())
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
        self.next_epoch = self
            .next_epoch
            .checked_add(1)
            .expect("provider page epoch exhausted");
        self.epochs.insert(provider, self.next_epoch);
        self.pages.insert(provider, entry);
        if let Some(observed) = script_403 {
            self.bootstrap_403.insert(provider, observed);
        }
    }
    pub(super) fn remove(&mut self, provider: &Provider) -> Option<CachedProviderPage> {
        self.order.retain(|value| value != provider);
        self.bootstrap_403.remove(provider);
        // Leave an epoch tombstone even when the Page is checked out in a
        // lease. Its delayed destructor must never undo explicit unlink.
        self.epochs.remove(provider);
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
    epoch: u64,
}
impl PageLease {
    pub(super) fn take(pages: &SharedPages, provider: Provider) -> Result<Self> {
        let (cached, observed, epoch) = {
            let mut cache = pages.borrow_mut();
            let cached = cache
                .pages
                .remove(&provider)
                .ok_or_else(|| anyhow::anyhow!("provider page is not available"))?;
            cache.order.retain(|entry| *entry != provider);
            let observed = cache.bootstrap_403.remove(&provider);
            let epoch = cache
                .epochs
                .get(&provider)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("provider page generation is not available"))?;
            (cached, observed, epoch)
        };
        let CachedProviderPage {
            page,
            browser_owner,
        } = cached;
        match Rc::try_unwrap(page) {
            Ok(page) => Ok(Self {
                page: Some(page),
                browser_owner,
                pages: pages.clone(),
                provider,
                bootstrap_403: observed,
                epoch,
            }),
            Err(page) => {
                pages.borrow_mut().insert_entry(
                    provider,
                    CachedProviderPage {
                        page,
                        browser_owner,
                    },
                    observed,
                );
                anyhow::bail!("provider page is still borrowed")
            }
        }
    }
    /// Match session persistence to the Browser whose cookie jar backs this
    /// exact leased Page. Existing unowned pages still use the unchanged
    /// shared worker Browser. A future isolated Page must never save cookies
    /// from the unrelated legacy jar.
    pub(super) fn browser_for_session_save(&self, shared: &Rc<Browser>) -> Rc<Browser> {
        Rc::clone(self.browser_owner.as_ref().unwrap_or(shared))
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
            let mut cache = self.pages.borrow_mut();
            // A newer provider may fill the bounded cache while this Page is
            // leased. Its late Drop must not evict that newer active entry.
            if cache.epochs.get(&self.provider) == Some(&self.epoch)
                && cache.pages.len() < cache.capacity
            {
                cache.insert_entry(
                    self.provider,
                    CachedProviderPage {
                        page: Rc::new(page),
                        browser_owner: self.browser_owner.take(),
                    },
                    self.bootstrap_403.take(),
                );
            }
            // Otherwise this was a stale lease after logout/replacement.
            // Drop Page and its previous Browser owner, never resurrect it.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn evicted_isolated_page_never_reenters_global_cookie_jar() {
        let global = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        global
            .cookies()
            .set(
                "SID=global-untouched; Secure; Path=/",
                "https://chatgpt.com/",
            )
            .unwrap();
        let private_a = Rc::new(
            obscura_core::Browser::builder()
                .stealth(false)
                .build()
                .unwrap(),
        );
        private_a
            .cookies()
            .set("SID=private-a; Secure; Path=/", "https://chatgpt.com/")
            .unwrap();
        let private_b = Rc::new(
            obscura_core::Browser::builder()
                .stealth(false)
                .build()
                .unwrap(),
        );
        private_b
            .cookies()
            .set("SID=private-b; Secure; Path=/", "https://meta.ai/")
            .unwrap();
        let a = private_a.new_page().await.unwrap();
        let b = private_b.new_page().await.unwrap();
        let a_weak = Rc::downgrade(&private_a);
        let pages = Rc::new(RefCell::new(ProviderPageCache::new(1)));
        pages
            .borrow_mut()
            .insert_owned(Provider::ChatGPT, a, private_a.clone(), None);
        drop(private_a);
        pages
            .borrow_mut()
            .insert_owned(Provider::MetaAI, b, private_b.clone(), None);
        assert!(a_weak.upgrade().is_none(), "A's evicted Browser is dropped");
        assert!(pages.borrow().get(&Provider::ChatGPT).is_none());
        assert!(pages
            .borrow()
            .require_legacy_shared_replay_safe(Provider::ChatGPT)
            .is_err());
        let error = super::super::worker::ensure_provider_page(&global, &pages, Provider::ChatGPT)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "isolated_context_rehydration_required");
        assert_eq!(global.cookies().get_all()[0].value, "global-untouched");
        assert_eq!(private_b.cookies().get_all()[0].value, "private-b");
        assert!(pages.borrow().contains_key(&Provider::MetaAI));
        // Fresh legacy cold provider remains eligible; no other adapter is
        // marked isolated by the two active providers' private context.
        assert!(pages
            .borrow()
            .require_legacy_shared_replay_safe(Provider::Claude)
            .is_ok());
    }

    #[tokio::test]
    async fn stale_lease_drop_does_not_erase_private_replay_tombstone() {
        let browser = Rc::new(
            obscura_core::Browser::builder()
                .stealth(false)
                .build()
                .unwrap(),
        );
        let page = browser.new_page().await.unwrap();
        let pages = Rc::new(RefCell::new(ProviderPageCache::new(1)));
        pages
            .borrow_mut()
            .insert_owned(Provider::ChatGPT, page, browser, None);
        let lease = PageLease::take(&pages, Provider::ChatGPT).unwrap();
        // Explicit cache invalidation during a lease prevents its late Drop
        // from resurrecting an owned context or permitting global replay.
        pages.borrow_mut().remove(&Provider::ChatGPT);
        drop(lease);
        assert!(!pages.borrow().contains_key(&Provider::ChatGPT));
        assert!(pages
            .borrow()
            .require_legacy_shared_replay_safe(Provider::ChatGPT)
            .is_err());
        // A brand-new worker cache has no historical owner until one is
        // admitted; it must not infer login success from this empty marker.
        assert!(ProviderPageCache::new(1)
            .require_legacy_shared_replay_safe(Provider::ChatGPT)
            .is_ok());
    }

    #[tokio::test]
    async fn session_save_browser_tracks_exact_owned_page_not_global_cookie_jar() {
        let shared = Rc::new(
            obscura_core::Browser::builder()
                .stealth(false)
                .build()
                .unwrap(),
        );
        shared
            .cookies()
            .set("SID=global-only; Secure; Path=/", "https://chatgpt.com/")
            .unwrap();
        let cfg: crate::config::AppConfig =
            serde_yaml::from_str("obscura:\n  stealth: false\nproviders: []\n").unwrap();
        let isolated =
            Rc::new(super::super::browser_factory::build_disposable_browser(&cfg).unwrap());
        isolated
            .cookies()
            .set("SID=isolated-only; Secure; Path=/", "https://chatgpt.com/")
            .unwrap();
        let owned_page =
            super::super::browser_factory::new_provider_page(&isolated, Provider::ChatGPT)
                .await
                .unwrap();
        let pages = Rc::new(RefCell::new(ProviderPageCache::new(2)));
        pages
            .borrow_mut()
            .insert_owned(Provider::ChatGPT, owned_page, isolated.clone(), None);
        let lease = PageLease::take(&pages, Provider::ChatGPT).unwrap();
        let selected = lease.browser_for_session_save(&shared);
        assert!(Rc::ptr_eq(&selected, &isolated));
        assert!(!Rc::ptr_eq(&selected, &shared));
        assert_eq!(selected.cookies().get_all()[0].value, "isolated-only");
        assert_eq!(shared.cookies().get_all()[0].value, "global-only");
        drop(lease);
        drop(selected);

        let legacy = shared.new_page().await.unwrap();
        pages.borrow_mut().insert(Provider::MetaAI, Rc::new(legacy));
        let legacy_lease = PageLease::take(&pages, Provider::MetaAI).unwrap();
        let selected_legacy = legacy_lease.browser_for_session_save(&shared);
        assert!(Rc::ptr_eq(&selected_legacy, &shared));
        assert_eq!(selected_legacy.cookies().get_all()[0].value, "global-only");
    }

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
            serde_yaml::from_str("obscura:\n  stealth: false\nproviders: []\n").unwrap();
        let browser =
            Rc::new(super::super::browser_factory::build_disposable_browser(&cfg).unwrap());
        let weak = Rc::downgrade(&browser);
        browser
            .cookies()
            .set("SID=stage-only; Secure; Path=/", "https://chatgpt.com/")
            .unwrap();
        let mut page =
            super::super::browser_factory::new_provider_page(&browser, Provider::ChatGPT)
                .await
                .unwrap();
        page.goto("data:text/html,<body>staged owner</body>")
            .await
            .unwrap();
        let cache = Rc::new(RefCell::new(ProviderPageCache::new(1)));
        cache
            .borrow_mut()
            .insert_owned(Provider::ChatGPT, page, browser.clone(), None);
        drop(browser);
        assert!(
            weak.upgrade().is_some(),
            "cache must own the staged Browser"
        );
        let mut lease = PageLease::take(&cache, Provider::ChatGPT).unwrap();
        assert!(weak.upgrade().is_some(), "leased Page retains its Browser");
        lease
            .goto("data:text/html,<body>leased</body>")
            .await
            .unwrap();
        assert_eq!(
            lease.evaluate("document.body.textContent"),
            serde_json::json!("leased")
        );
        drop(lease); // Cooperative cancellation or normal exit restores the same owner.
        assert!(weak.upgrade().is_some());
        assert_eq!(
            cache
                .borrow()
                .get(&Provider::ChatGPT)
                .unwrap()
                .evaluate("document.body.textContent"),
            serde_json::json!("leased")
        );
        let other = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let other_page = other.new_page().await.unwrap();
        cache
            .borrow_mut()
            .insert(Provider::MetaAI, Rc::new(other_page));
        assert!(
            weak.upgrade().is_none(),
            "eviction drops staged Browser and Page"
        );
    }

    #[tokio::test]
    async fn stale_owned_page_lease_cannot_resurrect_after_provider_removal() {
        let cfg: crate::config::AppConfig =
            serde_yaml::from_str("obscura:\n  stealth: false\nproviders: []\n").unwrap();
        let browser =
            Rc::new(super::super::browser_factory::build_disposable_browser(&cfg).unwrap());
        let weak = Rc::downgrade(&browser);
        let page = super::super::browser_factory::new_provider_page(&browser, Provider::ChatGPT)
            .await
            .unwrap();
        let pages = Rc::new(RefCell::new(ProviderPageCache::new(1)));
        pages
            .borrow_mut()
            .insert_owned(Provider::ChatGPT, page, browser.clone(), None);
        drop(browser);
        let lease = PageLease::take(&pages, Provider::ChatGPT).unwrap();
        // Explicit unlink while a Page is checked out leaves a tombstone.
        pages.borrow_mut().remove(&Provider::ChatGPT);
        drop(lease);
        assert!(!pages.borrow().contains_key(&Provider::ChatGPT));
        assert!(weak.upgrade().is_none(), "stale stage owner must be freed");
    }

    #[tokio::test]
    async fn late_owned_lease_cannot_evict_newer_provider_at_capacity() {
        let cfg: crate::config::AppConfig =
            serde_yaml::from_str("obscura:\n  stealth: false\nproviders: []\n").unwrap();
        let browser =
            Rc::new(super::super::browser_factory::build_disposable_browser(&cfg).unwrap());
        let weak = Rc::downgrade(&browser);
        let page = super::super::browser_factory::new_provider_page(&browser, Provider::ChatGPT)
            .await
            .unwrap();
        let cache = Rc::new(RefCell::new(ProviderPageCache::new(1)));
        cache
            .borrow_mut()
            .insert_owned(Provider::ChatGPT, page, browser.clone(), None);
        drop(browser);
        let lease = PageLease::take(&cache, Provider::ChatGPT).unwrap();
        let other = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let other_page = other.new_page().await.unwrap();
        cache
            .borrow_mut()
            .insert(Provider::MetaAI, Rc::new(other_page));
        drop(lease);
        assert_eq!(cache.borrow().len(), 1);
        assert!(cache.borrow().contains_key(&Provider::MetaAI));
        assert!(!cache.borrow().contains_key(&Provider::ChatGPT));
        assert!(
            weak.upgrade().is_none(),
            "stale stage cannot evict newer Page"
        );
    }

    #[tokio::test]
    async fn borrowed_owned_page_fails_lease_without_losing_context_or_evidence() {
        let cfg: crate::config::AppConfig =
            serde_yaml::from_str("obscura:\n  stealth: false\nproviders: []\n").unwrap();
        let browser =
            Rc::new(super::super::browser_factory::build_disposable_browser(&cfg).unwrap());
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
