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

/// The pinned Obscura public Browser API shares one cookie jar across all
/// pages. Until an isolated staging context exists, protect a *known applied*
/// ChatGPT/Meta AI page against replacement with a different imported jar.
/// This guard must run under the worker operation/provider locks and BEFORE
/// invalidating an existing first-party proof or mutating browser cookies.
/// A cold page or an identical snapshot can still be verified without resync.
/// A normal send can reuse the same authenticated page and cookie jar.
/// Keep the known applied marker for active recovery providers in that case,
/// so the next relay import remains protected. A newly created page cannot
/// inherit the previous page's applied-state assertion.
pub(super) fn retain_applied_after_send(provider: Provider, newly_opened: bool) -> bool {
    !newly_opened && matches!(provider, Provider::ChatGPT | Provider::MetaAI)
}

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

async fn bounded_warmup<F>(timeout: Duration, warmup: F) -> Result<()>
where
    F: std::future::Future<Output = Result<()>>,
{
    tokio::time::timeout(timeout, warmup)
        .await
        .context("provider session navigation timed out")?
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
mod live_replay_guard_tests {
    use super::*;

    #[test]
    fn changed_active_chatgpt_and_metaai_snapshots_are_deferred_before_mutation() {
        for provider in [Provider::ChatGPT, Provider::MetaAI] {
            let error = guard_live_replay(provider, Some("previous-private-hash"), "rotated-hash", true)
                .unwrap_err();
            assert!(error.downcast_ref::<ReplayRequiresIsolatedContext>().is_some());
            assert_eq!(error.to_string(), "replay_deferred_requires_isolated_context");
            for secret in ["previous-private-hash", "rotated-hash", "cookie", "token"] {
                assert!(!error.to_string().contains(secret));
            }
        }
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

    #[tokio::test]
    async fn replay_guard_blocks_destructive_activation_before_callback() {
        let applied = std::cell::RefCell::new(std::collections::HashMap::new());
        applied.borrow_mut().insert(Provider::ChatGPT, "known".to_owned());
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
        assert_eq!(applied.borrow().get(&Provider::ChatGPT).map(String::as_str), Some("known"));
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
