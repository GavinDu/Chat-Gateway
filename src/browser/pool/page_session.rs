//! The only provider-page warm-up path for the thread-affine Obscura worker.
//!
//! Keep the existing cookie/import/navigation/localStorage ordering here so
//! startup, refresh and login-complete do not drift apart.
use crate::{providers::Provider, session};
use anyhow::{Context as _, Result};
use std::time::Duration;

/// Time-bound the entire session replay, not only the first navigation.
/// This runs on the worker's existing thread; non-Send Obscura pages never
/// cross runtimes. Cancellation can stop cooperative futures but cannot
/// preempt synchronous JavaScript that blocks the worker thread.
const SESSION_WARMUP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(super) struct ReplayRequiresIsolatedContext;

impl std::fmt::Display for ReplayRequiresIsolatedContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("replay_deferred_requires_isolated_context")
    }
}

impl std::error::Error for ReplayRequiresIsolatedContext {}

/// A normal send can reuse the same authenticated page and cookie jar.
/// Keep the known applied marker for active recovery providers in that case,
/// so the next relay import remains protected. A newly created page cannot
/// inherit the previous page's applied-state assertion.
pub(super) fn retain_applied_after_send(provider: Provider, newly_opened: bool) -> bool {
    !newly_opened && matches!(provider, Provider::ChatGPT | Provider::MetaAI)
}

/// The pinned Obscura Browser shares a cookie jar across its pages.
/// Before mutating or invalidating first-party proof, preserve a known-applied
/// live ChatGPT/Meta AI page against destructive replacement. An identical
/// snapshot and a cold page remain eligible without forcing a resync.
pub(super) fn guard_live_replay(
    provider: Provider,
    applied_fingerprint: Option<&str>,
    candidate_fingerprint: &str,
    live_page_present: bool,
) -> Result<()> {
    if matches!(provider, Provider::ChatGPT | Provider::MetaAI)
        && live_page_present
        && applied_fingerprint.is_some_and(|applied| applied != candidate_fingerprint)
    {
        return Err(ReplayRequiresIsolatedContext.into());
    }
    Ok(())
}

/// A verified first-party session on a live page must not be replaced merely
/// because its applied fingerprint marker was cleared by page lifecycle.
/// Without a private staging jar there is no safe rollback of shared cookies.
/// This safety-only predicate includes a formerly positive observation after
/// its five-minute auth-status TTL expires. Never expose it as a verified login.
/// Keep cold/unverified first import eligible, and preserve the existing known
/// fingerprint mismatch check before touching credential-verification epochs.
pub(super) fn guard_verified_live_replay(
    provider: Provider,
    applied_fingerprint: Option<&str>,
    candidate_fingerprint: &str,
    live_page_present: bool,
    prior_first_party_positive: bool,
) -> Result<()> {
    guard_live_replay(
        provider,
        applied_fingerprint,
        candidate_fingerprint,
        live_page_present,
    )?;
    if matches!(provider, Provider::ChatGPT | Provider::MetaAI)
        && live_page_present
        && applied_fingerprint.is_none()
        && prior_first_party_positive
    {
        return Err(ReplayRequiresIsolatedContext.into());
    }
    Ok(())
}

async fn bounded_warmup<F>(timeout: Duration, warmup: F) -> Result<()>
where
    F: std::future::Future<Output = Result<()>>,
{
    tokio::time::timeout(timeout, warmup)
        .await
        .context("provider session navigation timed out")?
}

/// A failed cookie restore is not a source-backed account rejection.
/// ChatGPT/Meta AI must not navigate a new candidate Page with malformed or
/// partially replayed cookies. Deferred providers retain their historical
/// order because their failed-restore cleanup still needs an origin page.
/// This does NOT roll back cookie writes that occurred before an error.
async fn navigate_after_cookie_restore<F, Fut>(
    provider: Provider,
    cookie_restore: Result<()>,
    navigate: F,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    if matches!(provider, Provider::ChatGPT | Provider::MetaAI) {
        cookie_restore?;
        navigate().await
    } else {
        navigate().await?;
        cookie_restore
    }
}

/// Only the active recovery providers require first-party replay storage
/// enforcement; deferred adapters retain their established legacy behavior.
/// The actual URL is intentionally never incorporated into an error, because
/// a redirect query can contain private access tokens or account metadata.
fn checked_storage_replay_origin(provider: Provider, actual_url: &str) -> Result<Option<String>> {
    if !matches!(provider, Provider::ChatGPT | Provider::MetaAI) {
        return Ok(None);
    }
    if !super::pool_policy::provider_thread_url_is_same_origin(actual_url, provider.login_url()) {
        anyhow::bail!("provider_storage_origin_mismatch");
    }
    let origin = super::pool_policy::provider_storage_origin(provider.login_url())
        .ok_or_else(|| anyhow::anyhow!("provider_storage_origin_mismatch"))?;
    Ok(Some(origin))
}

#[cfg(test)]
mod storage_replay_origin_guard_tests {
    use super::*;

    #[test]
    fn first_party_current_url_can_write_storage_but_never_certifies_login() {
        assert_eq!(
            checked_storage_replay_origin(Provider::ChatGPT, "https://chatgpt.com/")
                .unwrap()
                .as_deref(),
            Some("https://chatgpt.com")
        );
        assert_eq!(
            checked_storage_replay_origin(
                Provider::MetaAI,
                "https://www.meta.ai/chat/?private=synthetic"
            )
            .unwrap()
            .as_deref(),
            Some("https://www.meta.ai")
        );
        // Deferred providers remain on the same legacy replay path.
        assert_eq!(
            checked_storage_replay_origin(Provider::Claude, "https://login.example/").unwrap(),
            None
        );
    }

    #[test]
    fn off_origin_redirect_is_rejected_without_leaking_page_url_or_storage() {
        for (provider, redirect) in [
            (
                Provider::ChatGPT,
                "https://accounts.google.com/login?token=private",
            ),
            (Provider::ChatGPT, "https://chatgpt.com.evil.example/"),
            (Provider::ChatGPT, "https://chatgpt.com@evil.example/"),
            (Provider::ChatGPT, "http://chatgpt.com/"),
            (Provider::MetaAI, "https://meta.ai/"),
            (Provider::MetaAI, "https://www.meta.ai.evil.example/"),
            (
                Provider::MetaAI,
                "https://www.facebook.com/login/?account=private",
            ),
        ] {
            let error = checked_storage_replay_origin(provider, redirect).unwrap_err();
            assert_eq!(error.to_string(), "provider_storage_origin_mismatch");
            assert!(!error.to_string().contains("private"));
            assert!(!error.to_string().contains("evil"));
        }
    }
}

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
        // Pinned Obscura ReadableStream lacks underlyingSource.pull(). Only
        // ChatGPT may receive the shape-gated compatibility preload, and it
        // must run before page scripts (including on the storage reload).
        // The JS itself also verifies the exact HTTPS first-party origin.
        if provider == Provider::ChatGPT {
            page.add_preload_script(include_str!("chatgpt_streams.js"));
        }
        bounded_warmup(SESSION_WARMUP_TIMEOUT, async {
            let cookie_restore = session_file
                .map(|file| session::restore_cookies(browser, file, provider.name()))
                .transpose()
                .map(|_| ());
            // Active providers fail before navigation on an import error:
            // a partially applied cookie jar is not a reason to load provider
            // scripts or a login route. Deferred providers keep their legacy
            // navigation-before-error order for origin cleanup.
            navigate_after_cookie_restore(provider, cookie_restore, || async {
                page.goto(provider.login_url()).await?;
                Ok(())
            })
            .await?;
            if let Some(file) = session_file {
                if !file.local_storage.is_empty() {
                    // Navigation may have redirected to a foreign login/SSO
                    // origin. Check actual URL before serializing secrets and
                    // check JS location.origin again inside every setItem.
                    match checked_storage_replay_origin(provider, &page.url())? {
                        Some(origin) => {
                            session::restore_local_storage_for_origin(page, file, &origin)?
                        }
                        None => session::restore_local_storage(page, file)?,
                    }
                    page.goto(provider.login_url()).await?;
                }
            }
            Ok(())
        })
        .await
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

/// A manual LoginComplete must not revoke the known-applied snapshot digest
/// until the replacement credential snapshot was actually persisted. A failed
/// save or cancelled future preserves the old marker so a later relay import
/// cannot mistake a failed save for a successfully rotated credential set.
/// No RefCell borrow survives the awaited persistence operation.
pub(super) async fn invalidate_applied_after_login_save<F, T>(
    applied: &std::cell::RefCell<std::collections::HashMap<Provider, String>>,
    provider: Provider,
    persist: F,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    // Persistence errors may include private vault paths or storage details.
    // This command boundary reports only a fixed diagnostic category.
    let outcome = persist
        .await
        .map_err(|_| anyhow::anyhow!("login_session_snapshot_save_failed"))?;
    applied.borrow_mut().remove(&provider);
    Ok(outcome)
}

#[cfg(test)]
mod login_save_marker_commit_tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[tokio::test]
    async fn failed_save_leaves_old_applied_marker_and_other_provider_unchanged() {
        let markers = RefCell::new(HashMap::from([
            (Provider::ChatGPT, "old-chatgpt-generation".to_owned()),
            (Provider::MetaAI, "other-metaai-generation".to_owned()),
        ]));
        let error = invalidate_applied_after_login_save(&markers, Provider::ChatGPT, async {
            Err::<(), _>(anyhow::anyhow!("private-failing-save-path"))
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "login_session_snapshot_save_failed");
        assert_eq!(
            markers.borrow().get(&Provider::ChatGPT).map(String::as_str),
            Some("old-chatgpt-generation")
        );
        assert_eq!(
            markers.borrow().get(&Provider::MetaAI).map(String::as_str),
            Some("other-metaai-generation")
        );
    }

    #[tokio::test]
    async fn successful_save_invalidates_only_committed_provider() {
        let markers = RefCell::new(HashMap::from([
            (Provider::ChatGPT, "old-chatgpt-generation".to_owned()),
            (Provider::MetaAI, "other-metaai-generation".to_owned()),
        ]));
        let confirmed = invalidate_applied_after_login_save(&markers, Provider::ChatGPT, async {
            Ok::<_, anyhow::Error>(true)
        })
        .await
        .unwrap();
        assert!(confirmed);
        assert!(!markers.borrow().contains_key(&Provider::ChatGPT));
        assert_eq!(
            markers.borrow().get(&Provider::MetaAI).map(String::as_str),
            Some("other-metaai-generation")
        );
    }

    #[tokio::test]
    async fn cancelled_save_cannot_revoke_live_applied_marker() {
        let markers = RefCell::new(HashMap::from([(
            Provider::ChatGPT,
            "old-chatgpt-generation".to_owned(),
        )]));
        let cancelled = tokio::time::timeout(
            Duration::from_millis(10),
            invalidate_applied_after_login_save(
                &markers,
                Provider::ChatGPT,
                std::future::pending::<Result<()>>(),
            ),
        )
        .await;
        assert!(cancelled.is_err());
        assert_eq!(
            markers.borrow().get(&Provider::ChatGPT).map(String::as_str),
            Some("old-chatgpt-generation")
        );
    }
}

/// Build the replacement completely before mutating the warm-page cache.
/// A failed cookie restore, navigation, or storage replay must not drop the
/// existing page; the import fingerprint remains retryable as well.
pub(super) async fn replace_after_success<T, F, Fut, Insert>(build: F, insert: Insert) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
    Insert: FnOnce(T),
{
    let replacement = build().await?;
    insert(replacement);
    Ok(())
}

#[cfg(test)]
mod cookie_restore_navigation_safety_tests {
    use super::*;

    #[tokio::test]
    async fn active_cookie_restore_errors_abort_before_any_navigation() {
        for provider in [Provider::ChatGPT, Provider::MetaAI] {
            let navigations = std::cell::Cell::new(0_u8);
            let error = navigate_after_cookie_restore(
                provider,
                Err(anyhow::anyhow!("private-token-in-invalid-cookie")),
                || async {
                    navigations.set(navigations.get() + 1);
                    Ok(())
                },
            )
            .await
            .unwrap_err();
            assert_eq!(
                navigations.get(),
                0,
                "failed active replay must not navigate"
            );
            // The source error is preserved for a typed/redacted outer
            // worker, but this helper never formats or logs its Display.
            assert!(error.to_string().contains("private-token"));
        }
    }

    #[tokio::test]
    async fn successful_cookie_restore_preserves_one_navigation_and_error_order() {
        for provider in [Provider::ChatGPT, Provider::MetaAI, Provider::Grok] {
            let navigations = std::cell::Cell::new(0_u8);
            navigate_after_cookie_restore(provider, Ok(()), || async {
                navigations.set(navigations.get() + 1);
                Ok(())
            })
            .await
            .unwrap();
            assert_eq!(navigations.get(), 1);
        }
        // Deferred providers' old origin-localStorage cleanup flow still
        // navigates once before returning their cookie restore error.
        for provider in [Provider::Grok, Provider::Claude] {
            let navigations = std::cell::Cell::new(0_u8);
            let error = navigate_after_cookie_restore(
                provider,
                Err(anyhow::anyhow!("synthetic-invalid-cookie")),
                || async {
                    navigations.set(navigations.get() + 1);
                    Ok(())
                },
            )
            .await
            .unwrap_err();
            assert_eq!(navigations.get(), 1);
            assert!(error.to_string().contains("synthetic-invalid-cookie"));
        }
    }

    #[tokio::test]
    async fn navigation_error_is_not_suppressed_after_successful_restore() {
        let navigations = std::cell::Cell::new(0_u8);
        let err = navigate_after_cookie_restore(Provider::ChatGPT, Ok(()), || async {
            navigations.set(navigations.get() + 1);
            Err(anyhow::anyhow!("synthetic-navigation-failed"))
        })
        .await
        .unwrap_err();
        assert_eq!(navigations.get(), 1);
        assert_eq!(err.to_string(), "synthetic-navigation-failed");
    }
}

#[cfg(test)]
mod live_replay_guard_tests {
    use super::*;

    #[test]
    fn changed_active_chatgpt_and_metaai_snapshots_are_deferred_before_mutation() {
        for provider in [Provider::ChatGPT, Provider::MetaAI] {
            let error = guard_live_replay(
                provider,
                Some("previous-private-hash"),
                "rotated-hash",
                true,
            )
            .unwrap_err();
            assert!(error
                .downcast_ref::<ReplayRequiresIsolatedContext>()
                .is_some());
            assert_eq!(
                error.to_string(),
                "replay_deferred_requires_isolated_context"
            );
            for secret in ["previous-private-hash", "rotated-hash", "cookie", "token"] {
                assert!(!error.to_string().contains(secret));
            }
        }
    }

    #[test]
    fn verified_live_page_without_fingerprint_defers_before_cookie_mutation() {
        let registry = crate::provider_state::ProviderSessionRegistry::default();
        let epoch = registry.begin_credential_verification("chatgpt").unwrap();
        assert!(registry.record_credentials_verified("chatgpt", epoch));
        let mut mutation_called = false;

        let result = guard_verified_live_replay(
            Provider::ChatGPT,
            None,
            "new-private-fingerprint",
            true,
            registry.credentials_verified("chatgpt"),
        );
        if result.is_ok() {
            mutation_called = true;
        }
        let err = result.unwrap_err();
        assert!(err
            .downcast_ref::<ReplayRequiresIsolatedContext>()
            .is_some());
        assert_eq!(err.to_string(), "replay_deferred_requires_isolated_context");
        assert!(!mutation_called, "shared jar must not be touched");
        assert!(
            registry.credentials_verified("chatgpt"),
            "source proof unchanged"
        );

        // The same safety rule will apply when Meta AI has a source-backed
        // account verifier; it currently must not infer a positive from cookies.
        assert!(guard_verified_live_replay(Provider::MetaAI, None, "new", true, true).is_err());
        // Unverified first import may still proceed without forced resync.
        assert!(guard_verified_live_replay(Provider::ChatGPT, None, "first", true, false).is_ok());
        assert!(guard_verified_live_replay(Provider::MetaAI, None, "first", true, false).is_ok());
        // No live page means there is no prior page to preserve.
        assert!(guard_verified_live_replay(Provider::ChatGPT, None, "first", false, true).is_ok());
        // A known identical applied snapshot can be reverified read-only.
        assert!(
            guard_verified_live_replay(Provider::ChatGPT, Some("same"), "same", true, true).is_ok()
        );
        assert!(
            guard_verified_live_replay(Provider::ChatGPT, Some("old"), "new", true, false).is_err()
        );
        // Explicitly deferred providers keep their own replay policy.
        assert!(guard_verified_live_replay(Provider::Grok, None, "new", true, true).is_ok());
    }

    #[test]
    fn historical_positive_is_safety_only_not_a_fresh_login_verdict() {
        // The guard only inspects a prior source-backed positive, not its
        // freshness. The registry's public credentials_verified() still
        // applies the five-minute TTL independently.
        let reg = crate::provider_state::ProviderSessionRegistry::default();
        let epoch = reg.begin_credential_verification("chatgpt").unwrap();
        assert!(reg.record_credentials_verified("chatgpt", epoch));
        assert!(guard_verified_live_replay(
            Provider::ChatGPT,
            None,
            "fresh-candidate",
            true,
            reg.has_prior_positive_account_observation("chatgpt"),
        )
        .is_err());
        reg.invalidate_credentials("chatgpt");
        assert!(guard_verified_live_replay(
            Provider::ChatGPT,
            None,
            "fresh-candidate",
            true,
            reg.has_prior_positive_account_observation("chatgpt"),
        )
        .is_ok());
    }

    #[test]
    fn failed_recheck_cannot_reenable_destructive_markerless_replay() {
        let registry = crate::provider_state::ProviderSessionRegistry::default();
        let first = registry.begin_credential_verification("chatgpt").unwrap();
        assert!(registry.record_credentials_verified("chatgpt", first));

        // A new request rotates public account proof, not the evidence that
        // the same live cookie jar must survive a transient failed recheck.
        let second = registry.begin_credential_verification("chatgpt").unwrap();
        assert!(!registry.credentials_verified("chatgpt"));
        assert!(!registry.record_credentials_verified("chatgpt", first));
        let safety = registry.has_prior_positive_account_observation("chatgpt");
        assert!(safety);
        let mut destructive_write = false;
        let verdict = guard_verified_live_replay(
            Provider::ChatGPT,
            None,
            "candidate-private-fingerprint",
            true,
            safety,
        );
        if verdict.is_ok() {
            destructive_write = true;
        }
        assert!(verdict
            .unwrap_err()
            .downcast_ref::<ReplayRequiresIsolatedContext>()
            .is_some());
        assert!(
            !destructive_write,
            "recheck did not authorize shared-jar writes"
        );

        // Current proof must be established from the *new* epoch only.
        assert!(registry.record_credentials_verified("chatgpt", second));
        assert!(registry.credentials_verified("chatgpt"));
        registry.record_needs_login("chatgpt");
        assert!(!registry.has_prior_positive_account_observation("chatgpt"));
        assert!(guard_verified_live_replay(
            Provider::ChatGPT,
            None,
            "candidate",
            true,
            registry.has_prior_positive_account_observation("chatgpt"),
        )
        .is_ok());
    }

    #[test]
    fn no_live_page_no_prior_applied_snapshot_and_identical_replay_remain_allowed() {
        for provider in [Provider::ChatGPT, Provider::MetaAI] {
            assert!(guard_live_replay(provider, None, "first", true).is_ok());
            assert!(guard_live_replay(provider, Some("old"), "changed", false).is_ok());
            assert!(guard_live_replay(provider, Some("same"), "same", true).is_ok());
        }
        // The other provider recoveries are explicitly deferred by owner.
        assert!(guard_live_replay(Provider::Grok, Some("old"), "changed", true).is_ok());
    }

    #[test]
    fn only_reused_active_provider_page_retains_applied_fingerprint_after_send() {
        assert!(retain_applied_after_send(Provider::ChatGPT, false));
        assert!(retain_applied_after_send(Provider::MetaAI, false));
        assert!(!retain_applied_after_send(Provider::ChatGPT, true));
        assert!(!retain_applied_after_send(Provider::MetaAI, true));
        assert!(!retain_applied_after_send(Provider::Grok, false));
        assert!(!retain_applied_after_send(Provider::Grok, true));
    }

    #[test]
    fn rejected_candidate_keeps_existing_verified_epoch_until_worker_admits_replay() {
        let registry = crate::provider_state::ProviderSessionRegistry::default();
        let previous_epoch = registry.begin_credential_verification("chatgpt").unwrap();
        assert!(registry.record_credentials_verified("chatgpt", previous_epoch));
        assert!(registry.credentials_verified("chatgpt"));

        // The HTTP import queue/persistence stage must not invalidate this
        // proof: it refers to the still-active page, not the new candidate.
        assert!(guard_live_replay(
            Provider::ChatGPT,
            Some("active-fingerprint"),
            "changed-fingerprint",
            true,
        )
        .is_err());
        assert!(registry.credentials_verified("chatgpt"));

        // After a permitted, actual worker verification begins, only its new
        // opaque epoch can publish an updated first-party positive result.
        assert!(guard_live_replay(
            Provider::ChatGPT,
            Some("active-fingerprint"),
            "active-fingerprint",
            true,
        )
        .is_ok());
        let next_epoch = registry.begin_credential_verification("chatgpt").unwrap();
        assert!(!registry.credentials_verified("chatgpt"));
        assert!(!registry.record_credentials_verified("chatgpt", previous_epoch));
        assert!(registry.record_credentials_verified("chatgpt", next_epoch));
        assert!(registry.credentials_verified("chatgpt"));

        // User unlink/reset must still revoke even a previously verified page.
        registry.invalidate_credentials("chatgpt");
        assert!(!registry.credentials_verified("chatgpt"));
    }

    #[tokio::test]
    async fn replay_guard_blocks_destructive_activation_before_callback() {
        let applied = std::cell::RefCell::new(std::collections::HashMap::new());
        applied
            .borrow_mut()
            .insert(Provider::ChatGPT, "known".to_owned());
        let mut attempted_mutations = 0_usize;
        let guard = guard_live_replay(
            Provider::ChatGPT,
            applied.borrow().get(&Provider::ChatGPT).map(String::as_str),
            "candidate",
            true,
        );
        if guard.is_ok() {
            attempted_mutations += 1;
        }
        assert!(guard.is_err());
        assert_eq!(attempted_mutations, 0);
        assert_eq!(
            applied.borrow().get(&Provider::ChatGPT).map(String::as_str),
            Some("known")
        );
    }
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
        let failure = replace_after_success(
            || async { Err::<String, _>(anyhow::anyhow!("synthetic cookie restore failure")) },
            |replacement| {
                pages.borrow_mut().insert(Provider::Grok, replacement);
            },
        )
        .await;
        assert!(failure.is_err());
        assert_eq!(
            pages.borrow().get(&Provider::Grok).map(String::as_str),
            Some("warm-page")
        );
        replace_after_success(
            || async { Ok(String::from("restored-page")) },
            |replacement| {
                pages.borrow_mut().insert(Provider::Grok, replacement);
            },
        )
        .await
        .unwrap();
        assert_eq!(
            pages.borrow().get(&Provider::Grok).map(String::as_str),
            Some("restored-page")
        );
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

#[cfg(test)]
mod warmup_timeout_tests {
    use super::{bounded_warmup, replace_after_success, SESSION_WARMUP_TIMEOUT};
    use std::{cell::RefCell, future::pending, time::Duration};

    #[test]
    fn session_warmup_has_bounded_window() {
        assert_eq!(SESSION_WARMUP_TIMEOUT, Duration::from_secs(30));
    }

    #[tokio::test]
    async fn unresponsive_navigation_is_bounded_and_returns_fixed_redacted_error() {
        let result = bounded_warmup(Duration::from_millis(1), async {
            pending::<anyhow::Result<()>>().await
        })
        .await;
        let error = result.unwrap_err().to_string();
        assert_eq!(error, "provider session navigation timed out");
        assert!(!error.contains("cookie"));
        assert!(!error.contains("token"));
    }

    #[tokio::test]
    async fn successful_warmup_is_not_treated_as_timeout() {
        let result = bounded_warmup(Duration::from_millis(50), async { Ok(()) }).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn provider_error_is_not_swallowed_or_reclassified_as_timeout() {
        let result = bounded_warmup(Duration::from_millis(50), async {
            anyhow::bail!("synthetic provider error")
        })
        .await;
        assert_eq!(result.unwrap_err().to_string(), "synthetic provider error");
    }

    #[tokio::test]
    async fn timed_out_warmup_does_not_replace_prior_page_or_cache_a_fingerprint() {
        let warm = RefCell::new(String::from("prior-valid-page"));
        let result = replace_after_success(
            || async {
                bounded_warmup(Duration::from_millis(1), async {
                    pending::<anyhow::Result<()>>().await
                })
                .await?;
                Ok(String::from("new-page"))
            },
            |replacement| *warm.borrow_mut() = replacement,
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "provider session navigation timed out"
        );
        assert_eq!(warm.borrow().as_str(), "prior-valid-page");
        replace_after_success(
            || async { Ok(String::from("retry-success")) },
            |replacement| *warm.borrow_mut() = replacement,
        )
        .await
        .unwrap();
        assert_eq!(warm.borrow().as_str(), "retry-success");
    }
}

#[cfg(test)]
mod chatgpt_stream_preload_tests {
    use std::time::Duration;

    fn encoded_data_page(html: &str) -> String {
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        format!("data:text/html,{encoded}")
    }

    #[tokio::test]
    async fn first_party_guard_does_not_modify_unrelated_embedded_pages() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.add_preload_script(include_str!("chatgpt_streams.js"));
        page.goto(&encoded_data_page(
            r#"<html><body><script>
            globalThis.__wasPatched = ReadableStream.__chatGatewayPullCompat === true;
            </script></body></html>"#,
        ))
        .await
        .unwrap();
        assert_eq!(
            page.evaluate("globalThis.__wasPatched").as_bool(),
            Some(false)
        );
    }

    #[tokio::test]
    async fn pinned_obscura_preload_runs_before_author_scripts_and_unblocks_pull() {
        // The *only* host bypass lives in this in-memory data: fixture.
        // Production always evaluates the exact HTTPS ChatGPT origin guard.
        let original = include_str!("chatgpt_streams.js");
        let origin_guard = "  if (typeof location === 'undefined' || location.protocol !== 'https:' ||\n      !['chatgpt.com', 'www.chatgpt.com'].includes(location.hostname) ||\n      !['', '443'].includes(String(location.port || ''))) return;";
        assert!(original.contains(origin_guard));
        let fixture_script = original.replacen(origin_guard, "", 1);
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.add_preload_script(&fixture_script);
        page.goto(&encoded_data_page(
            r#"<html><body><script>
            globalThis.__wasPatched = ReadableStream.__chatGatewayPullCompat === true;
            const stream = new ReadableStream({
                pull(controller) {
                    globalThis.__pulls = (globalThis.__pulls || 0) + 1;
                    controller.enqueue('pinned-compat-answer');
                    controller.close();
                }
            });
            stream.getReader().read().then(result => {
                globalThis.__streamAnswer = result.value;
            }).catch(() => {
                globalThis.__streamAnswer = 'failed';
            });
            </script></body></html>"#,
        ))
        .await
        .unwrap();
        crate::providers::common::drive_page(&mut page, Duration::from_millis(60)).await;
        assert_eq!(
            page.evaluate("globalThis.__wasPatched").as_bool(),
            Some(true)
        );
        assert_eq!(
            page.evaluate("globalThis.__streamAnswer").as_str(),
            Some("pinned-compat-answer")
        );
        assert_eq!(page.evaluate("globalThis.__pulls").as_f64(), Some(1.0));
    }
}
