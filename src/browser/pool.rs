use crate::config::AppConfig;
use crate::login::{self, LoginAction, LoginView};
use crate::providers::{self, Provider, ProviderChunk};
use crate::session;
use anyhow::Result;
use dashmap::DashMap;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, Notify, OwnedSemaphorePermit, Semaphore};

fn effective_session_pool_size(configured: usize) -> usize {
    configured.min(1)
}

const MAX_BROWSER_WORKER_RESTART_BACKOFF: Duration = Duration::from_secs(30);

fn join_finished_worker(worker: &mut Option<std::thread::JoinHandle<()>>) -> bool {
    let Some(handle) = worker.as_ref() else {
        return true;
    };
    if !handle.is_finished() {
        return false;
    }
    if let Some(handle) = worker.take() {
        if handle.join().is_err() {
            tracing::debug!("browser worker thread joined after panic");
        }
    }
    true
}

fn next_browser_worker_restart_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BROWSER_WORKER_RESTART_BACKOFF)
}

fn closed_command_channel() -> mpsc::Sender<Command> {
    let (sender, receiver) = mpsc::channel::<Command>(1);
    drop(receiver);
    sender
}

#[cfg(test)]
mod worker_supervisor_tests {
    use super::{
        closed_command_channel, join_finished_worker, next_browser_worker_restart_backoff,
        WorkerReadyGuard,
    };
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::Duration;

    #[test]
    fn finished_panicked_worker_is_joined_before_replacement() {
        let worker = std::thread::spawn(|| panic!("injected worker panic"));
        while !worker.is_finished() {
            std::thread::yield_now();
        }
        let mut worker = Some(worker);
        assert!(join_finished_worker(&mut worker));
        assert!(
            worker.is_none(),
            "the finished worker handle must be consumed"
        );
    }

    #[test]
    fn restart_backoff_doubles_and_is_capped() {
        assert_eq!(
            next_browser_worker_restart_backoff(Duration::from_secs(1)),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_browser_worker_restart_backoff(Duration::from_secs(16)),
            Duration::from_secs(30)
        );
        assert_eq!(
            next_browser_worker_restart_backoff(Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn closed_command_channel_is_observed_as_closed_by_supervisor() {
        assert!(closed_command_channel().is_closed());
    }

    #[test]
    fn worker_ready_guard_clears_liveness_when_worker_exits() {
        let ready = Arc::new(AtomicBool::new(true));
        drop(WorkerReadyGuard(ready.clone()));
        assert!(!ready.load(Ordering::Acquire));
    }

    #[test]
    fn injected_worker_panic_closes_command_channel_and_clears_readiness() {
        let ready = Arc::new(AtomicBool::new(true));
        let (command_tx, command_rx) = tokio::sync::mpsc::channel::<()>(1);
        let worker_ready = ready.clone();
        let worker = std::thread::spawn(move || {
            let _ready_guard = WorkerReadyGuard(worker_ready);
            let _commands = command_rx;
            panic!("injected browser-worker panic for supervisor regression test");
        });

        assert!(worker.join().is_err(), "the test worker should panic");
        assert!(
            command_tx.is_closed(),
            "a panicked worker must close its command receiver so the supervisor can detect death"
        );
        assert!(
            !ready.load(Ordering::Acquire),
            "worker readiness must be cleared during panic unwinding"
        );
    }
}

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

/// Bookkeeping for one active login page. last_seen drives the
/// abandoned-login sweep (Go swept after 35s of no client contact for
/// waiting jobs; here the UI polls every 5-8s, so a 30-minute idle
/// threshold fits a user working through a slow SSO flow).
struct LoginMeta {
    last_seen: Instant,
}

const LOGIN_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

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
    pub errors: Vec<String>,
}

pub struct ObscuraPermit {
    _permit: OwnedSemaphorePermit,
    pub queue_ms: u128,
}

impl ObscuraPool {
    /// Spawn the browser thread (browser build + page warm-up happen
    /// there; call warm_all() to await readiness).
    pub async fn new(cfg: Arc<AppConfig>) -> Result<Arc<Self>> {
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
        });
        let mut worker_handle = Some(browser_thread(
            cfg.clone(),
            cmd_rx,
            ready_tx,
            worker_ready.clone(),
        ));

        // Recreate the thread-affine engine after a crash; never move Page values
        // between threads. The worker performs the same browser build and warm-up
        // sequence before it is advertised as ready again.
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if shutting_down.load(Ordering::Acquire) {
                    return;
                }
                if !cmd_tx.read().await.is_closed() {
                    continue;
                }
                worker_ready.store(false, Ordering::Release);
                tokio::time::sleep(backoff).await;
                if shutting_down.load(Ordering::Acquire) {
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
                worker_handle = Some(browser_thread(
                    cfg.clone(),
                    next_rx,
                    next_ready_tx,
                    worker_ready.clone(),
                ));
                match tokio::time::timeout(Duration::from_secs(90), next_ready_rx).await {
                    Ok(Ok(Ok(()))) => {
                        worker_ready.store(true, Ordering::Release);
                        worker_generation.fetch_add(1, Ordering::AcqRel);
                        crate::metrics::record_browser_worker_restart();
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

/// Drop login pages idle longer than LOGIN_IDLE_TIMEOUT (Go's
/// abandoned-job sweep, with a threshold fitted to the UI's 5-8s
/// polling instead of Go's 35s — a user mid-SSO must not lose the page
/// between polls).
fn sweep_login_pages(
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

fn touch_login_meta(login_meta: &Rc<RefCell<HashMap<Provider, LoginMeta>>>, provider: Provider) {
    if let Some(m) = login_meta.borrow_mut().get_mut(&provider) {
        m.last_seen = Instant::now();
    }
}

/// Typed outcome for a probe that confirms the provider has redirected to login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderSessionExpiredError;

impl std::fmt::Display for ProviderSessionExpiredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("provider session requires login")
    }
}

impl std::error::Error for ProviderSessionExpiredError {}

/// Verify that a warmed provider page still looks like an authenticated chat page.
/// Call only while holding that provider page's lock so probes never race a send.
fn verify_session_page(
    _provider: Provider,
    page: &obscura_core::Page,
    adapter: &dyn providers::SiteAdapter,
) -> Result<bool> {
    let is_login_page = providers::common::is_login_page(page);
    let input_found = adapter
        .selector_set()
        .input
        .iter()
        .any(|selector| page.query_selector(selector).is_some());
    classify_session_probe(is_login_page, input_found)
}

fn classify_session_probe(is_login_page: bool, input_found: bool) -> Result<bool> {
    if session_probe_requires_login(is_login_page, input_found) {
        return Err(anyhow::Error::new(ProviderSessionExpiredError));
    }
    if !session_probe_page_healthy(is_login_page, input_found) {
        anyhow::bail!("session verification failed: chat input not found");
    }
    Ok(true)
}

#[cfg(test)]
mod session_probe_tests {
    #[test]
    fn probe_distinguishes_expired_login_page_from_unwarmed_page() {
        let expired = super::classify_session_probe(true, true).unwrap_err();
        assert!(expired
            .downcast_ref::<super::ProviderSessionExpiredError>()
            .is_some());
        assert!(super::classify_session_probe(false, false).is_err());
        assert!(matches!(
            super::classify_session_probe(false, true),
            Ok(true)
        ));
    }

    #[test]
    fn probe_requires_chat_input_and_not_a_login_page() {
        assert!(super::session_probe_page_healthy(false, true));
        assert!(!super::session_probe_page_healthy(true, true));
        assert!(!super::session_probe_page_healthy(false, false));
    }
}

fn session_probe_requires_login(is_login_page: bool, input_found: bool) -> bool {
    login::login_state(is_login_page, input_found).0 == "login_required"
}

fn session_probe_page_healthy(is_login_page: bool, input_found: bool) -> bool {
    let (_, logged_in) = login::login_state(is_login_page, input_found);
    logged_in == Some(true)
}

fn build_login_view(
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
    let (state, logged_in) = login::login_state(is_login, input_found);
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

fn login_active_info(
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

fn build_browser(cfg: &AppConfig) -> Result<obscura_core::Browser> {
    let mut builder = obscura_core::Browser::builder().stealth(cfg.obscura.stealth);
    if let Some(user_agent) = cfg
        .obscura
        .user_agent
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        builder = builder.user_agent(user_agent.to_owned());
    }
    let storage = cfg
        .obscura
        .storage_dir
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if cfg.server.chrome_profile_dir.is_empty() {
                None
            } else {
                Some(cfg.server.chrome_profile_dir.clone())
            }
        });
    if let Some(dir) = storage {
        builder = builder.storage_dir(dir);
    }
    Ok(builder.build()?)
}

struct WorkerReadyGuard(Arc<AtomicBool>);

impl Drop for WorkerReadyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn browser_thread(
    cfg: Arc<AppConfig>,
    mut cmd_rx: mpsc::Receiver<Command>,
    ready_tx: oneshot::Sender<Result<()>>,
    worker_ready: Arc<AtomicBool>,
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
                let pages: Rc<RefCell<HashMap<Provider, Rc<obscura_core::Page>>>> =
                    Rc::new(RefCell::new(HashMap::new()));
                let locks: Rc<RefCell<HashMap<Provider, Rc<tokio::sync::Mutex<()>>>>> =
                    Rc::new(RefCell::new(HashMap::new()));
                // Login delegate UI state (see src/login.rs).
                let login_pages: Rc<RefCell<HashMap<Provider, Rc<obscura_core::Page>>>> =
                    Rc::new(RefCell::new(HashMap::new()));
                let login_meta: Rc<RefCell<HashMap<Provider, LoginMeta>>> =
                    Rc::new(RefCell::new(HashMap::new()));

                // Pre-warm one persistent page per enabled provider
                for pcfg in &cfg.providers {
                    if !pcfg.enabled { continue; }
                    let Some(provider) = Provider::from_name(&pcfg.name) else { continue };
                    match browser.new_page().await {
                        Ok(mut page) => {
                            let session_file = match session::load_session_file(&pcfg.name) {
                                Ok(file) => file,
                                Err(e) => {
                                    tracing::warn!(provider = %pcfg.name, "session file could not be loaded; continuing without restored session: {e}");
                                    None
                                }
                            };
                            if let Some(file) = &session_file {
                                if let Err(e) = session::restore_cookies(&browser, file, &pcfg.name) {
                                    tracing::warn!(provider = %pcfg.name, "cookie restore failed; provider may require login: {e}");
                                }
                            }
                            if let Err(e) = page.goto(provider.login_url()).await {
                                tracing::warn!(provider = %pcfg.name, "warm goto failed: {e}");
                            }
                            if let Some(file) = &session_file {
                                if let Err(error) = session::restore_local_storage(&page, file) {
                                    tracing::warn!(provider = %pcfg.name, "localStorage restore failed: {error}");
                                }
                                // Reload-equivalent so the app boots with the session applied
                                if let Err(e) = page.goto(provider.login_url()).await {
                                    tracing::warn!(provider = %pcfg.name, "warm re-goto failed: {e}");
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

                // Probe each warmed provider page independently. A small stable
                // per-provider offset avoids synchronized bursts across providers.
                for provider in pages.borrow().keys().copied().collect::<Vec<_>>() {
                    let Some(page) = pages.borrow().get(&provider).cloned() else { continue };
                    let Some(lock) = locks.borrow().get(&provider).cloned() else { continue };
                    let Some(adapter) = registry.get(&provider).cloned() else { continue };
                    tokio::task::spawn_local(async move {
                        let jitter = provider.name().bytes().map(u64::from).sum::<u64>() % 21;
                        tokio::time::sleep(Duration::from_secs(30 + jitter)).await;
                        loop {
                            {
                                let _guard = lock.lock().await;
                                match verify_session_page(provider, &page, adapter.as_ref()) {
                                    Ok(true) => crate::provider_state::record_success(provider.name()),
                                    Ok(false) => crate::provider_state::record_needs_login(provider.name()),
                                    Err(_) => crate::provider_state::record_probe_failure(provider.name()),
                                }
                            }
                            tokio::time::sleep(Duration::from_secs(60 + jitter)).await;
                        }
                    });
                }

                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        Command::Shutdown { reply } => {
                            let _ = reply.send(());
                            break;
                        }
                        Command::Discover { provider, reply } => {
                            let browser = browser.clone();
                            let registry = registry.clone();
                            tokio::task::spawn_local(async move {
                                let res: Result<Vec<String>> = async {
                                    let adapter = registry.get(&provider).cloned()
                                        .ok_or_else(|| anyhow::anyhow!("provider not enabled"))?;
                                    let mut page = browser.new_page().await?;
                                    adapter.discover_models(&mut page).await
                                }.await;
                                let _ = reply.send(res);
                            });
                        }
                        Command::Send { provider, prompt, model, has_history, thread_url, timeout, completion_idle, chunks } => {
                            let Some(page) = pages.borrow().get(&provider).cloned() else {
                                let _ = chunks.send(Err(anyhow::anyhow!("provider page not warmed"))).await;
                                continue;
                            };
                            let Some(lock) = locks.borrow().get(&provider).cloned() else { continue };
                            let Some(adapter) = registry.get(&provider).cloned() else {
                                let _ = chunks.send(Err(anyhow::anyhow!("provider not enabled"))).await;
                                continue;
                            };
                            let browser_for_save = browser.clone();
                            let provider_name = provider.name().to_string();
                            let poll_tick = gateway_poll_tick;
                            tokio::task::spawn_local(async move {
                                let _guard = lock.lock().await;
                                // Reopen only a validated provider-owned thread. Otherwise return
                                // to the provider landing page to avoid cross-conversation reuse.
                                // Navigating the current page avoids leaking one tab per conversation.
                                let target_url = thread_url.as_deref().unwrap_or_else(|| provider.login_url());
                                if page.url() != target_url {
                                    let target_literal = serde_json::to_string(target_url)
                                        .expect("provider URL serialization cannot fail");
                                    let _ = page.evaluate(&format!("window.location.href = {target_literal}; true"));
                                    let navigation = tokio::time::timeout(Duration::from_secs(15), async {
                                        loop {
                                            if page.url() == target_url {
                                                break;
                                            }
                                            tokio::time::sleep(Duration::from_millis(50)).await;
                                        }
                                    }).await;
                                    if navigation.is_err() {
                                        let _ = chunks.send(Err(anyhow::anyhow!("provider thread navigation timed out"))).await;
                                        return;
                                    }
                                }
                                // Login/session-expiry detection (Go concept,
                                // Rust form — Rust v1 had none): fail with a
                                // distinct session_expired error so the API
                                // can report it and the router can skip
                                // this provider.
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
                                    if let Err(e) = adapter.select_model(&page, &model).await {
                                        let _ = chunks.send(Err(e)).await;
                                        return;
                                    }
                                }

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
                                let page_for_save = page.clone();
                                tokio::select! {
                                    _ = cancel_send.notified() => return,
                                    _ = providers::run_send(
                                        adapter,
                                        page,
                                        prompt,
                                        timeout,
                                        poll_tick,
                                        completion_idle,
                                        save_tx,
                                    ) => {}
                                }
                                // Save only after the send loop has finished and while
                                // this provider's page lock is still held. A cancelled send
                                // returns above without persisting a partial session.
                                if completion_rx.await.unwrap_or(false) {
                                    if let Err(error) = session::save_session_if_changed(
                                        &browser_for_save,
                                        &page_for_save,
                                        &provider_name,
                                    ).await {
                                        tracing::warn!(provider = %provider_name, %error, "rotated session snapshot failed");
                                    }
                                }
                            });
                        }
                        Command::CurrentUrl { provider, reply } => {
                            let result = pages.borrow().get(&provider)
                                .map(|page| page.url())
                                .ok_or_else(|| anyhow::anyhow!("provider page not warmed"));
                            let _ = reply.send(result);
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
                                    verify_session_page(provider, &page, adapter.as_ref())?;
                                    Ok(true)
                                }.await;
                                let _ = reply.send(result);
                            });
                        }
                        Command::TestAdapter { provider, reply } => {
                            let browser = browser.clone();
                            let registry = registry.clone();
                            tokio::task::spawn_local(async move {
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
                                let mut page = match browser.new_page().await {
                                    Ok(p) => p,
                                    Err(e) => {
                                        report.errors.push(format!("open page: {e}"));
                                        finish(report);
                                        return;
                                    }
                                };
                                let session_file = match session::load_session_file(provider.name()) {
                                    Ok(file) => file,
                                    Err(error) => {
                                        report.errors.push(format!("session file load failed: {error}"));
                                        None
                                    }
                                };
                                if let Some(file) = session_file {
                                    if let Err(error) = session::restore_cookies(&browser, &file, provider.name()) {
                                        report.errors.push(format!("session cookie restore failed: {error}"));
                                    }
                                    if page.goto(provider.login_url()).await.is_err() {
                                        report.errors.push("page navigation failed".to_string());
                                        finish(report);
                                        return;
                                    }
                                    if let Err(error) = session::restore_local_storage(&page, &file) {
                                        report.errors.push(format!("session localStorage restore failed: {error}"));
                                    }
                                    if page.goto(provider.login_url()).await.is_err() {
                                        report.errors.push("page navigation failed".to_string());
                                        finish(report);
                                        return;
                                    }
                                } else if page.goto(provider.login_url()).await.is_err() {
                                    report.errors.push("page navigation failed".to_string());
                                    finish(report);
                                    return;
                                }
                                tokio::time::sleep(Duration::from_millis(1200)).await;
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
                                let set = adapter.selector_set();
                                let sets: [(&str, &[&str]); 3] = [
                                    ("input", set.input),
                                    ("send", set.send),
                                    ("response", set.response),
                                ];
                                report.probe = providers::common::probe_selector_groups(&page, &sets);
                                // Phase 1 — input (20s, Go's budget).
                                let input_sel = match providers::common::wait_for_selector(&page, sets[0].1, 20000).await {
                                    Ok(()) => sets[0].1.iter().copied().find(|s| page.query_selector(s).is_some()).unwrap_or(sets[0].1[0]),
                                    Err(e) => {
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
                                tokio::time::sleep(Duration::from_millis(400)).await;
                                let baseline = adapter.response_text(&page).filter(|t| !t.is_empty());
                                // Phase 3 — send button or Enter (5s).
                                match providers::common::click_send_or_enter(&page, sets[1].1, input_sel, 5000).await {
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
                                tokio::time::sleep(Duration::from_millis(1500)).await;
                                let deadline = std::time::Instant::now() + Duration::from_secs(45);
                                while std::time::Instant::now() < deadline {
                                    if let Some(t) = adapter.response_text(&page) {
                                        if !t.is_empty() && Some(&t) != baseline.as_ref() {
                                            report.response_token = true;
                                            break;
                                        }
                                    }
                                    tokio::time::sleep(Duration::from_millis(300)).await;
                                }
                                if !report.response_token {
                                    report.errors.push("response: no token within 45s".to_string());
                                } else if let Err(error) =
                                    session::save_session_if_changed(&browser, &page, provider.name()).await
                                {
                                    tracing::warn!(provider = provider.name(), %error, "probe session snapshot failed");
                                }
                                finish(report);
                            });
                        }
                        Command::RefreshSession { provider, reply } => {
                            let browser = browser.clone();
                            let pages = pages.clone();
                            let locks = locks.clone();
                            tokio::task::spawn_local(async move {
                                let res: Result<()> = async {
                                    let lock = locks.borrow().get(&provider).cloned()
                                        .ok_or_else(|| anyhow::anyhow!("provider page lock missing"))?;
                                    let _guard = lock.lock().await;
                                    let mut page = browser.new_page().await?;
                                    page.goto(provider.login_url()).await?;
                                    let storage_cleared = page.evaluate(
                                        "(()=>{try{localStorage.clear();return true;}catch(_){return false;}})()"
                                    );
                                    if !storage_cleared.as_bool().unwrap_or(false) {
                                        anyhow::bail!("provider localStorage could not be cleared");
                                    }
                                    session::clear_provider_cookies(&browser, provider.name())?;
                                    let file = session::load_session_file(provider.name())?
                                        .ok_or_else(|| anyhow::anyhow!("imported provider session is missing"))?;
                                    session::restore_cookies(&browser, &file, provider.name())?;
                                    page.goto(provider.login_url()).await?;
                                    session::restore_local_storage(&page, &file)?;
                                    page.goto(provider.login_url()).await?;
                                    pages.borrow_mut().insert(provider, Rc::new(page));
                                    Ok(())
                                }.await;
                                let _ = reply.send(res);
                            });
                        }
                        Command::UnlinkSession { provider, reply } => {
                            let browser = browser.clone();
                            let pages = pages.clone();
                            let locks = locks.clone();
                            let login_pages = login_pages.clone();
                            let login_meta = login_meta.clone();
                            tokio::task::spawn_local(async move {
                                let res: Result<bool> = async {
                                    let lock = locks.borrow().get(&provider).cloned()
                                        .unwrap_or_else(|| Rc::new(tokio::sync::Mutex::new(())));
                                    let _guard = lock.lock().await;
                                    let mut page = browser.new_page().await?;
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
                            tokio::task::spawn_local(async move {
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
                                    let mut page = browser.new_page().await?;
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
                            let pages = pages.clone();
                            let locks = locks.clone();
                            let login_pages = login_pages.clone();
                            let login_meta = login_meta.clone();
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
                                    // Same format/path as every other save:
                                    // sessions/<provider>.json via
                                    // session::save_session (cookies from
                                    // the browser-wide CookieStore +
                                    // localStorage from this page).
                                    session::save_session(&browser, &login_page, provider.name()).await?;
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
                                            let mut page = browser.new_page().await?;
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
