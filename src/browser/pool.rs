use crate::config::AppConfig;
use crate::login::{self, LoginAction, LoginView};
use crate::providers::{self, Provider, ProviderChunk};
use crate::session;
use anyhow::Result;
use dashmap::DashMap;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};

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
    Discover {
        provider: Provider,
        reply: oneshot::Sender<Result<Vec<String>>>,
    },
    Send {
        provider: Provider,
        prompt: String,
        model: String,
        has_history: bool,
        timeout: Duration,
        chunks: mpsc::Sender<Result<ProviderChunk>>,
    },
    SaveSession {
        provider: Provider,
        reply: oneshot::Sender<Result<()>>,
    },
    TestAdapter {
        provider: Provider,
        reply: oneshot::Sender<Result<TestAdapterReport>>,
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
                semaphores.insert(
                    p.name.clone(),
                    Arc::new(Semaphore::new(p.session_pool_size.max(1))),
                );
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
            rx.await
                .map_err(|_| anyhow::anyhow!("browser thread died during warm-up"))??;
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
        let permit = sem.acquire_owned().await?;
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
            .send(Command::Discover {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("browser thread gone"))?;
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
    ) -> Result<mpsc::Receiver<Result<ProviderChunk>>> {
        let provider =
            Provider::from_name(name).ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
        let timeout = Duration::from_secs(
            self.cfg
                .provider(name)
                .map(|p| p.timeout_seconds)
                .unwrap_or(120),
        );
        let (tx, rx) = mpsc::channel(32);
        self.cmd_tx
            .send(Command::Send {
                provider,
                prompt,
                model,
                has_history,
                timeout,
                chunks: tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("browser thread gone"))?;
        Ok(rx)
    }

    /// Live-send test-adapter run for one provider (Go RunAdapterTest
    /// phases, restored-tree form): session check → selector probe →
    /// dummy prompt send → up to 45s response-token wait.
    pub async fn test_adapter(&self, name: &str) -> Result<TestAdapterReport> {
        let provider =
            Provider::from_name(name).ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::TestAdapter {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("browser thread gone"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped test-adapter"))?
    }

    /// Persist the provider's current cookies + localStorage.
    pub async fn save_session(&self, name: &str) -> Result<()> {
        let provider =
            Provider::from_name(name).ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::SaveSession {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("browser thread gone"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped save"))?
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
            .send(Command::LoginStart {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("browser thread gone"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped login start"))?
    }

    /// Login UI: drop the provider's login page (idempotent).
    pub async fn login_cancel(&self, name: &str) -> Result<()> {
        let provider = self.login_provider(name)?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::LoginCancel {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("browser thread gone"))?;
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
            .send(Command::LoginComplete {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("browser thread gone"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped login complete"))?
    }

    /// Login UI: live info for all providers with an active login page.
    pub async fn login_status_all(&self) -> Result<Vec<LoginActiveInfo>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::LoginStatusAll { reply: tx })
            .await
            .map_err(|_| anyhow::anyhow!("browser thread gone"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped login status"))
    }

    /// Login UI: full live view of one provider's login page.
    pub async fn login_view(&self, name: &str) -> Result<LoginView> {
        let provider = self.login_provider(name)?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::LoginView {
                provider,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("browser thread gone"))?;
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
            .send(Command::LoginAct {
                provider,
                action,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("browser thread gone"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("browser thread dropped login act"))?
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

fn build_login_view(
    provider: Provider,
    page: &obscura_core::Page,
    registry: &Rc<HashMap<Provider, Arc<dyn providers::ChatProvider>>>,
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
    registry: &Rc<HashMap<Provider, Arc<dyn providers::ChatProvider>>>,
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

fn browser_thread(
    cfg: Arc<AppConfig>,
    mut cmd_rx: mpsc::Receiver<Command>,
    ready_tx: oneshot::Sender<Result<()>>,
) {
    std::thread::spawn(move || {
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
                                session::restore_local_storage(&page, file);
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
                            let Some(page) = pages.borrow().get(&provider).cloned() else {
                                let _ = chunks.send(Err(anyhow::anyhow!("provider page not warmed"))).await;
                                continue;
                            };
                            let Some(lock) = locks.borrow().get(&provider).cloned() else { continue };
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
                            let res = match pages.borrow().get(&provider).cloned() {
                                Some(page) => session::save_session(&browser, &page, provider.name()).await,
                                None => Err(anyhow::anyhow!("provider page not warmed")),
                            };
                            let _ = reply.send(res);
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
    });
}
