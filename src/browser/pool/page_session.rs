//! The only provider-page warm-up path for the thread-affine Obscura worker.
//!
//! Keep the existing cookie/import/navigation/localStorage ordering here so
//! startup, refresh and login-complete do not drift apart.
use crate::{providers::Provider, session};
use anyhow::Result;

pub(super) struct PageSession;

impl PageSession {
    /// Restore a provider snapshot using the single canonical warm-up order:
    /// cookies → navigation → localStorage → application reload.
    /// With no snapshot, this only navigates to the provider's login/chat entrypoint.
    pub(super) async fn open(
        browser: &obscura_core::Browser,
        page: &mut obscura_core::Page,
        provider: Provider,
        session_file: Option<&session::SessionFile>,
    ) -> Result<()> {
        let cookie_restore = session_file
            .map(|file| session::restore_cookies(browser, file, provider.name()))
            .transpose();
        // Navigate even when cookie import failed so the caller's fail-closed
        // cleanup can clear origin-scoped localStorage on the provider origin.
        page.goto(provider.login_url()).await?;
        cookie_restore?;
        if let Some(file) = session_file {
            if !file.local_storage.is_empty() {
                session::restore_local_storage(page, file)?;
                page.goto(provider.login_url()).await?;
            }
        }
        Ok(())
    }
}

/// Timestamp-only / ordering-only cookie events must not recreate a provider
/// page. A failed activation is never cached and is retried on the next sync.
pub(super) async fn activate_if_changed<F, Fut>(
    applied: &std::cell::RefCell<std::collections::HashMap<Provider, String>>,
    provider: Provider,
    snapshot: &session::SessionFile,
    activate: F,
) -> Result<bool>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let fingerprint = session::session_state_hash(snapshot)?;
    if applied.borrow().get(&provider) == Some(&fingerprint) {
        return Ok(false);
    }
    activate().await?;
    applied.borrow_mut().insert(provider, fingerprint);
    Ok(true)
}

/// Build the replacement completely before mutating the warm-page cache.
/// A failed cookie restore, navigation, or storage replay must not drop the
/// existing page; the import fingerprint remains retryable as well.
pub(super) async fn replace_after_success<T, F, Fut>(
    pages: &std::cell::RefCell<std::collections::HashMap<Provider, T>>,
    provider: Provider,
    build: F,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let replacement = build().await?;
    pages.borrow_mut().insert(provider, replacement);
    Ok(())
}

#[cfg(test)]
mod activation_tests {
    use super::*;
    #[tokio::test]
    async fn failed_refresh_keeps_existing_page_until_replacement_is_ready() {
        let pages = std::cell::RefCell::new(std::collections::HashMap::from([(
            Provider::Grok,
            String::from("warm-page"),
        )]));
        let failure = replace_after_success(&pages, Provider::Grok, || async {
            anyhow::bail!("synthetic cookie restore failure")
        })
        .await;
        assert!(failure.is_err());
        assert_eq!(pages.borrow().get(&Provider::Grok).map(String::as_str), Some("warm-page"));
        replace_after_success(&pages, Provider::Grok, || async {
            Ok(String::from("restored-page"))
        })
        .await
        .unwrap();
        assert_eq!(pages.borrow().get(&Provider::Grok).map(String::as_str), Some("restored-page"));
    }

    #[tokio::test]
    async fn identical_snapshots_do_not_reload_and_failed_activation_can_retry() {
        let applied = std::cell::RefCell::new(std::collections::HashMap::new());
        let mut file: session::SessionFile = serde_json::from_value(serde_json::json!({
            "cookies":[{"name":"synthetic","value":"one","domain":"grok.com","path":"/","secure":true,"http_only":true}],
            "local_storage":{}, "ts":"before"
        })).unwrap();
        let calls = std::cell::Cell::new(0);
        let apply = || async {
            calls.set(calls.get() + 1);
            Ok(())
        };
        assert!(activate_if_changed(&applied, Provider::Grok, &file, apply)
            .await
            .unwrap());
        for _ in 0..30 {
            file.ts = "later".into();
            assert!(!activate_if_changed(&applied, Provider::Grok, &file, apply)
                .await
                .unwrap());
        }
        assert_eq!(calls.get(), 1);
        file.cookies[0].value = "rotated".into();
        assert!(
            activate_if_changed(&applied, Provider::Grok, &file, || async {
                anyhow::bail!("synthetic activation failure")
            })
            .await
            .is_err()
        );
        assert!(activate_if_changed(&applied, Provider::Grok, &file, apply)
            .await
            .unwrap());
        assert_eq!(calls.get(), 2);
        applied.borrow_mut().remove(&Provider::Grok);
        assert!(activate_if_changed(&applied, Provider::Grok, &file, apply)
            .await
            .unwrap());
        assert_eq!(
            calls.get(),
            3,
            "unlink/new login invalidates activation idempotency"
        );
    }
}
