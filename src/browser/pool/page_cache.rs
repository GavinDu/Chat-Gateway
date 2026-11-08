//! Bounded idle-page ownership and exclusive leases for mutable browser driving.
use crate::providers::Provider;
use anyhow::Result;
use obscura_core::Page;
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    ops::{Deref, DerefMut},
    rc::Rc,
};

pub(super) type SharedPages = Rc<RefCell<ProviderPageCache>>;

pub(super) struct ProviderPageCache {
    pages: HashMap<Provider, Rc<Page>>,
    order: VecDeque<Provider>,
    capacity: usize,
}

impl ProviderPageCache {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            pages: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }
    pub(super) fn insert(&mut self, provider: Provider, page: Rc<Page>) {
        self.remove(&provider);
        while self.pages.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.pages.remove(&oldest);
            }
        }
        self.order.push_back(provider);
        self.pages.insert(provider, page);
    }
    pub(super) fn remove(&mut self, provider: &Provider) -> Option<Rc<Page>> {
        self.order.retain(|value| value != provider);
        self.pages.remove(provider)
    }
}
impl Deref for ProviderPageCache {
    type Target = HashMap<Provider, Rc<Page>>;
    fn deref(&self) -> &Self::Target {
        &self.pages
    }
}

/// Remove the idle page while its provider lock is held. No RefCell borrow
/// crosses await, and Drop restores ownership on success, error or cancellation.
pub(super) struct PageLease {
    page: Option<Page>,
    pages: SharedPages,
    provider: Provider,
}
impl PageLease {
    pub(super) fn take(pages: &SharedPages, provider: Provider) -> Result<Self> {
        let page = pages
            .borrow_mut()
            .remove(&provider)
            .ok_or_else(|| anyhow::anyhow!("provider page is not available"))?;
        match Rc::try_unwrap(page) {
            Ok(page) => Ok(Self {
                page: Some(page),
                pages: pages.clone(),
                provider,
            }),
            Err(page) => {
                pages.borrow_mut().insert(provider, page);
                anyhow::bail!("provider page is still borrowed")
            }
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
            self.pages.borrow_mut().insert(self.provider, Rc::new(page));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
