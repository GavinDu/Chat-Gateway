use super::pool_policy;
use crate::config::AppConfig;
use crate::login::{self, LoginAction, LoginView};
use crate::metrics::MetricsRegistry;
use crate::provider_guard::ProviderGuard;
use crate::provider_state::ProviderSessionRegistry;
use crate::providers::{self, Provider, ProviderChunk};
use crate::session::{self, SessionWriteTracker};
use anyhow::Result;
use dashmap::DashMap;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, Notify, OwnedSemaphorePermit, Semaphore};

#[path = "pool/browser_factory.rs"]
mod browser_factory;
#[path = "pool/diagnostics.rs"]
mod diagnostics;
#[path = "pool/login_helpers.rs"]
mod login_helpers;
#[path = "pool/navigation.rs"]
mod navigation;
#[path = "pool/page_cache.rs"]
mod page_cache;
#[path = "pool/page_session.rs"]
mod page_session;
#[path = "pool/supervisor.rs"]
mod supervisor;
#[path = "pool/worker.rs"]
mod worker;
use browser_factory::build_browser;
use diagnostics::verify_session_page;
pub use diagnostics::ProviderSessionExpiredError;
use login_helpers::{
    build_login_view, login_active_info, sweep_login_pages, touch_login_meta, LoginMeta,
};
use navigation::{provider_landing_navigation_ready, provider_thread_navigation_ready};
use page_cache::{PageLease, ProviderPageCache, SharedPages};
use page_session::PageSession;
use supervisor::{
    closed_command_channel, effective_session_pool_size, join_finished_worker,
    join_worker_on_shutdown, next_browser_worker_restart_backoff, wait_for_worker_shutdown,
    BrowserWorkerSpawner,
};
use worker::browser_thread;

#[cfg(test)]
mod queue_capacity_tests {
    use super::effective_session_pool_size;

    #[test]
    fn configured_capacity_never_exceeds_single_persistent_page() {
        assert_eq!(effective_session_pool_size(1), 1);
        assert_eq!(effective_session_pool_size(2), 1);
        assert_eq!(effective_session_pool_size(8), 1);
    }
}

#[derive(Debug)]
pub struct ProviderBusyError;

impl std::fmt::Display for ProviderBusyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("provider queue timeout expired")
    }
}

impl std::error::Error for ProviderBusyError {}

/// A command could not be delivered because the browser worker is restarting.
/// Callers may safely retry this error against another provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserWorkerUnavailableError;

impl BrowserWorkerUnavailableError {
    pub fn is_retryable(&self) -> bool {
        true
    }
}

impl std::fmt::Display for BrowserWorkerUnavailableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("browser worker unavailable; retry after recovery")
    }
}

impl std::error::Error for BrowserWorkerUnavailableError {}

fn browser_worker_unavailable() -> anyhow::Error {
    anyhow::Error::new(BrowserWorkerUnavailableError)
}

#[cfg(test)]
mod test_adapter_restore_tests {
    use super::{record_test_adapter_restore_result, TestAdapterReport};

    #[test]
    fn restore_success_allows_adapter_probe_to_continue() {
        let mut report = TestAdapterReport::default();
        assert!(record_test_adapter_restore_result(
            &mut report,
            "session restore failed",
            Ok(())
        ));
        assert!(report.errors.is_empty());
    }

    #[test]
    fn restore_failure_is_reported_and_stops_adapter_probe() {
        let mut report = TestAdapterReport::default();
        let should_continue = record_test_adapter_restore_result(
            &mut report,
            "session restore failed",
            Err(anyhow::anyhow!("injected restore failure")),
        );
        assert!(!should_continue);
        assert_eq!(
            report.errors,
            vec!["session restore failed: injected restore failure"]
        );
    }
}

#[cfg(test)]
mod browser_worker_error_tests {
    use super::{browser_worker_unavailable, BrowserWorkerUnavailableError};

    #[test]
    fn worker_loss_is_exposed_as_a_typed_retryable_error() {
        let error = browser_worker_unavailable();
        let typed = error
            .downcast_ref::<BrowserWorkerUnavailableError>()
            .expect("worker loss should preserve its error type");
        assert!(typed.is_retryable());
    }
}

/// ObscuraPool - one persistent, pre-warmed page per provider
/// (DashMap-indexed by provider name on the API side).
///
/// The embedded obscura engine is thread-affine (each Page wraps a V8
/// runtime behind a RefCell and is neither Clone nor Send), so all pages
/// live on a dedicated browser thread running a current-thread runtime
/// and LocalSet. The HTTP side talks to it over channels; per-provider
/// semaphores (this side) bound concurrent sends, and a per-provider
/// async mutex (browser side) serializes access to its single page.
/// Provider switch is therefore a channel send + map lookup, not a
/// process/tab spawn.
pub struct ObscuraPool {
    cmd_tx: Arc<tokio::sync::RwLock<mpsc::Sender<Command>>>,
    ready: tokio::sync::Mutex<Option<oneshot::Receiver<Result<()>>>>,
    semaphores: DashMap<String, Arc<Semaphore>>,
    cfg: Arc<AppConfig>,
    shutting_down: Arc<AtomicBool>,
    worker_ready: Arc<AtomicBool>,
    worker_generation: Arc<AtomicU64>,
    provider_guard: ProviderGuard,
    provider_sessions: ProviderSessionRegistry,
    metrics: MetricsRegistry,
    session_writes: SessionWriteTracker,
}

enum Command {
    Discover {
        provider: Provider,
        reply: oneshot::Sender<Result<Vec<String>>>,
    },
    Send {
        provider: Provider,
        prompt: String,
        model: String,
        has_history: bool,
        thread_url: Option<String>,
        timeout: Duration,
        completion_idle: Duration,
        provider_guard: ProviderGuard,
        chunks: mpsc::Sender<Result<ProviderChunk>>,
    },
    CurrentUrl {
        provider: Provider,
        reply: oneshot::Sender<Result<String>>,
    },
    VerifySession {
        provider: Provider,
        reply: oneshot::Sender<Result<bool>>,
    },
    TestAdapter {
        provider: Provider,
        reply: oneshot::Sender<Result<TestAdapterReport>>,
    },
    RefreshSession {
        provider: Provider,
        reply: oneshot::Sender<Result<()>>,
    },
    UnlinkSession {
        provider: Provider,
        reply: oneshot::Sender<Result<bool>>,
    },
    // Login delegate UI (see src/login.rs): dedicated pages on the
    // browser thread, separate from the warm provider pages. Login
    // pages are plain Rc<Page> (no &mut needed after creation — all
    // relay actions use &Page methods; Navigate recreates the page).
    LoginStart {
        provider: Provider,
        reply: oneshot::Sender<Result<String>>,
    },
    LoginCancel {
        provider: Provider,
        reply: oneshot::Sender<Result<()>>,
    },
    LoginComplete {
        provider: Provider,
        reply: oneshot::Sender<Result<LoginCompleted>>,
    },
    LoginStatusAll {
        reply: oneshot::Sender<Vec<LoginActiveInfo>>,
    },
    LoginView {
        provider: Provider,
        reply: oneshot::Sender<Result<LoginView>>,
    },
    LoginAct {
        provider: Provider,
        action: LoginAction,
        reply: oneshot::Sender<Result<String>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// One provider's live login-page info for the /login status endpoint.
#[derive(Debug, Clone)]
pub struct LoginActiveInfo {
    pub provider: String,
    pub url: String,
    pub title: String,
    pub logged_in: Option<bool>,
    pub input_found: bool,
    pub state: String,
}

/// Outcome of POST /login/api/:provider/complete.
#[derive(Debug, Clone)]
pub struct LoginCompleted {
    pub session_path: String,
    pub warm_swapped: bool,
    pub warning: Option<String>,
}

/// Record a TestAdapter restore failure and signal that probing must stop.
fn record_test_adapter_restore_result(
    report: &mut TestAdapterReport,
    stage: &str,
    result: Result<()>,
) -> bool {
    match result {
        Ok(()) => true,
        Err(error) => {
            report.errors.push(format!("{stage}: {error}"));
            false
        }
    }
}

/// Live-send test-adapter outcome, phase by phase (Go RunAdapterTest
/// shape, adapted to the restored tree: probe uses the full Rust v1
/// selector arrays, and the live send uses the restored send path).
#[derive(Default, Debug, serde::Serialize)]
pub struct TestAdapterReport {
    pub session_ok: bool,
    pub probe: Vec<(String, Option<String>)>,
    pub input_found: bool,
    pub send_clicked: bool,
    pub response_token: bool,
    /// Diagnostic flags only; never echo typed text or arbitrary page labels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typed_prompt_retained: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub send_controls: Vec<String>,
    pub errors: Vec<String>,
}

pub struct ObscuraPermit {
    _permit: OwnedSemaphorePermit,
    pub queue_ms: u128,
}

impl ObscuraPool {
    /// Test-only admission/semaphore handle. Real Axum handler tests inject a
    /// scripted ChatBackend; no browser thread, provider cookies or owner
    /// session storage are created. Browser commands fail closed immediately.
    #[cfg(test)]
    pub(crate) fn for_scripted_backend_tests(cfg: Arc<AppConfig>) -> Arc<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(1);
        drop(cmd_rx);
        let semaphores = DashMap::new();
        for provider in cfg.providers.iter().filter(|provider| provider.enabled) {
            semaphores.insert(provider.name.clone(), Arc::new(Semaphore::new(1)));
        }
        Arc::new(Self {
            cmd_tx: Arc::new(tokio::sync::RwLock::new(cmd_tx)),
            ready: tokio::sync::Mutex::new(None),
            semaphores,
            cfg,
            shutting_down: Arc::new(AtomicBool::new(false)),
            worker_ready: Arc::new(AtomicBool::new(false)),
            worker_generation: Arc::new(AtomicU64::new(0)),
            provider_guard: ProviderGuard::default(),
            provider_sessions: ProviderSessionRegistry::default(),
            metrics: MetricsRegistry::default(),
            session_writes: SessionWriteTracker::default(),
        })
    }

    /// Spawn the browser thread (browser build + page warm-up happen
    /// there; call warm_all() to await readiness).
    pub async fn new(cfg: Arc<AppConfig>) -> Result<Arc<Self>> {
        Self::new_with_worker_spawner(cfg, Arc::new(browser_thread)).await
    }

    async fn new_with_worker_spawner(
        cfg: Arc<AppConfig>,
        worker_spawner: BrowserWorkerSpawner,
    ) -> Result<Arc<Self>> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(64);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<()>>();
        let semaphores = DashMap::new();
        for p in &cfg.providers {
            if p.enabled {
                let effective_pool_size = effective_session_pool_size(p.session_pool_size);
                if p.session_pool_size > effective_pool_size {
                    tracing::warn!(
                        provider = %p.name,
                        configured_pool_size = p.session_pool_size,
                        effective_pool_size,
                        "session_pool_size capped at one until per-conversation pages are supported"
                    );
                }
                semaphores.insert(
                    p.name.clone(),
                    Arc::new(Semaphore::new(effective_pool_size)),
                );
            }
        }
        let cmd_tx = Arc::new(tokio::sync::RwLock::new(cmd_tx));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let worker_ready = Arc::new(AtomicBool::new(false));
        let worker_generation = Arc::new(AtomicU64::new(0));
        let pool = Arc::new(Self {
            cmd_tx: cmd_tx.clone(),
            ready: tokio::sync::Mutex::new(Some(ready_rx)),
            semaphores,
            cfg: cfg.clone(),
            shutting_down: shutting_down.clone(),
            worker_ready: worker_ready.clone(),
            worker_generation: worker_generation.clone(),
            provider_guard: ProviderGuard::default(),
            provider_sessions: ProviderSessionRegistry::default(),
            metrics: MetricsRegistry::default(),
            session_writes: SessionWriteTracker::default(),
        });
        let provider_sessions = pool.provider_sessions.clone();
        let metrics = pool.metrics.clone();
        let session_writes = pool.session_writes.clone();
        let mut worker_handle = Some(worker_spawner(
            cfg.clone(),
            cmd_rx,
            ready_tx,
            worker_ready.clone(),
            provider_sessions.clone(),
            metrics.clone(),
            session_writes.clone(),
        ));

        // Recreate the thread-affine engine after a crash; never move Page values
        // between threads. The worker performs the same browser build and warm-up
        // sequence before it is advertised as ready again.
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if shutting_down.load(Ordering::Acquire) {
                    join_worker_on_shutdown(worker_handle.take()).await;
                    return;
                }
                if !cmd_tx.read().await.is_closed() {
                    continue;
                }
                worker_ready.store(false, Ordering::Release);
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = wait_for_worker_shutdown(&shutting_down) => {
                        join_worker_on_shutdown(worker_handle.take()).await;
                        return;
                    }
                }
                if shutting_down.load(Ordering::Acquire) {
                    join_worker_on_shutdown(worker_handle.take()).await;
                    return;
                }
                if !cmd_tx.read().await.is_closed() {
                    continue;
                }
                // The receiver closes as the worker exits. Retain and join the
                // thread handle before creating a replacement so a slow or
                // stuck worker cannot be silently orphaned on every retry.
                if !join_finished_worker(&mut worker_handle) {
                    continue;
                }
                let (next_tx, next_rx) = mpsc::channel::<Command>(64);
                *cmd_tx.write().await = next_tx;
                let (next_ready_tx, next_ready_rx) = oneshot::channel::<Result<()>>();
                worker_handle = Some(worker_spawner(
                    cfg.clone(),
                    next_rx,
                    next_ready_tx,
                    worker_ready.clone(),
                    provider_sessions.clone(),
                    metrics.clone(),
                    session_writes.clone(),
                ));
                match tokio::time::timeout(Duration::from_secs(90), next_ready_rx).await {
                    Ok(Ok(Ok(()))) => {
                        worker_ready.store(true, Ordering::Release);
                        worker_generation.fetch_add(1, Ordering::AcqRel);
                        metrics.record_browser_worker_restart();
                        tracing::info!("browser worker restarted and warmed successfully");
                        backoff = Duration::from_secs(1);
                    }
                    Ok(Ok(Err(error))) => {
                        tracing::warn!(error = %error, "browser worker restart failed during warm-up");
                        // Force the next supervisor pass to retry. The failed
                        // worker may still own its receiver, so merely dropping
                        // this sender would not make the channel appear closed.
                        *cmd_tx.write().await = closed_command_channel();
                        worker_ready.store(false, Ordering::Release);
                        backoff = next_browser_worker_restart_backoff(backoff);
                    }
                    Ok(Err(_)) => {
                        tracing::warn!("browser worker exited before restart warm-up completed");
                        *cmd_tx.write().await = closed_command_channel();
                        worker_ready.store(false, Ordering::Release);
                        backoff = next_browser_worker_restart_backoff(backoff);
                    }
                    Err(_) => {
                        tracing::warn!("browser worker restart warm-up timed out");
                        // Close the timed-out worker's channel so the next
                        // supervisor pass can retry instead of remaining stuck.
                        *cmd_tx.write().await = closed_command_channel();
                        worker_ready.store(false, Ordering::Release);
                        backoff = next_browser_worker_restart_backoff(backoff);
                    }
                }
            }
        });
        Ok(pool)
    }

    /// Clone the per-gateway provider pacing and challenge guard.
    pub fn provider_guard(&self) -> ProviderGuard {
        self.provider_guard.clone()
    }

    /// Clone the per-gateway provider session lifecycle registry.
    pub fn provider_sessions(&self) -> ProviderSessionRegistry {
        self.provider_sessions.clone()
    }

    pub fn metrics(&self) -> MetricsRegistry {
        self.metrics.clone()
    }

    /// Whether the dedicated browser thread still owns its command receiver.
    ///
    /// A closed command channel means the worker exited or panicked; it does not imply automatic recovery. Readiness
    /// must fail closed in that state instead of advertising the configured
    /// providers as usable.
    pub fn worker_alive(&self) -> bool {
        self.worker_ready.load(Ordering::Acquire)
    }

    /// Monotonic count of successful worker generations after the initial worker.
    /// The value changes only after a replacement worker has completed warm-up.
    pub fn worker_generation(&self) -> u64 {
        self.worker_generation.load(Ordering::Acquire)
    }

    /// Await browser build + per-provider warm-up (v2 ordering: session
    /// cookies restored into the browser store BEFORE the first goto,
    /// localStorage after goto, then a reload-equivalent second goto).
    pub async fn warm_all(&self) -> Result<()> {
        let mut slot = self.ready.lock().await;
        if let Some(rx) = slot.take() {
            rx.await
                .map_err(|_| anyhow::anyhow!("browser thread died during warm-up"))??;
            self.worker_ready.store(true, Ordering::Release);
        }
        Ok(())
    }

    /// Bound concurrent sends per provider (v1 pool semantics).
    pub async fn acquire(&self, provider: &str) -> Result<ObscuraPermit> {
        let start = std::time::Instant::now();
        let sem = self
            .semaphores
            .get(provider)
            .map(|v| v.clone())
            .unwrap_or_else(|| Arc::new(Semaphore::new(2)));
        let queue_timeout_seconds = self
            .cfg
            .provider(provider)
            .map(|p| p.queue_timeout_seconds)
            .unwrap_or(30);
        let permit = tokio::time::timeout(
            Duration::from_secs(queue_timeout_seconds),
            sem.acquire_owned(),
        )
        .await
        .map_err(|_| anyhow::Error::new(ProviderBusyError))??;
        Ok(ObscuraPermit {
            _permit: permit,
            queue_ms: start.elapsed().as_millis(),
        })
    }

    /// Discover a provider's models on a fresh page (no message sent).
    pub async fn discover(&self, name: &str) -> Result<Vec<String>> {
        let provider =
            Provider::from_name(name).ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .read()
            .await
            .send(Command::Discover {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| browser_worker_unavailable())?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped discover"))?
    }

    /// Send a prompt on the provider's warmed page; response chunks
    /// stream back over the returned channel receiver. `model` is the
    /// requested model (for UI selection on new conversations only)
    /// and `has_history` marks a returning conversation (selection
    /// skipped — the model is already fixed by the prior turns).
    pub async fn send_stream(
        &self,
        name: &str,
        prompt: String,
        model: String,
        has_history: bool,
        thread_url: Option<String>,
    ) -> Result<mpsc::Receiver<Result<ProviderChunk>>> {
        let provider =
            Provider::from_name(name).ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
        let timeout = Duration::from_secs(
            self.cfg
                .provider(name)
                .map(|p| p.timeout_seconds)
                .unwrap_or(120),
        );
        let completion_idle = Duration::from_millis(
            self.cfg
                .provider(name)
                .map(|p| p.completion_idle_ms)
                .unwrap_or(15_000),
        );
        let (tx, rx) = mpsc::channel(32);
        self.cmd_tx
            .read()
            .await
            .send(Command::Send {
                provider,
                prompt,
                model,
                has_history,
                thread_url,
                timeout,
                completion_idle,
                provider_guard: self.provider_guard.clone(),
                chunks: tx,
            })
            .await
            .map_err(|_| browser_worker_unavailable())?;
        Ok(rx)
    }

    /// Read the active provider page URL after a completed send.
    pub async fn current_url(&self, name: &str) -> Result<String> {
        let provider =
            Provider::from_name(name).ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .read()
            .await
            .send(Command::CurrentUrl {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| browser_worker_unavailable())?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped current URL reply"))?
    }

    /// Read-only authentication probe on the warmed page. The browser worker
    /// serializes it with sends through the provider's page lock.
    pub async fn verify_session(&self, name: &str) -> Result<bool> {
        let provider =
            Provider::from_name(name).ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .read()
            .await
            .send(Command::VerifySession {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| browser_worker_unavailable())?;
        rx.await.map_err(|_| browser_worker_unavailable())?
    }

    /// Live-send test-adapter run for one provider (Go RunAdapterTest
    /// phases, restored-tree form): session check → selector probe →
    /// dummy prompt send → up to 45s response-token wait.
    pub async fn test_adapter(&self, name: &str) -> Result<TestAdapterReport> {
        let provider =
            Provider::from_name(name).ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .read()
            .await
            .send(Command::TestAdapter {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| browser_worker_unavailable())?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped test-adapter"))?
    }

    pub async fn refresh_session(&self, name: &str) -> Result<()> {
        let provider = self.login_provider(name)?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .read()
            .await
            .send(Command::RefreshSession {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| browser_worker_unavailable())?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped session refresh"))?
    }

    /// Clear provider cookies and page storage before deleting its saved session.
    pub async fn unlink_session(&self, name: &str) -> Result<bool> {
        let provider =
            Provider::from_name(name).ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .read()
            .await
            .send(Command::UnlinkSession {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| browser_worker_unavailable())?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped session unlink"))?
    }

    fn login_provider(&self, name: &str) -> Result<Provider> {
        let provider =
            Provider::from_name(name).ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
        match self.cfg.provider(name) {
            Some(p) if p.enabled => Ok(provider),
            _ => Err(anyhow::anyhow!("provider {name} not enabled")),
        }
    }

    /// Login UI: open a dedicated login page for a provider (fresh page,
    /// navigated to the provider URL; any previous login page for the
    /// provider is dropped). Returns the page's URL.
    pub async fn login_start(&self, name: &str) -> Result<String> {
        let provider = self.login_provider(name)?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .read()
            .await
            .send(Command::LoginStart {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| browser_worker_unavailable())?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped login start"))?
    }

    /// Login UI: drop the provider's login page (idempotent).
    pub async fn login_cancel(&self, name: &str) -> Result<()> {
        let provider = self.login_provider(name)?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .read()
            .await
            .send(Command::LoginCancel {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| browser_worker_unavailable())?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped login cancel"))?
    }

    /// Login UI: capture the login page's session to
    /// sessions/<provider>.json (existing format) and swap the login
    /// page in as the provider's warm page so the next send boots with
    /// the new session immediately.
    pub async fn login_complete(&self, name: &str) -> Result<LoginCompleted> {
        let provider = self.login_provider(name)?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .read()
            .await
            .send(Command::LoginComplete {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| browser_worker_unavailable())?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped login complete"))?
    }

    /// Login UI: live info for all providers with an active login page.
    pub async fn login_status_all(&self) -> Result<Vec<LoginActiveInfo>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .read()
            .await
            .send(Command::LoginStatusAll { reply: tx })
            .await
            .map_err(|_| browser_worker_unavailable())?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped login status"))
    }

    /// Login UI: full live view of one provider's login page.
    pub async fn login_view(&self, name: &str) -> Result<LoginView> {
        let provider = self.login_provider(name)?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .read()
            .await
            .send(Command::LoginView {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| browser_worker_unavailable())?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped login view"))?
    }

    /// Login UI: perform one relay action on a provider's login page.
    /// The action's text (if any) is never logged — see login.rs.
    pub async fn login_act(&self, name: &str, action: LoginAction) -> Result<String> {
        action.validate()?;
        let provider = self.login_provider(name)?;
        if let LoginAction::Navigate { url } = &action {
            login::validate_navigation_url_for_provider(name, url)?;
        }
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .read()
            .await
            .send(Command::LoginAct {
                provider,
                action,
                reply: tx,
            })
            .await
            .map_err(|_| browser_worker_unavailable())?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped login act"))?
    }

    /// Stop the browser worker after HTTP requests have drained.
    pub async fn shutdown(&self) -> Result<()> {
        self.shutting_down.store(true, Ordering::Release);
        self.worker_ready.store(false, Ordering::Release);
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .read()
            .await
            .send(Command::Shutdown { reply: tx })
            .await
            .map_err(|_| anyhow::anyhow!("browser thread gone during shutdown"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped shutdown reply"))?;
        Ok(())
    }
}

struct WorkerReadyGuard(Arc<AtomicBool>);

impl Drop for WorkerReadyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
