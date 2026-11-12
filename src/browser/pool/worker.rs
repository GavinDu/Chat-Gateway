//! Thread-affine Obscura browser worker runtime and command loop.
//!
//! The worker stays on its dedicated current-thread runtime because the
//! embedded V8-backed Page values are not Send. HTTP-side ownership and
//! command delivery remain in the parent pool module.
use super::*;

/// Session restore/save failures may contain upstream URLs, vault paths, or
/// credential-bearing text. Only stable categories cross the log boundary;
/// the surrounding fixed log message identifies the failing operation.
fn session_worker_failure_category(error: &anyhow::Error) -> &'static str {
    if error
        .downcast_ref::<BrowserWorkerUnavailableError>()
        .is_some()
    {
        "browser_worker_unavailable"
    } else {
        "session_operation_failed"
    }
}

#[cfg(test)]
mod session_worker_error_redaction_tests {
    use super::*;

    #[test]
    fn untrusted_session_error_diagnostics_never_enter_log_categories() {
        let secret = anyhow::anyhow!("cookie=very-secret; bearer=private; path=/vault/token");
        assert_eq!(
            session_worker_failure_category(&secret),
            "session_operation_failed"
        );
        let worker = anyhow::Error::new(BrowserWorkerUnavailableError);
        assert_eq!(
            session_worker_failure_category(&worker),
            "browser_worker_unavailable"
        );
        assert!(!session_worker_failure_category(&secret).contains("secret"));
    }
}

/// The browser worker waits for a composer *before* invoking the provider
/// adapter. A timeout here otherwise bypasses adapter-level diagnostics.
/// Never record the selector, URL, prompt, page text or upstream error string.
fn report_composer_preflight_failure(
    provider: &str,
    page: &obscura_core::Page,
    error: anyhow::Error,
) -> anyhow::Error {
    tracing::warn!(
        provider,
        stage = "composer_preflight",
        outcome = "unavailable",
        "provider composer diagnostic"
    );
    if provider == "mistral" {
        let diagnostics = providers::mistral::mistral_readiness_diagnostics(page);
        tracing::warn!(
            provider = "mistral",
            stage = "composer_preflight",
            diagnostics = %diagnostics,
            "Mistral composer readiness failed"
        );
        // Only observed first-party script HTTP 403 evidence can justify the
        // typed asset-blocked error. DOM faults and missing Resource Timing
        // status are not proof of a CDN or authentication policy denial.
        if diagnostics.contains("blocker=script_http_403") {
            return anyhow::Error::new(providers::mistral::MistralAssetBlockedError);
        }
    }
    error
}

#[cfg(test)]
mod composer_preflight_diagnostic_tests {
    use super::*;

    #[tokio::test]
    async fn missing_mistral_composer_retains_original_failure_without_403_evidence() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,%3Chtml%3E%3Cbody%3Eno%20composer%3C/body%3E%3C/html%3E")
            .await
            .unwrap();
        let original = anyhow::anyhow!("composer missing");
        let reported = report_composer_preflight_failure("mistral", &page, original);
        assert_eq!(reported.to_string(), "composer missing");
        assert!(reported
            .downcast_ref::<providers::mistral::MistralAssetBlockedError>()
            .is_none());
    }

    #[tokio::test]
    async fn missing_chatgpt_composer_retains_original_failure() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,%3Chtml%3E%3Cbody%3E%3C/body%3E%3C/html%3E")
            .await
            .unwrap();
        let reported = report_composer_preflight_failure(
            "chatgpt",
            &page,
            anyhow::anyhow!("composer unavailable"),
        );
        assert_eq!(reported.to_string(), "composer unavailable");
    }
}

/// Cancel an in-flight provider operation as soon as its downstream receiver
/// disappears. The biased order checks disconnect first when both branches
/// are ready, avoiding even an immediately-ready stale navigation/model step.
async fn send_unless_disconnected<T>(
    chunks: &mpsc::Sender<Result<ProviderChunk>>,
    operation: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = chunks.closed() => None,
        output = operation => Some(output),
    }
}

/// A page lock may also be held by a status probe or login operation that
/// does not use the global operation gate. Bound this second queue separately:
/// one stuck provider must not leave an admitted send waiting forever.
async fn acquire_page_lock<'a>(
    lock: &'a tokio::sync::Mutex<()>,
    deadline: tokio::time::Instant,
) -> Result<tokio::sync::MutexGuard<'a, ()>> {
    tokio::time::timeout_at(deadline, lock.lock())
        .await
        .map_err(|_| anyhow::Error::new(ProviderBusyError))
}

/// Abandoned refreshes must not wait behind a send and mutate its session later.
/// Once activation starts, finish it atomically with respect to this lock.
async fn session_refresh_permit<'a>(
    reply: &mut oneshot::Sender<Result<()>>,
    lock: &'a tokio::sync::Mutex<()>,
) -> Option<tokio::sync::MutexGuard<'a, ()>> {
    tokio::select! {
        biased;
        _ = reply.closed() => None,
        guard = lock.lock() => Some(guard),
    }
}

/// Adapter diagnostics must use the same actionable input as the send path:
/// hidden autosizers and disabled composers cannot receive a real prompt.
fn diagnostic_input_selector<'a>(
    page: &obscura_core::Page,
    selectors: &[&'a str],
) -> Option<&'a str> {
    selectors
        .iter()
        .copied()
        .find(|selector| providers::common::visible_composer_exists(page, &[*selector]))
}

/// Probe the visible editor, never the first hidden duplicate DOM match.
/// The test string stays inside the page and is not included in diagnostics.
fn diagnostic_prompt_retained(page: &obscura_core::Page, selector: &str, prompt: &str) -> bool {
    let (Ok(selector), Ok(prompt)) = (
        serde_json::to_string(selector),
        serde_json::to_string(prompt),
    ) else {
        return false;
    };
    let script = format!(
        r#"(() => {{
            const el = Array.from(document.querySelectorAll({selector})).find(el =>
                el.getClientRects().length > 0 &&
                window.getComputedStyle(el).visibility !== 'hidden' &&
                !el.closest('[hidden], [inert], [aria-hidden="true"], [aria-disabled="true"]') &&
                !el.disabled && !el.matches(':disabled') &&
                !el.closest('fieldset[disabled]') && !el.readOnly &&
                el.getAttribute('aria-disabled') !== 'true' &&
                el.getAttribute('aria-readonly') !== 'true' &&
                el.getAttribute('contenteditable') !== 'false'
            );
            if (!el) return false;
            const value = 'value' in el ? el.value : (el.innerText || el.textContent || '');
            return typeof value === 'string' && value.trim() === {prompt}.trim();
        }})()"#
    );
    page.evaluate(&script).as_bool() == Some(true)
}

#[cfg(test)]
mod adapter_diagnostic_composer_tests {
    #[tokio::test]
    async fn hidden_and_disabled_diagnostic_inputs_cannot_override_visible_editor() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <textarea id="hidden" class="composer" style="display:none">stale</textarea>
            <textarea id="disabled" class="composer" disabled>stale</textarea>
            <fieldset disabled><textarea id="inherited" class="composer">stale</textarea></fieldset>
            <div aria-disabled="true"><textarea id="aria-inherited" class="composer">diagnostic draft</textarea></div>
            <div id="noneditable" class="composer" contenteditable="false">stale</div>
            <div id="aria-readonly" class="composer" contenteditable="true" aria-readonly="true">stale</div>
            <textarea id="ready" class="composer">diagnostic draft</textarea>
            </body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        assert_eq!(
            super::diagnostic_input_selector(
                &page,
                &[
                    "#hidden",
                    "#disabled",
                    "#inherited",
                    "#aria-inherited",
                    "#noneditable",
                    "#aria-readonly",
                    "#ready"
                ]
            ),
            Some("#ready")
        );
        assert!(super::diagnostic_prompt_retained(
            &page,
            ".composer",
            "diagnostic draft"
        ));
        page.evaluate("document.querySelector('#ready').value = ''");
        assert!(!super::diagnostic_prompt_retained(
            &page,
            ".composer",
            "diagnostic draft"
        ));
        assert_eq!(
            super::diagnostic_input_selector(&page, &["#aria-inherited"]),
            None,
            "diagnostics must not report a disabled wrapper as composer ready"
        );
        page.evaluate("document.querySelector('[aria-disabled]').removeAttribute('aria-disabled')");
        assert_eq!(
            super::diagnostic_input_selector(&page, &["#aria-inherited"]),
            Some("#aria-inherited")
        );
        assert!(super::diagnostic_prompt_retained(
            &page,
            "#aria-inherited",
            "diagnostic draft"
        ));
    }
}

#[cfg(test)]
mod adapter_diagnostic_location_tests {
    #[test]
    fn page_location_diagnostics_strip_query_fragment_and_userinfo() {
        assert_eq!(
            super::sanitize_page_location("https://chatgpt.com/c/abc?prompt=secret#frag"),
            "https://chatgpt.com/c/abc"
        );
        assert_eq!(
            super::sanitize_page_location("https://user:pass@chatgpt.com/c/abc"),
            "https://chatgpt.com/c/abc"
        );
        assert_eq!(
            super::sanitize_page_location("https://user:pass@chatgpt.com"),
            "https://chatgpt.com"
        );
        assert_eq!(super::sanitize_page_location("about:blank"), "about:blank");
        assert!(
            super::sanitize_page_location(&format!("https://chatgpt.com/{}", "a".repeat(400)))
                .chars()
                .count()
                <= 256
        );
    }
}

/// Provider pages can put prompts, thread ids or account tokens in the URL
/// query or fragment. Keep only a bounded scheme/host/path prefix.
fn sanitize_page_location(url: &str) -> String {
    let base = url.split(['?', '#']).next().unwrap_or(url);
    let without_userinfo = match base.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('/') {
            Some((authority, path)) if authority.contains('@') => {
                let host = authority
                    .rsplit_once('@')
                    .map_or(authority, |(_, host)| host);
                format!("{scheme}://{host}/{path}")
            }
            None if rest.contains('@') => {
                let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
                format!("{scheme}://{host}")
            }
            _ => base.to_string(),
        },
        None => base.to_string(),
    };
    without_userinfo.chars().take(256).collect()
}

/// Fixed selector names only: never log arbitrary field attributes or values.
fn diagnostic_input_controls(page: &obscura_core::Page) -> Vec<String> {
    [
        ("ChatGPT desktop", "#prompt-textarea"),
        (
            "ChatGPT mobile",
            "textarea#mobile-composer-prompt[name=\"prompt\"]",
        ),
        (
            "Ask Grok anything aria",
            "textarea[aria-label=\"Ask Grok anything\"]",
        ),
        ("Ask anything aria", "textarea[aria-label=\"Ask anything\"]"),
        ("Ask Grok aria", "textarea[aria-label=\"Ask Grok\"]"),
        ("Message Grok aria", "textarea[aria-label=\"Message Grok\"]"),
        ("Ask placeholder", "textarea[placeholder*=\"Ask\"]"),
        ("Message placeholder", "textarea[placeholder*=\"Message\"]"),
        ("labelled textarea", "textarea[aria-label]"),
        ("any textarea", "textarea"),
        (
            "editable textbox",
            "[contenteditable=\"true\"][role=\"textbox\"]",
        ),
        ("any editable", "[contenteditable]"),
    ]
    .into_iter()
    .filter(|(_, selector)| page.query_selector(selector).is_some())
    .map(|(name, _)| name.to_owned())
    .collect()
}

async fn ensure_provider_page(
    browser: &obscura_core::Browser,
    pages: &SharedPages,
    provider: Provider,
) -> Result<bool> {
    if pages.borrow().contains_key(&provider) {
        return Ok(false);
    }
    let mut page = browser_factory::new_page(browser).await?;
    crate::browser::network_tap::attach_provider_network_tap(&mut page, provider);
    let snapshot = session::load_session_file(provider.name())?;
    if let Err(error) = PageSession::open(browser, &mut page, provider, snapshot.as_ref()).await {
        session::clear_provider_cookies(browser, provider.name())?;
        return Err(error);
    }
    pages.borrow_mut().insert(provider, Rc::new(page));
    Ok(true)
}

pub(super) fn browser_thread(
    cfg: Arc<AppConfig>,
    mut cmd_rx: mpsc::Receiver<Command>,
    ready_tx: oneshot::Sender<Result<()>>,
    worker_ready: Arc<AtomicBool>,
    provider_sessions: ProviderSessionRegistry,
    metrics: MetricsRegistry,
    session_write_tracker: SessionWriteTracker,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let _ready_guard = WorkerReadyGuard(worker_ready.clone());
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ready_tx.send(Err(e.into()));
                return;
            }
        };
        rt.block_on(async move {
            let local = tokio::task::LocalSet::new();
            local.run_until(async move {
                let gateway_poll_tick = Duration::from_millis(cfg.server.gateway_poll_tick_ms);
                let browser = match build_browser(&cfg) {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let browser = Rc::new(browser);
                let registry = Rc::new(providers::build_registry(&cfg));
                // Warm pages + per-provider locks live in shared RefCells
                // so login commands (which run in spawn_local tasks) can
                // read them and Complete can swap a login page in as the
                // warm page. Every borrow is short (no await while a
                // borrow is held) — map entries are cloned out first.
                let pages = Rc::new(RefCell::new(ProviderPageCache::new(cfg.obscura.max_cached_provider_pages)));
                // Serializing browser-driving work bounds transient pages and V8
                // heaps while other HTTP/status work remains independently runnable.
                let operation_gate = Rc::new(tokio::sync::Mutex::new(()));
                let applied_imports: Rc<RefCell<HashMap<Provider, String>>> = Rc::new(RefCell::new(HashMap::new()));
                let locks: Rc<RefCell<HashMap<Provider, Rc<tokio::sync::Mutex<()>>>>> =
                    Rc::new(RefCell::new(HashMap::new()));
                // Login delegate UI state (see src/login.rs).
                let login_pages: Rc<RefCell<HashMap<Provider, Rc<obscura_core::Page>>>> =
                    Rc::new(RefCell::new(HashMap::new()));
                let login_meta: Rc<RefCell<HashMap<Provider, LoginMeta>>> =
                    Rc::new(RefCell::new(HashMap::new()));

                // Every enabled provider has a lock even when its page is cold.
                for pcfg in cfg.providers.iter().filter(|pcfg| pcfg.enabled) {
                    if let Some(provider) = Provider::from_name(&pcfg.name) {
                        locks.borrow_mut().insert(provider, Rc::new(tokio::sync::Mutex::new(())));
                    }
                }
                // Keep only a bounded working set; other providers open on demand.
                for pcfg in cfg.providers.iter().filter(|pcfg| pcfg.enabled).take(cfg.obscura.max_cached_provider_pages) {
                    let Some(provider) = Provider::from_name(&pcfg.name) else { continue };
                    match super::browser_factory::new_page(&browser).await {
                        Ok(mut page) => {
                            crate::browser::network_tap::attach_provider_network_tap(&mut page, provider);
                            let session_file = match session::load_session_file(&pcfg.name) {
                                Ok(file) => file,
                                Err(error) => {
                                    // Preserve unreadable/corrupt vault material for diagnosis.
                                    // The page starts without restored credentials and must
                                    // pass the same interactive-login health check as a fresh page.
                                    tracing::warn!(
                                        provider = %pcfg.name,
                                        category = session_worker_failure_category(&error),
                                        "provider session snapshot could not be loaded; interactive login may be required"
                                    );
                                    None
                                }
                            };
                            let restore_failed = match PageSession::open(
                                &browser,
                                &mut page,
                                provider,
                                session_file.as_ref(),
                            )
                            .await
                            {
                                Ok(()) => false,
                                Err(error) => {
                                    tracing::warn!(
                                        provider = %pcfg.name,
                                        category = session_worker_failure_category(&error),
                                        "provider page session warm-up failed"
                                    );
                                    true
                                }
                            };
                            if restore_failed {
                                // Do not publish a partially restored page. Clear runtime
                                // state, retaining the encrypted snapshot: a load/transport
                                // failure does not establish expired credentials.
                                let cleanup = (|| -> Result<()> {
                                    session::clear_provider_cookies(&browser, provider.name())?;
                                    let cleared = page.evaluate(
                                        "(() => { try { localStorage.clear(); return true; } catch (_) { return false; } })()"
                                    );
                                    if cleared != serde_json::Value::Bool(true) {
                                        anyhow::bail!("provider localStorage cleanup failed");
                                    }
                                    Ok(())
                                })();
                                if let Err(error) = cleanup {
                                    provider_sessions.record_probe_failure(provider.name());
                                    tracing::error!(
                                        provider = %pcfg.name,
                                        category = session_worker_failure_category(&error),
                                        "could not clear a partially restored provider session; page withheld"
                                    );
                                    continue;
                                }
                                provider_sessions.record_probe_failure(provider.name());
                                tracing::warn!(provider = %pcfg.name,
                                    "provider warm-up failed; page withheld and saved session retained");
                                continue;
                            } else {
                                // Verify the restored session before publishing this page as
                                // usable. Expired credentials are revoked from disk and from
                                // the embedded browser before interactive login is requested.
                                let verification = match registry.get(&provider) {
                                    Some(adapter) => verify_session_page(provider, &page, adapter.as_ref()),
                                    None => Err(anyhow::anyhow!("provider adapter is not registered")),
                                };
                                match verification {
                                    Ok(true) => provider_sessions.record_success(provider.name()),
                                    // No chat input is inconclusive, not evidence of expiry.
                                    Ok(false) => {}
                                    Err(error) if error.downcast_ref::<ProviderSessionExpiredError>().is_some() => {
                                        let cleanup = (|| -> Result<()> {
                                            session::clear_provider_cookies(&browser, provider.name())?;
                                            let cleared = page.evaluate(
                                                "(() => { try { localStorage.clear(); return true; } catch (_) { return false; } })()"
                                            );
                                            if cleared != serde_json::Value::Bool(true) {
                                                anyhow::bail!("provider localStorage cleanup failed");
                                            }
                                            session::delete_session_snapshot(provider.name())?;
                                            Ok(())
                                        })();
                                        if let Err(cleanup_error) = cleanup {
                                            provider_sessions.record_probe_failure(provider.name());
                                            tracing::error!(
                                                provider = %pcfg.name,
                                                %cleanup_error,
                                                "expired provider session cleanup failed; page withheld"
                                            );
                                            continue;
                                        }
                                        if let Err(navigation_error) = page.goto(provider.login_url()).await {
                                            provider_sessions.record_probe_failure(provider.name());
                                            tracing::error!(
                                                provider = %pcfg.name,
                                                %navigation_error,
                                                "provider navigation after expired-session cleanup failed; page withheld"
                                            );
                                            continue;
                                        }
                                        provider_sessions.record_needs_login(provider.name());
                                        tracing::warn!(
                                            provider = %pcfg.name,
                                            "expired provider session was invalidated; interactive login required"
                                        );
                                    }
                                    Err(error) => {
                                        provider_sessions.record_probe_failure(provider.name());
                                        tracing::warn!(
                                            provider = %pcfg.name,
                                            category = session_worker_failure_category(&error),
                                            "provider session could not be verified during warm-up"
                                        );
                                    }
                                }
                            }
                            pages.borrow_mut().insert(provider, Rc::new(page));
                            locks.borrow_mut().insert(provider, Rc::new(tokio::sync::Mutex::new(())));
                            tracing::info!(provider = %pcfg.name, url = provider.login_url(), "page warmed");
                        }
                        Err(e) => tracing::warn!(provider = %pcfg.name, "warm failed: {e}"),
                    }
                }
                tracing::info!("ObscuraPool warmed: {} pages", pages.borrow().len());
                worker_ready.store(true, Ordering::Release);
                let _ = ready_tx.send(Ok(()));

                // app::start_background_workers probes the current cache entry
                // under its provider lock. Do not retain startup pages in a second
                // periodic task after refresh has replaced or evicted them.

                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        Command::Shutdown { reply } => {
                            let _ = reply.send(());
                            break;
                        }
                        Command::Discover { provider, reply } => {
                            let browser = browser.clone();
                            let registry = registry.clone();
                            let operation_gate = operation_gate.clone();
                            tokio::task::spawn_local(async move {
                                let _operation = operation_gate.lock().await;
                                let res: Result<Vec<String>> = async {
                                    let adapter = registry.get(&provider).cloned()
                                        .ok_or_else(|| anyhow::anyhow!("provider not enabled"))?;
                                    let mut page = super::browser_factory::new_page(&browser).await?;
                                    adapter.discover_models(&mut page).await
                                }.await;
                                let _ = reply.send(res);
                            });
                        }
                        Command::Send { provider, prompt, model, has_history, thread_url, timeout, completion_idle, provider_guard, chunks } => {
                            // A request can be cancelled while waiting in the
                            // worker command queue. Never navigate a page for
                            // an abandoned sender after that admission timeout.
                            if chunks.is_closed() { continue; }
                            let pages = pages.clone();
                            let operation_gate = operation_gate.clone();
                            let applied_imports = applied_imports.clone();
                            let Some(lock) = locks.borrow().get(&provider).cloned() else { continue };
                            let Some(adapter) = registry.get(&provider).cloned() else {
                                let _ = chunks.send(Err(anyhow::anyhow!("provider not enabled"))).await;
                                continue;
                            };
                            let operation_queue_timeout = Duration::from_secs(
                                cfg.provider(provider.name()).map(|config| config.queue_timeout_seconds).unwrap_or(30));
                            let browser_for_save = browser.clone();
                            let provider_name = provider.name().to_string();
                            let poll_tick = gateway_poll_tick;
                            let request_metrics = metrics.clone();
                            let send_session_write_tracker = session_write_tracker.clone();
                            tokio::task::spawn_local(async move {
                                // Both operation admission and the provider page lock
                                // consume one queue budget, rather than each waiting for
                                // the full configured timeout independently.
                                let queue_deadline =
                                    tokio::time::Instant::now() + operation_queue_timeout;
                                let Some(operation) = send_unless_disconnected(&chunks,
                                    tokio::time::timeout_at(queue_deadline, operation_gate.lock())).await else { return; };
                                let _operation = match operation {
                                    Ok(guard) => guard,
                                    Err(_) => { let _ = chunks.send(Err(anyhow::Error::new(ProviderBusyError))).await; return; }
                                };
                                let page_lock_started = Instant::now();
                                let Some(page_lock) = send_unless_disconnected(
                                    &chunks,
                                    acquire_page_lock(&lock, queue_deadline),
                                ).await else {
                                    return;
                                };
                                request_metrics.record_provider_page_lock_wait(
                                    &provider_name,
                                    page_lock_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                                );
                                let _guard = match page_lock {
                                    Ok(guard) => guard,
                                    Err(error) => {
                                        let _ = chunks.send(Err(error)).await;
                                        return;
                                    }
                                };
                                // A queued request may have been cancelled
                                // while another send owned the page lock.
                                if chunks.is_closed() { return; }
                                applied_imports.borrow_mut().remove(&provider);
                                let Some(opened) = send_unless_disconnected(&chunks,
                                    ensure_provider_page(&browser_for_save, &pages, provider)).await else { return; };
                                let just_opened = match opened {
                                    Ok(opened) => opened,
                                    Err(error) => { let _ = chunks.send(Err(error)).await; return; }
                                };
                                let mut page = match PageLease::take(&pages, provider) {
                                    Ok(page) => page,
                                    Err(error) => { let _ = chunks.send(Err(error)).await; return; }
                                };
                                // Reopen only a validated provider-owned thread. Otherwise return
                                // to the provider landing page to avoid cross-conversation reuse.
                                // Navigating the current page avoids leaking one tab per conversation.
                                let target_url = thread_url.as_deref().unwrap_or_else(|| provider.login_url());
                                if thread_url.is_some()
                                    && !pool_policy::provider_thread_url_is_same_origin(target_url, provider.login_url())
                                {
                                    let _ = chunks
                                        .send(Err(anyhow::anyhow!(
                                            "provider thread URL origin mismatch; refusing to navigate"
                                        )))
                                        .await;
                                    return;
                                }
                                if (!just_opened || thread_url.is_some()) && pool_policy::provider_send_requires_navigation(
                                    &page.url(), target_url, thread_url.is_some(),
                                ) {
                                    let Some(navigation) = send_unless_disconnected(&chunks,
                                        navigation::navigate_provider_document(&mut page, target_url, Duration::from_secs(30))).await else { return; };
                                    if let Err(error) = navigation {
                                        let _ = chunks.send(Err(error)).await;
                                        return;
                                    }
                                }
                                if let Err(error) = providers::common::check_session(provider.name(), &page) {
                                    let _ = chunks.send(Err(error)).await;
                                    return;
                                }
                                if crate::provider_guard::is_challenge_page(&page) {
                                    provider_guard.disable_provider(provider.name(), "challenge page detected; manual review required");
                                    let _ = chunks.send(Err(anyhow::Error::new(providers::ProviderChallengeError::new(provider.name())))).await;
                                    return;
                                }
                                let Some(composer) = send_unless_disconnected(&chunks,
                                    providers::common::wait_for_selector(&mut page, &adapter.effective_selectors().input_refs(), 15_000)).await else { return; };
                                if let Err(error) = composer {
                                    let classified = report_composer_preflight_failure(
                                        provider.name(),
                                        &page,
                                        error,
                                    );
                                    let _ = chunks.send(Err(classified)).await;
                                    return;
                                }
                                let ready = if thread_url.is_some() {
                                    provider_thread_navigation_ready(&page.url(), target_url, &page, adapter.as_ref())
                                } else {
                                    provider_landing_navigation_ready(&page.url(), target_url, &page, adapter.as_ref())
                                };
                                if !ready {
                                    let reason = if thread_url.is_none()
                                        && !pool_policy::landing_is_not_previous_thread(adapter.name(), &page.url()) {
                                        "provider landing redirected to an existing conversation; refusing cross-thread send"
                                    } else {
                                        "provider composer unavailable after navigation"
                                    };
                                    let _ = chunks.send(Err(anyhow::anyhow!(reason))).await;
                                    return;
                                }
                                // Login/session-expiry detection (Go concept,
                                // Rust form — Rust v1 had none): fail with a
                                // distinct session_expired error so the API
                                // can report it and the router can skip
                                // this provider.
                                if chunks.is_closed() { return; }
                                if let Err(e) = providers::common::check_session(provider.name(), &page) {
                                    let _ = chunks.send(Err(e)).await;
                                    return;
                                }
                                // Model selection (Go's biggest functional
                                // lead; Rust v1 never had it): new
                                // conversations only, before typing.
                                // Failure fails the attempt so the router
                                // can try the next provider.
                                if !has_history {
                                    let Some(model_selection) = send_unless_disconnected(
                                        &chunks,
                                        adapter.select_model(&mut page, &model),
                                    ).await else {
                                        return;
                                    };
                                    if let Err(e) = model_selection {
                                        let _ = chunks.send(Err(e)).await;
                                        return;
                                    }
                                }
                                // A disconnected client must not start a send
                                // or generate a new saved provider session.
                                if chunks.is_closed() { return; }

                                // Observe completion while forwarding chunks. Only a
                                // successful done chunk triggers a session snapshot;
                                // expired/failed sends must never persist stale state.
                                let (save_tx, mut save_rx) = mpsc::channel::<Result<ProviderChunk>>(32);
                                let (completion_tx, completion_rx) = oneshot::channel();
                                // Keep cancellation separate from the provider result channel: a
                                // dropped HTTP consumer must interrupt the browser send itself.
                                let cancel_send = Arc::new(Notify::new());
                                let cancel_forwarder = cancel_send.clone();
                                tokio::task::spawn_local(async move {
                                    let mut completed = false;
                                    loop {
                                        tokio::select! {
                                            // Detect disconnect immediately, even if the provider
                                            // is currently waiting on a browser operation and has
                                            // not produced another chunk yet.
                                            _ = chunks.closed() => {
                                                cancel_forwarder.notify_one();
                                                break;
                                            }
                                            chunk = save_rx.recv() => {
                                                let Some(chunk) = chunk else { break };
                                                if matches!(&chunk, Ok(chunk) if chunk.done) {
                                                    completed = true;
                                                }
                                                if chunks.send(chunk).await.is_err() {
                                                    cancel_forwarder.notify_one();
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    let _ = completion_tx.send(completed);
                                });
                                let cancel_adapter = adapter.clone();
                                let cancelled = tokio::select! {
                                    _ = cancel_send.notified() => true,
                                    _ = providers::run_send(
                                        adapter,
                                        &mut page,
                                        prompt,
                                        timeout,
                                        poll_tick,
                                        providers::ProviderSendPolicy {
                                            completion_idle,
                                            provider_guard,
                                            metrics: request_metrics,
                                        },
                                        save_tx,
                                    ) => false
                                };
                                if cancelled {
                                    cancel_adapter.cancel_generation(&page);
                                    providers::common::drive_page(&mut page, Duration::from_millis(100)).await;
                                    return;
                                }
                                // Save only after the send loop has finished and while
                                // this provider's page lock is still held. A cancelled send
                                // returns above without persisting a partial session.
                                if completion_rx.await.unwrap_or(false) {
                                    if let Err(error) = session::save_session_if_changed(
                                        &browser_for_save,
                                        &page,
                                        &provider_name,
                                        &send_session_write_tracker,
                                    ).await {
                                        tracing::warn!(provider = %provider_name, category = session_worker_failure_category(&error), "rotated session snapshot failed");
                                    }
                                }
                            });
                        }
                        Command::CurrentUrl { provider, reply } => {
                            let pages = pages.clone();
                            let lock = locks.borrow().get(&provider).cloned();
                            tokio::task::spawn_local(async move {
                                let result = if let Some(lock) = lock {
                                    let _guard = lock.lock().await;
                                    pages.borrow().get(&provider).map(|page| page.url())
                                        .ok_or_else(|| anyhow::anyhow!("provider page is cold"))
                                } else { Err(anyhow::anyhow!("provider is not enabled")) };
                                let _ = reply.send(result);
                            });
                        }
                        Command::VerifySession { provider, reply } => {
                            let pages = pages.clone();
                            let locks = locks.clone();
                            let registry = registry.clone();
                            tokio::task::spawn_local(async move {
                                let result: Result<bool> = async {
                                    let lock = locks.borrow().get(&provider).cloned()
                                        .ok_or_else(|| anyhow::anyhow!("provider page lock missing"))?;
                                    let adapter = registry.get(&provider).cloned()
                                        .ok_or_else(|| anyhow::anyhow!("provider not enabled"))?;
                                    let _guard = lock.lock().await;
                                    let Some(page) = pages.borrow().get(&provider).cloned() else {
                                        return Ok(false);
                                    };
                                    // An inconclusive probe is false, never a verified login.
                                    // Preserve the distinction used during warm-up and by status.
                                    verify_session_page(provider, &page, adapter.as_ref())
                                }.await;
                                let _ = reply.send(result);
                            });
                        }
                        Command::TestAdapter { provider, reply } => {
                            let browser = browser.clone();
                            let registry = registry.clone();
                            let adapter_session_write_tracker = session_write_tracker.clone();
                            let operation_gate = operation_gate.clone();
                            tokio::task::spawn_local(async move {
                                let _operation = operation_gate.lock().await;
                                let mut report = TestAdapterReport::default();
                                let finish = |report: TestAdapterReport| {
                                    let _ = reply.send(Ok(report));
                                };
                                let Some(adapter) = registry.get(&provider).cloned() else {
                                    report.errors.push("provider not enabled".to_string());
                                    finish(report);
                                    return;
                                };
                                // Fresh page with the saved session applied
                                // (same warm-up ordering: cookies before
                                // goto, localStorage after, then re-goto).
                                let mut page = match super::browser_factory::new_page(&browser).await {
                                    Ok(p) => p,
                                    Err(e) => {
                                        report.errors.push(format!("open page: {e}"));
                                        finish(report);
                                        return;
                                    }
                                };
                                // Test-adapter navigation uses an independent page, so attach
                                // the same sanitized network diagnostics before its first load.
                                crate::browser::network_tap::attach_provider_network_tap(&mut page, provider);
                                let session_file = match session::load_session_file(provider.name()) {
                                    Ok(file) => file,
                                    Err(error) => {
                                        report.errors.push(format!("session file load failed: {error}"));
                                        None
                                    }
                                };
                                if !record_test_adapter_restore_result(
                                    &mut report,
                                    "session restore failed",
                                    PageSession::open(
                                        &browser,
                                        &mut page,
                                        provider,
                                        session_file.as_ref(),
                                    )
                                    .await,
                                ) {
                                    finish(report);
                                    return;
                                }
                                providers::common::drive_page(&mut page, Duration::from_millis(1200)).await;
                                // Classify the landed page without exposing prompts,
                                // thread ids, account tokens or cookie values.
                                report.page = Some(sanitize_page_location(&page.url()));
                                report.restored_cookie_count = Some(browser.cookies().get_all().len());
                                // Phase 0 — session check.
                                if providers::common::is_login_page(&page) {
                                    report.errors.push("session expired: re-login required before running --test-adapter".to_string());
                                    finish(report);
                                    return;
                                }
                                report.session_ok = true;
                                // Selector probe over the full Rust v1
                                // arrays (snapshot, not Go's 5s waits —
                                // obscura query_selector is instant).
                                let effective = adapter.effective_selectors();
                                let inputs = effective.input_refs();
                                let sends = effective.send_refs();
                                let responses = effective.response_refs();
                                let sets: [(&str, &[&str]); 3] = [
                                    ("input", &inputs),
                                    ("send", &sends),
                                    ("response", &responses),
                                ];
                                report.probe = providers::common::probe_selector_groups(&page, &sets);
                                report.input_controls = diagnostic_input_controls(&page);
                                // Phase 1 — input (20s, Go's budget).
                                let input_sel = match providers::common::wait_for_selector(&mut page, sets[0].1, 20000).await {
                                    Ok(()) => diagnostic_input_selector(&page, sets[0].1).unwrap_or(sets[0].1[0]),
                                    Err(e) => {
                                        report.input_controls = diagnostic_input_controls(&page);
                                        report.errors.push(format!("input: {e}"));
                                        finish(report);
                                        return;
                                    }
                                };
                                report.input_found = true;
                                // Phase 2 — dummy prompt (Go RunAdapterTest).
                                const DUMMY: &str = "chat-gateway selector test — please reply with the word OK";
                                if let Err(e) = providers::common::human_type(&page, &[input_sel], DUMMY).await {
                                    report.errors.push(format!("input type: {e}"));
                                    finish(report);
                                    return;
                                }
                                providers::common::drive_page(&mut page, Duration::from_millis(400)).await;
                                report.input_controls = diagnostic_input_controls(&page);
                                report.typed_prompt_retained =
                                    Some(diagnostic_prompt_retained(&page, input_sel, DUMMY));
                                if report.typed_prompt_retained != Some(true) {
                                    report.errors.push("input: diagnostic prompt changed before send".to_string());
                                    finish(report);
                                    return;
                                }
                                // Only fixed, known control names enter diagnostics. Do not
                                // forward arbitrary upstream labels or input values.
                                for (label, selector) in [
                                    ("Send", "button[aria-label=\"Send\"]"),
                                    ("Send prompt", "button[aria-label=\"Send prompt\"]"),
                                    ("Send message", "button[aria-label=\"Send message\"]"),
                                    ("Mobile composer Send message", "form:has(textarea#mobile-composer-prompt[name=\"prompt\"]) button[aria-label=\"Send message\"]"),
                                    ("Submit", "button[aria-label=\"Submit\"]"),
                                    ("Submit prompt", "button[aria-label=\"Submit prompt\"]"),
                                    ("send-button test id", "button[data-testid=\"send-button\"]"),
                                    ("composer-submit-button test id", "button[data-testid=\"composer-submit-button\"]"),
                                ] {
                                    if page.query_selector(selector).is_some() { report.send_controls.push(label.into()); }
                                }
                                let baseline = adapter.response_text(&page).filter(|t| !t.is_empty());
                                // Phase 3 — send button or Enter (5s).
                                match providers::common::click_send_or_enter(&mut page, sets[1].1, input_sel, 5000).await {
                                    Ok(clicked) => report.send_clicked = clicked,
                                    Err(e) => {
                                        report.errors.push(format!("send: {e}"));
                                        finish(report);
                                        return;
                                    }
                                }
                                // Phase 4 — up to 45s (Go) for a response
                                // token differing from baseline. Go drains
                                // a mutation-observer queue; obscura
                                // exposes no observer hook, so snapshot
                                // polling (the run_send technique) is the
                                // engine equivalent.
                                providers::common::drive_page(&mut page, Duration::from_millis(1500)).await;
                                let deadline = std::time::Instant::now() + Duration::from_secs(45);
                                while std::time::Instant::now() < deadline {
                                    if let Some(t) = adapter.response_text(&page) {
                                        if !t.is_empty() && Some(&t) != baseline.as_ref() {
                                            report.response_token = true;
                                            break;
                                        }
                                    }
                                    providers::common::drive_page(&mut page, Duration::from_millis(300)).await;
                                }
                                if !report.response_token {
                                    report.errors.push("response: no token within 45s".to_string());
                                } else if let Err(error) =
                                    session::save_session_if_changed(&browser, &page, provider.name(), &adapter_session_write_tracker).await
                                {
                                    tracing::warn!(provider = provider.name(), category = session_worker_failure_category(&error), "probe session snapshot failed");
                                }
                                finish(report);
                            });
                        }
                        Command::RefreshSession { provider, mut reply } => {
                            if reply.is_closed() { continue; }
                            let operation_gate = operation_gate.clone();
                            let applied_imports = applied_imports.clone();
                            let provider_sessions = provider_sessions.clone();
                            let browser = browser.clone();
                            let pages = pages.clone();
                            let locks = locks.clone();
                            tokio::task::spawn_local(async move {
                                let res: Result<()> = async {
                                    let Some(_operation) = session_refresh_permit(&mut reply, &operation_gate).await else { return Ok(()); };
                                    let lock = locks.borrow().get(&provider).cloned()
                                        .ok_or_else(|| anyhow::anyhow!("provider page lock missing"))?;
                                    let Some(_guard) = session_refresh_permit(&mut reply, &lock).await else {
                                        return Ok(());
                                    };
                                    if reply.is_closed() { return Ok(()); }
                                    let file = session::load_session_file(provider.name())?
                                        .ok_or_else(|| anyhow::anyhow!("imported provider session is missing"))?;
                                    // Validate every eligible cookie before the refresh clears
                                    // the old browser-wide jar. Invalid snapshots must not
                                    // disrupt a previously usable provider page/session.
                                    session::preflight_cookie_restore(&file, provider.name())?;
                                    let activated = page_session::activate_if_changed(&applied_imports, provider, &file, || async {
                                    page_session::replace_after_success(
                                    || async {
                                    let mut page = super::browser_factory::new_page(&browser).await?;
                                    // Observe script fetch failures on the restored page too.
                                    crate::browser::network_tap::attach_provider_network_tap(&mut page, provider);
                                    page.goto(provider.login_url()).await?;
                                    let storage_cleared = page.evaluate(
                                        "(()=>{try{localStorage.clear();return true;}catch(_){return false;}})()"
                                    );
                                    if !storage_cleared.as_bool().unwrap_or(false) {
                                        anyhow::bail!("provider localStorage could not be cleared");
                                    }
                                    session::clear_provider_cookies(&browser, provider.name())?;
                                    PageSession::open(&browser, &mut page, provider, Some(&file)).await?;
                                    Ok(Rc::new(page))
                                    },
                                    |replacement| { pages.borrow_mut().insert(provider, replacement); },
                                    ).await
                                    }).await?;
                                    if activated { provider_sessions.record_session_import(provider.name()); }
                                    Ok(())
                                }.await;
                                if res.is_err() { provider_sessions.record_probe_failure(provider.name()); }
                                let _ = reply.send(res);
                            });
                        }
                        Command::UnlinkSession { provider, reply } => {
                            let browser = browser.clone();
                            let pages = pages.clone();
                            let locks = locks.clone();
                            let login_pages = login_pages.clone();
                            let login_meta = login_meta.clone();
                            let operation_gate = operation_gate.clone();
                            let applied_imports = applied_imports.clone();
                            tokio::task::spawn_local(async move {
                                let _operation = operation_gate.lock().await;
                                let res: Result<bool> = async {
                                    let lock = locks.borrow().get(&provider).cloned()
                                        .unwrap_or_else(|| Rc::new(tokio::sync::Mutex::new(())));
                                    let _guard = lock.lock().await;
                                    applied_imports.borrow_mut().remove(&provider);
                                    let mut page = super::browser_factory::new_page(&browser).await?;
                                    page.goto(provider.login_url()).await?;
                                    let storage_cleared = page.evaluate(
                                        "(()=>{try{localStorage.clear();return true;}catch(_){return false;}})()"
                                    );
                                    if !storage_cleared.as_bool().unwrap_or(false) {
                                        anyhow::bail!("provider localStorage could not be cleared");
                                    }
                                    session::clear_provider_cookies(&browser, provider.name())?;
                                    page.goto(provider.login_url()).await?;
                                    let removed = session::delete_session_snapshot(provider.name())?;
                                    pages.borrow_mut().insert(provider, Rc::new(page));
                                    login_pages.borrow_mut().remove(&provider);
                                    login_meta.borrow_mut().remove(&provider);
                                    Ok(removed)
                                }.await;
                                let _ = reply.send(res);
                            });
                        }
                        Command::LoginStart { provider, reply } => {
                            let browser = browser.clone();
                            let login_pages = login_pages.clone();
                            let login_meta = login_meta.clone();
                            let operation_gate = operation_gate.clone();
                            tokio::task::spawn_local(async move {
                                let _operation = operation_gate.lock().await;
                                sweep_login_pages(&login_pages, &login_meta);
                                // A fresh login page per start (Go used an
                                // isolated temp profile for the same
                                // reason); any previous login page for
                                // this provider is dropped. Note the
                                // browser-wide cookie jar is shared with
                                // the warm pages, so a still-valid saved
                                // session may already show as logged in.
                                login_pages.borrow_mut().remove(&provider);
                                login_meta.borrow_mut().remove(&provider);
                                let res: Result<String> = async {
                                    let mut page = super::browser_factory::new_page(&browser).await?;
                                    page.goto(provider.login_url()).await?;
                                    let url = page.url();
                                    login_pages.borrow_mut().insert(provider, Rc::new(page));
                                    login_meta.borrow_mut().insert(provider, LoginMeta { last_seen: Instant::now() });
                                    Ok(url)
                                }.await;
                                let _ = reply.send(res);
                            });
                        }
                        Command::LoginCancel { provider, reply } => {
                            login_pages.borrow_mut().remove(&provider);
                            login_meta.borrow_mut().remove(&provider);
                            let _ = reply.send(Ok(()));
                        }
                        Command::LoginComplete { provider, reply } => {
                            let browser = browser.clone();
                            let registry = registry.clone();
                            let pages = pages.clone();
                            let locks = locks.clone();
                            let login_pages = login_pages.clone();
                            let login_meta = login_meta.clone();
                            let login_session_write_tracker = session_write_tracker.clone();
                            let applied_imports = applied_imports.clone();
                            tokio::task::spawn_local(async move {
                                sweep_login_pages(&login_pages, &login_meta);
                                let res: Result<LoginCompleted> = async {
                                    let Some(login_page) = login_pages.borrow().get(&provider).cloned() else {
                                        anyhow::bail!("no active login page for {} (press Start first)", provider.name());
                                    };
                                    // Serialize with in-flight sends so
                                    // the warm-page swap below cannot race
                                    // a send already running on the old
                                    // page. A send spawned just before this
                                    // swap still uses its cloned old page
                                    // for that one attempt; later sends use
                                    // the swapped page.
                                    let lock = locks.borrow().get(&provider).cloned()
                                        .unwrap_or_else(|| Rc::new(tokio::sync::Mutex::new(())));
                                    let _guard = lock.lock().await;
                                    if crate::provider_guard::is_challenge_page(&login_page) {
                                        anyhow::bail!("provider challenge requires manual completion before saving a session");
                                    }
                                    let adapter = registry.get(&provider)
                                        .ok_or_else(|| anyhow::anyhow!("provider adapter is not registered"))?;
                                    super::diagnostics::require_login_completion_ready(
                                        verify_session_page(provider, &login_page, adapter.as_ref())
                                    )?;
                                    applied_imports.borrow_mut().remove(&provider);
                                    // Same format/path as every other save:
                                    // sessions/<provider>.json via
                                    // session::save_session (cookies from
                                    // the browser-wide CookieStore +
                                    // localStorage from this page).
                                    session::save_session(&browser, &login_page, provider.name(), &login_session_write_tracker).await?;
                                    pages.borrow_mut().insert(provider, login_page);
                                    login_pages.borrow_mut().remove(&provider);
                                    login_meta.borrow_mut().remove(&provider);
                                    Ok(LoginCompleted {
                                        session_path: session::session_path(provider.name())?,
                                        warm_swapped: true,
                                        warning: None,
                                    })
                                }.await;
                                let _ = reply.send(res);
                            });
                        }
                        Command::LoginStatusAll { reply } => {
                            let login_pages = login_pages.clone();
                            let login_meta = login_meta.clone();
                            let registry = registry.clone();
                            tokio::task::spawn_local(async move {
                                sweep_login_pages(&login_pages, &login_meta);
                                let mut out = vec![];
                                for (provider, page) in login_pages.borrow().iter() {
                                    let provider = *provider;
                                    let page = page.clone();
                                    out.push(login_active_info(provider, &page, &registry));
                                }
                                let _ = reply.send(out);
                            });
                        }
                        Command::LoginView { provider, reply } => {
                            let login_pages = login_pages.clone();
                            let login_meta = login_meta.clone();
                            let registry = registry.clone();
                            tokio::task::spawn_local(async move {
                                sweep_login_pages(&login_pages, &login_meta);
                                touch_login_meta(&login_meta, provider);
                                let res = match login_pages.borrow().get(&provider).cloned() {
                                    Some(page) => Ok(build_login_view(provider, &page, &registry)),
                                    None => Ok(LoginView::inactive(provider.name())),
                                };
                                let _ = reply.send(res);
                            });
                        }
                        Command::LoginAct { provider, action, reply } => {
                            let browser = browser.clone();
                            let login_pages = login_pages.clone();
                            let login_meta = login_meta.clone();
                            tokio::task::spawn_local(async move {
                                sweep_login_pages(&login_pages, &login_meta);
                                touch_login_meta(&login_meta, provider);
                                let res: Result<String> = async {
                                    match action {
                                        LoginAction::Click { selector } => {
                                            let Some(page) = login_pages.borrow().get(&provider).cloned() else {
                                                anyhow::bail!("no active login page for {}", provider.name());
                                            };
                                            login::validate_navigation_url_for_provider(provider.name(), &page.url())?;
                                            match page.query_selector(&selector) {
                                                Some(el) => { el.click()?; Ok(format!("clicked {selector}")) }
                                                None => anyhow::bail!("no element matches {selector}"),
                                            }
                                        }
                                        LoginAction::ClickButton { index } => {
                                            let Some(page) = login_pages.borrow().get(&provider).cloned() else {
                                                anyhow::bail!("no active login page for {}", provider.name());
                                            };
                                            login::validate_navigation_url_for_provider(provider.name(), &page.url())?;
                                            let js = format!(
                                                "(function(){{ var b=document.querySelectorAll('button, [role=\"button\"]')[{}]; if(!b) return false; b.click(); return true; }})()",
                                                index
                                            );
                                            if page.evaluate(&js).as_bool() == Some(true) {
                                                Ok(format!("clicked button #{index}"))
                                            } else {
                                                anyhow::bail!("no button #{index} on the page")
                                            }
                                        }
                                        // NOTE: `text` is used only to drive
                                        // human_type and is never logged,
                                        // stored, or echoed back.
                                        LoginAction::Type { selector, text } => {
                                            let Some(page) = login_pages.borrow().get(&provider).cloned() else {
                                                anyhow::bail!("no active login page for {}", provider.name());
                                            };
                                            login::validate_navigation_url_for_provider(provider.name(), &page.url())?;
                                            providers::common::human_type(&page, &[selector.as_str()], &text).await?;
                                            Ok(format!("typed into {selector}"))
                                        }
                                        LoginAction::Enter { selector } => {
                                            let Some(page) = login_pages.borrow().get(&provider).cloned() else {
                                                anyhow::bail!("no active login page for {}", provider.name());
                                            };
                                            login::validate_navigation_url_for_provider(provider.name(), &page.url())?;
                                            providers::common::press_enter(&page, &selector)?;
                                            Ok(format!("Enter dispatched on {selector}"))
                                        }
                                        LoginAction::Navigate { url } => {
                                            // Rc<Page> cannot goto (it needs
                                            // &mut), so navigation recreates
                                            // the page. Cookies survive
                                            // (browser-wide jar); page JS
                                            // state does not — acceptable
                                            // for SSO hops, which re-drive
                                            // from the URL.
                                            login_pages.borrow_mut().remove(&provider);
                                            let mut page = super::browser_factory::new_page(&browser).await?;
                                            page.goto(&url).await?;
                                            let final_url = page.url();
                                            // A trusted URL may redirect elsewhere. Never leave a
                                            // redirected, untrusted page available for relay actions.
                                            if let Err(error) = login::validate_navigation_url_for_provider(provider.name(), &final_url) {
                                                anyhow::bail!("navigation redirected outside provider/SSO allowlists: {error}");
                                            }
                                            login_pages.borrow_mut().insert(provider, Rc::new(page));
                                            Ok(format!("navigated to {final_url}"))
                                        }
                                    }
                                }.await;
                                let _ = reply.send(res);
                            });
                        }
                    }
                }
            }).await;
        });
    })
}

#[cfg(test)]
mod queued_send_cancellation_tests {
    use super::*;

    #[tokio::test]
    async fn already_disconnected_consumer_prevents_any_queued_side_effect() {
        let (tx, rx) = mpsc::channel::<Result<ProviderChunk>>(1);
        drop(rx);
        let ran = std::cell::Cell::new(false);
        let output = send_unless_disconnected(&tx, async {
            ran.set(true);
            7
        })
        .await;
        assert_eq!(output, None);
        assert!(
            !ran.get(),
            "abandoned command must not execute its operation"
        );
    }

    #[tokio::test]
    async fn disconnect_interrupts_in_progress_provider_wait() {
        let (tx, rx) = mpsc::channel::<Result<ProviderChunk>>(1);
        let (started_tx, started_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            send_unless_disconnected(&tx, async {
                let _ = started_tx.send(());
                std::future::pending::<()>().await
            })
            .await
        });
        started_rx.await.unwrap();
        drop(rx);
        let output = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("disconnection must cancel a blocked provider operation")
            .unwrap();
        assert_eq!(output, None);
    }

    #[tokio::test]
    async fn busy_provider_page_lock_has_bounded_wait_and_recovers() {
        let lock = tokio::sync::Mutex::new(());
        let held = lock.lock().await;
        let error = acquire_page_lock(
            &lock,
            tokio::time::Instant::now() + Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert!(error.downcast_ref::<ProviderBusyError>().is_some());
        drop(held);
        assert!(acquire_page_lock(
            &lock,
            tokio::time::Instant::now() + Duration::from_millis(50),
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn expired_shared_queue_deadline_rejects_second_page_lock_wait() {
        let lock = tokio::sync::Mutex::new(());
        let held = lock.lock().await;
        // The operation-gate stage may have consumed the queue budget before
        // page-lock admission. Do not grant another full queue timeout.
        let expired = tokio::time::Instant::now() - Duration::from_millis(1);
        let error = acquire_page_lock(&lock, expired).await.unwrap_err();
        assert!(error.downcast_ref::<ProviderBusyError>().is_some());
        drop(held);
    }

    #[tokio::test]
    async fn connected_consumer_allows_provider_operation() {
        let (tx, _rx) = mpsc::channel::<Result<ProviderChunk>>(1);
        let output = send_unless_disconnected(&tx, async { 7 }).await;
        assert_eq!(output, Some(7));
    }
}

#[cfg(test)]
mod refresh_cancellation_tests {
    use super::*;

    #[tokio::test]
    async fn disconnected_refresh_does_not_acquire_an_available_lock() {
        let (mut tx, rx) = oneshot::channel::<Result<()>>();
        drop(rx);
        let lock = tokio::sync::Mutex::new(());
        assert!(session_refresh_permit(&mut tx, &lock).await.is_none());
    }

    #[tokio::test]
    async fn queued_refresh_disconnects_without_waiting_for_busy_provider() {
        let lock = Rc::new(tokio::sync::Mutex::new(()));
        let held = lock.lock().await;
        let (mut tx, rx) = oneshot::channel::<Result<()>>();
        let waiting = session_refresh_permit(&mut tx, &lock);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut waiting)
                .await
                .is_err()
        );
        drop(rx);
        assert!(tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .is_none());
        drop(held);
        assert!(lock.try_lock().is_ok());
    }

    #[tokio::test]
    async fn connected_refresh_can_activate_after_current_operation_finishes() {
        let lock = tokio::sync::Mutex::new(());
        let held = lock.lock().await;
        let (mut tx, _rx) = oneshot::channel::<Result<()>>();
        let waiting = session_refresh_permit(&mut tx, &lock);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut waiting)
                .await
                .is_err()
        );
        drop(held);
        assert!(waiting.await.is_some());
    }
}
