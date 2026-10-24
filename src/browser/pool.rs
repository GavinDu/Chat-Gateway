use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use dashmap::DashMap;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use anyhow::Result;
use crate::config::AppConfig;
use crate::providers::{self, Provider, ProviderChunk};
use crate::session;

/// ObscuraPool - one persistent, pre-warmed page per provider
/// (DashMap-indexed by provider name on the API side).
///
/// The embedded obscura engine is thread-affine (each Page wraps a V8
/// runtime behind a RefCell and is neither Clone nor Send), so all pages
/// live on a dedicated browser thread running a current-thread runtime
/// + LocalSet. The HTTP side talks to it over channels; per-provider
/// semaphores (this side) bound concurrent sends, and a per-provider
/// async mutex (browser side) serializes access to its single page.
/// Provider switch is therefore a channel send + map lookup, not a
/// process/tab spawn.
pub struct ObscuraPool {
    cmd_tx: mpsc::Sender<Command>,
    ready: tokio::sync::Mutex<Option<oneshot::Receiver<Result<()>>>>,
    semaphores: DashMap<String, Arc<Semaphore>>,
    cfg: Arc<AppConfig>,
}

enum Command {
    Discover { provider: Provider, reply: oneshot::Sender<Result<Vec<String>>> },
    Send { provider: Provider, prompt: String, model: String, has_history: bool, timeout: Duration, chunks: mpsc::Sender<Result<ProviderChunk>> },
    SaveSession { provider: Provider, reply: oneshot::Sender<Result<()>> },
    TestAdapter { provider: Provider, reply: oneshot::Sender<Result<TestAdapterReport>> },
}

/// Live-send test-adapter outcome, phase by phase (Go RunAdapterTest
/// shape, adapted to the restored tree: probe uses the full Rust v1
/// selector arrays, and the live send uses the restored send path).
#[derive(Default, Debug)]
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
                semaphores.insert(p.name.clone(), Arc::new(Semaphore::new(p.session_pool_size.max(1))));
            }
        }
        let pool = Arc::new(Self {
            cmd_tx,
            ready: tokio::sync::Mutex::new(Some(ready_rx)),
            semaphores,
            cfg: cfg.clone(),
        });
        browser_thread(cfg, cmd_rx, ready_tx);
        Ok(pool)
    }

    /// Await browser build + per-provider warm-up (v2 ordering: session
    /// cookies restored into the browser store BEFORE the first goto,
    /// localStorage after goto, then a reload-equivalent second goto).
    pub async fn warm_all(&self) -> Result<()> {
        let mut slot = self.ready.lock().await;
        if let Some(rx) = slot.take() {
            rx.await.map_err(|_| anyhow::anyhow!("browser thread died during warm-up"))??;
        }
        Ok(())
    }

    /// Bound concurrent sends per provider (v1 pool semantics).
    pub async fn acquire(&self, provider: &str) -> Result<ObscuraPermit> {
        let start = std::time::Instant::now();
        let sem = self.semaphores.get(provider).map(|v| v.clone())
            .unwrap_or_else(|| Arc::new(Semaphore::new(2)));
        let permit = sem.acquire_owned().await?;
        Ok(ObscuraPermit { _permit: permit, queue_ms: start.elapsed().as_millis() })
    }

    /// Discover a provider's models on a fresh page (no message sent).
    pub async fn discover(&self, name: &str) -> Result<Vec<String>> {
        let provider = Provider::from_name(name)
            .ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(Command::Discover { provider, reply: tx }).await
            .map_err(|_| anyhow::anyhow!("browser thread gone"))?;
        rx.await.map_err(|_| anyhow::anyhow!("browser thread dropped discover"))?
    }

    /// Send a prompt on the provider's warmed page; response chunks
    /// stream back over the returned channel receiver. `model` is the
    /// requested model (for UI selection on new conversations only)
    /// and `has_history` marks a returning conversation (selection
    /// skipped — the model is already fixed by the prior turns).
    pub async fn send_stream(&self, name: &str, prompt: String, model: String, has_history: bool) -> Result<mpsc::Receiver<Result<ProviderChunk>>> {
        let provider = Provider::from_name(name)
            .ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
        let timeout = Duration::from_secs(
            self.cfg.provider(name).map(|p| p.timeout_seconds).unwrap_or(120),
        );
        let (tx, rx) = mpsc::channel(32);
        self.cmd_tx.send(Command::Send { provider, prompt, model, has_history, timeout, chunks: tx }).await
            .map_err(|_| anyhow::anyhow!("browser thread gone"))?;
        Ok(rx)
    }

    /// Live-send test-adapter run for one provider (Go RunAdapterTest
    /// phases, restored-tree form): session check → selector probe →
    /// dummy prompt send → up to 45s response-token wait.
    pub async fn test_adapter(&self, name: &str) -> Result<TestAdapterReport> {
        let provider = Provider::from_name(name)
            .ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(Command::TestAdapter { provider, reply: tx }).await
            .map_err(|_| anyhow::anyhow!("browser thread gone"))?;
        rx.await.map_err(|_| anyhow::anyhow!("browser thread dropped test-adapter"))?
    }

    /// Persist the provider's current cookies + localStorage.
    pub async fn save_session(&self, name: &str) -> Result<()> {
        let provider = Provider::from_name(name)
            .ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(Command::SaveSession { provider, reply: tx }).await
            .map_err(|_| anyhow::anyhow!("browser thread gone"))?;
        rx.await.map_err(|_| anyhow::anyhow!("browser thread dropped save"))?
    }
}

fn build_browser(cfg: &AppConfig) -> Result<obscura_core::Browser> {
    let mut builder = obscura_core::Browser::builder().stealth(cfg.obscura.stealth);
    let storage = cfg.obscura.storage_dir.clone().filter(|s| !s.is_empty())
        .or_else(|| if cfg.server.chrome_profile_dir.is_empty() { None } else { Some(cfg.server.chrome_profile_dir.clone()) });
    if let Some(dir) = storage {
        builder = builder.storage_dir(dir);
    }
    Ok(builder.build()?)
}

fn browser_thread(
    cfg: Arc<AppConfig>,
    mut cmd_rx: mpsc::Receiver<Command>,
    ready_tx: oneshot::Sender<Result<()>>,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ready_tx.send(Err(e.into()));
                return;
            }
        };
        rt.block_on(async move {
            let local = tokio::task::LocalSet::new();
            local.run_until(async move {
                let browser = match build_browser(&cfg) {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let browser = Rc::new(browser);
                let registry = Rc::new(providers::build_registry(&cfg));
                let mut pages: HashMap<Provider, Rc<obscura_core::Page>> = HashMap::new();
                let mut locks: HashMap<Provider, Rc<tokio::sync::Mutex<()>>> = HashMap::new();

                // Pre-warm one persistent page per enabled provider
                for pcfg in &cfg.providers {
                    if !pcfg.enabled { continue; }
                    let Some(provider) = Provider::from_name(&pcfg.name) else { continue };
                    match browser.new_page().await {
                        Ok(mut page) => {
                            let session_file = session::load_session_file(&pcfg.name)
                                .ok().flatten();
                            if let Some(file) = &session_file {
                                if let Err(e) = session::restore_cookies(&browser, file) {
                                    tracing::warn!(provider = %pcfg.name, "cookie restore failed: {e}");
                                }
                            }
                            if let Err(e) = page.goto(provider.login_url()).await {
                                tracing::warn!(provider = %pcfg.name, "warm goto failed: {e}");
                            }
                            if let Some(file) = &session_file {
                                session::restore_local_storage(&page, file);
                                // Reload-equivalent so the app boots with the session applied
                                if let Err(e) = page.goto(provider.login_url()).await {
                                    tracing::warn!(provider = %pcfg.name, "warm re-goto failed: {e}");
                                }
                            }
                            pages.insert(provider, Rc::new(page));
                            locks.insert(provider, Rc::new(tokio::sync::Mutex::new(())));
                            tracing::info!(provider = %pcfg.name, url = provider.login_url(), "page warmed");
                        }
                        Err(e) => tracing::warn!(provider = %pcfg.name, "warm failed: {e}"),
                    }
                }
                tracing::info!("ObscuraPool warmed: {} pages", pages.len());
                let _ = ready_tx.send(Ok(()));

                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
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
                        Command::Send { provider, prompt, model, has_history, timeout, chunks } => {
                            let Some(page) = pages.get(&provider).cloned() else {
                                let _ = chunks.send(Err(anyhow::anyhow!("provider page not warmed"))).await;
                                continue;
                            };
                            let Some(lock) = locks.get(&provider).cloned() else { continue };
                            let Some(adapter) = registry.get(&provider).cloned() else {
                                let _ = chunks.send(Err(anyhow::anyhow!("provider not enabled"))).await;
                                continue;
                            };
                            tokio::task::spawn_local(async move {
                                let _guard = lock.lock().await;
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
                                providers::run_send(adapter, page, prompt, timeout, chunks).await;
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
                                if let Ok(Some(file)) = session::load_session_file(provider.name()) {
                                    let _ = session::restore_cookies(&browser, &file);
                                    if page.goto(provider.login_url()).await.is_err() {
                                        report.errors.push("page navigation failed".to_string());
                                        finish(report);
                                        return;
                                    }
                                    session::restore_local_storage(&page, &file);
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
                                }
                                finish(report);
                            });
                        }
                        Command::SaveSession { provider, reply } => {
                            let res = match pages.get(&provider) {
                                Some(page) => session::save_session(&browser, page, provider.name()).await,
                                None => Err(anyhow::anyhow!("provider page not warmed")),
                            };
                            let _ = reply.send(res);
                        }
                    }
                }
            }).await;
        });
    });
}
