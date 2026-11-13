use axum::{
    extract::{Form, Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::{Html, IntoResponse, Response},
    Json,
};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, OpenOptions},
    io::Write,
    path::Path as FsPath,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

const PAIRING_LIFETIME: Duration = Duration::from_secs(10 * 60);
const REQUEST_SKEW_SECONDS: i64 = 60;
const NONCE_RETENTION: Duration = Duration::from_secs(5 * 60);
const MAX_PAIR_ATTEMPTS: u8 = 5;
const MAX_RELAY_REQUESTS_PER_MINUTE: usize = 120;
const RELAY_SESSION_ACTIVATION_TIMEOUT: Duration = Duration::from_secs(30);
// Decrypting status presence is read-only, unlike session import. Bound the
// dashboard wait without cancelling or replaying credential mutations.
const RELAY_STATUS_SCAN_TIMEOUT: Duration = Duration::from_secs(5);
// Do not let an older relay import overwrite a newer provider snapshot while
// another request is activating. A timed-out waiter has not touched the vault.
const RELAY_IMPORT_QUEUE_TIMEOUT: Duration = Duration::from_secs(45);

/// Bound the response wait, not the underlying browser worker operation.
/// A caller that times out must keep the spawned refresh handle alive until
/// the worker finishes, so a later import/unlink cannot overtake it.
async fn bounded_session_activation<T>(
    wait: Duration,
    activation: impl std::future::Future<Output = T>,
) -> Result<T, tokio::time::error::Elapsed> {
    tokio::time::timeout(wait, activation).await
}

struct PendingApproval {
    id: String,
    csrf: String,
    poll_secret: String,
    extension_id: String,
    expires: Instant,
    approved: bool,
}

fn approval_navigation_is_allowed(headers: &HeaderMap) -> bool {
    headers.get("sec-fetch-mode").and_then(|v| v.to_str().ok()) == Some("navigate")
        && headers.get("sec-fetch-dest").and_then(|v| v.to_str().ok()) == Some("document")
}

/// Serialize import persistence/activation and unlink revocation for one
/// registered provider without blocking unrelated providers.
#[derive(Default)]
struct ProviderImportGates {
    gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ProviderImportGates {
    fn for_provider(&self, provider: &str) -> Option<Arc<tokio::sync::Mutex<()>>> {
        let mut gates = self.gates.lock().ok()?;
        Some(Arc::clone(
            gates
                .entry(provider.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        ))
    }
}

/// Keep the provider's serialization guard alive independently of the HTTP
/// response future. Cancelling the caller must not unlock an already-started
/// vault write, browser activation, or revocation running in the worker.
async fn run_provider_mutation_detached<T: Send + 'static>(
    guard: tokio::sync::OwnedMutexGuard<()>,
    operation: impl std::future::Future<Output = T> + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    tokio::spawn(async move {
        let _provider_mutation = guard;
        operation.await
    })
    .await
}

/// Return a bounded import response while retaining the serialization gate
/// for a refresh that is still running in the browser worker after timeout.
/// Unlike dropping a timed-out oneshot receiver, the deferred join keeps the
/// worker reply alive and only unlocks once the actual refresh has finished.
async fn run_provider_mutation_detached_with_tail<T: Send + 'static, U: Send + 'static>(
    guard: tokio::sync::OwnedMutexGuard<()>,
    operation: impl std::future::Future<Output = (T, Option<tokio::task::JoinHandle<U>>)>
        + Send
        + 'static,
    record_tail: impl FnOnce(u64) + Send + 'static,
) -> Result<T, tokio::sync::oneshot::error::RecvError> {
    let (reply, receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _provider_mutation = guard;
        let (response, pending_refresh) = operation.await;
        let _ = reply.send(response);
        if let Some(refresh) = pending_refresh {
            // The HTTP caller was already sent its timeout response. The
            // browser worker can still occupy the per-provider gate here;
            // measure that hidden serialization tail after it actually ends.
            let tail_started = Instant::now();
            let _ = refresh.await;
            record_tail(tail_started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        }
    });
    receiver.await
}

#[cfg(test)]
mod provider_import_gate_tests {
    use super::{
        bounded_session_activation, run_provider_mutation_detached,
        run_provider_mutation_detached_with_tail, ProviderImportGates,
    };
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn same_provider_serializes_while_another_provider_is_independent() {
        let gates = ProviderImportGates::default();
        let first = gates.for_provider("chatgpt").unwrap();
        let second = gates.for_provider("chatgpt").unwrap();
        let gemini = gates.for_provider("gemini").unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &gemini));

        let held = first.lock().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), second.lock())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), gemini.lock())
                .await
                .is_ok()
        );
        drop(held);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), second.lock())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn unlink_waits_for_same_provider_import_before_revoking() {
        let gates = ProviderImportGates::default();
        let import_gate = gates.for_provider("gemini").unwrap();
        let unlink_gate = gates.for_provider("gemini").unwrap();
        let other_provider_gate = gates.for_provider("chatgpt").unwrap();
        let state = Arc::new(tokio::sync::Mutex::new(false));
        let import_guard = import_gate.lock().await;
        *state.lock().await = true;

        let revoked = Arc::clone(&state);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = entered_tx.send(());
            let _guard = unlink_gate.lock().await;
            *revoked.lock().await = false;
        });
        entered_rx.await.unwrap();
        // The unrelated provider remains independently available.
        assert!(other_provider_gate.try_lock().is_ok());
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !task.is_finished(),
            "unlink must wait for import completion"
        );
        assert!(*state.lock().await, "unlink must not run during import");
        drop(import_guard);
        tokio::time::timeout(Duration::from_millis(200), task)
            .await
            .expect("revocation should proceed after import")
            .unwrap();
        assert!(!*state.lock().await, "unlink must leave session revoked");
    }

    #[tokio::test]
    async fn cancelled_http_waiter_keeps_gate_until_detached_mutation_finishes() {
        let gates = ProviderImportGates::default();
        let gate = gates.for_provider("gemini").unwrap();
        let owned = Arc::clone(&gate).lock_owned().await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let request = tokio::spawn(run_provider_mutation_detached(owned, async move {
            let _ = started_tx.send(());
            let _ = release_rx.await;
        }));
        started_rx.await.unwrap();
        // Simulate disconnecting the HTTP request after the mutation starts.
        request.abort();
        let _ = request.await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), gate.lock())
                .await
                .is_err(),
            "disconnect must not unlock an in-flight mutation"
        );
        release_tx.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), gate.lock())
                .await
                .is_ok(),
            "the mutation must eventually release its provider gate"
        );
    }

    #[tokio::test]
    async fn activation_timeout_returns_but_retains_gate_until_worker_finishes() {
        let gates = ProviderImportGates::default();
        let gate = gates.for_provider("gemini").unwrap();
        let owned = Arc::clone(&gate).lock_owned().await;
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let metrics = Arc::new(crate::metrics::MetricsRegistry::default());
        let tail_metrics = Arc::clone(&metrics);
        let response = run_provider_mutation_detached_with_tail(
            owned,
            async move {
                let mut refresh = tokio::spawn(async move {
                    let _ = release_rx.await;
                });
                assert!(
                    bounded_session_activation(Duration::from_millis(10), &mut refresh)
                        .await
                        .is_err(),
                    "the response must time out before the worker completes"
                );
                ("session_activation_timeout", Some(refresh))
            },
            move |elapsed_ms| {
                tail_metrics.record_relay_import_phase("activation_tail", elapsed_ms);
            },
        )
        .await
        .unwrap();
        assert_eq!(response, "session_activation_timeout");
        assert!(
            tokio::time::timeout(Duration::from_millis(20), gate.lock())
                .await
                .is_err(),
            "a timed-out refresh must still block later provider mutations"
        );
        assert!(
            !metrics
                .render_with_provider_states(&[])
                .contains("gateway_relay_session_import_phase_ms_count{phase=\"activation_tail\"}"),
            "no post-timeout tail is recorded before worker completion"
        );
        release_tx.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), gate.lock())
                .await
                .is_ok(),
            "the provider gate must release after actual worker completion"
        );
        let rendered = metrics.render_with_provider_states(&[]);
        assert!(
            rendered.contains(
                "gateway_relay_session_import_phase_ms_count{phase=\"activation_tail\"} 1"
            ),
            "the hidden refresh tail must be recorded once"
        );
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_poison_provider_gate() {
        let gates = ProviderImportGates::default();
        let gate = gates.for_provider("gemini").unwrap();
        let held = gate.lock().await;
        assert!(tokio::time::timeout(Duration::from_millis(20), gate.lock())
            .await
            .is_err());
        drop(held);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), gate.lock())
                .await
                .is_ok()
        );
    }
}

pub struct RelayState {
    approval: Mutex<Option<PendingApproval>>,
    pairing_hash: Mutex<[u8; 32]>,
    pairing_expires: Mutex<Instant>,
    pairing_attempts: Mutex<u8>,
    install_token_hash: Mutex<Option<[u8; 32]>>,
    used_nonces: Mutex<HashMap<String, Instant>>,
    request_times: Mutex<VecDeque<Instant>>,
    import_gates: ProviderImportGates,
}

impl RelayState {
    pub fn new() -> anyhow::Result<(Self, String)> {
        let mut random = [0_u8; 8];
        SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| anyhow::anyhow!("could not generate relay pairing code"))?;
        let number = u64::from_be_bytes(random) % 100_000_000;
        let code = format!("{number:08}");
        let hash: [u8; 32] = Sha256::digest(code.as_bytes()).into();
        Ok((
            Self {
                approval: Mutex::new(None),
                pairing_hash: Mutex::new(hash),
                pairing_expires: Mutex::new(Instant::now() + PAIRING_LIFETIME),
                pairing_attempts: Mutex::new(0),
                install_token_hash: Mutex::new(load_install_token_hash()?),
                used_nonces: Mutex::new(HashMap::new()),
                request_times: Mutex::new(VecDeque::new()),
                import_gates: ProviderImportGates::default(),
            },
            code,
        ))
    }
}

/// Only a local operator holding the separate, Docker-volume-backed pairing
/// administration secret can rotate a pairing code. Never expose this secret
/// to the browser extension or accept an unauthenticated regeneration request.
pub async fn rotate_pairing_code(
    State(app): State<Arc<crate::AppState>>,
    headers: HeaderMap,
) -> Response {
    let secret = match fs::read_to_string("/run/secrets/pairing-admin.key") {
        Ok(value) if value.trim().len() >= 32 => value,
        _ => {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "pairing_admin_unavailable",
                "pairing administration is not configured",
            )
        }
    };
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    let expected: [u8; 32] = Sha256::digest(secret.trim().as_bytes()).into();
    let candidate: [u8; 32] = Sha256::digest(supplied.as_bytes()).into();
    if !bool::from(candidate.ct_eq(&expected)) {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "pairing_admin_rejected",
            "pairing administrator authentication required",
        );
    }
    let mut attempts = match app.relay.pairing_attempts.lock() {
        Ok(value) => value,
        Err(_) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "relay_unavailable",
                "relay state is unavailable",
            )
        }
    };
    let mut hash = match app.relay.pairing_hash.lock() {
        Ok(value) => value,
        Err(_) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "relay_unavailable",
                "relay state is unavailable",
            )
        }
    };
    let mut expiry = match app.relay.pairing_expires.lock() {
        Ok(value) => value,
        Err(_) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "relay_unavailable",
                "relay state is unavailable",
            )
        }
    };
    let mut random = [0_u8; 8];
    if SystemRandom::new().fill(&mut random).is_err() {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "relay_unavailable",
            "could not generate pairing code",
        );
    }
    let code = format!("{:08}", u64::from_be_bytes(random) % 100_000_000);
    *hash = Sha256::digest(code.as_bytes()).into();
    *expiry = Instant::now() + PAIRING_LIFETIME;
    *attempts = 0;
    Json(serde_json::json!({"ok":true,"code":code,"expires_in_seconds":PAIRING_LIFETIME.as_secs()}))
        .into_response()
}

fn install_token_hash_path() -> &'static FsPath {
    FsPath::new("sessions/relay-install-token.sha256")
}

fn load_install_token_hash() -> anyhow::Result<Option<[u8; 32]>> {
    match fs::read(install_token_hash_path()) {
        Ok(bytes) => {
            let hash: [u8; 32] = bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("persisted relay install token hash is malformed"))?;
            Ok(Some(hash))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn persist_install_token_hash(hash: &[u8; 32]) -> anyhow::Result<()> {
    persist_private_record("relay-install-token.sha256", hash)
}

fn persist_private_record(name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    fs::create_dir_all("sessions")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions("sessions", fs::Permissions::from_mode(0o700))?;
    }
    let temporary = format!("sessions/{name}.tmp-{}", uuid::Uuid::new_v4());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    if let Err(error) = (|| -> anyhow::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })() {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    if let Err(error) = fs::rename(&temporary, format!("sessions/{name}")) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    if let Ok(directory) = fs::File::open("sessions") {
        directory.sync_all()?;
    }
    Ok(())
}

fn delete_install_token_hash() -> anyhow::Result<()> {
    match fs::remove_file(install_token_hash_path()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[derive(Deserialize)]
pub struct PairRequest {
    code: String,
}

fn json_error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (
        status,
        Json(serde_json::json!({"error":{"code":code,"message":message}})),
    )
        .into_response()
}

fn extension_id_is_valid(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|byte| (b'a'..=b'p').contains(&byte))
}

fn extension_id() -> Option<String> {
    let id = match std::env::var("CHAT_GATEWAY_EXTENSION_ID") {
        Ok(id) => id,
        Err(_) => fs::read_to_string("sessions/relay-extension-id").ok()?,
    };
    extension_id_is_valid(&id).then_some(id)
}

fn requested_extension_id(headers: &HeaderMap) -> Option<String> {
    let id = headers
        .get(header::ORIGIN)?
        .to_str()
        .ok()?
        .strip_prefix("chrome-extension://")?;
    extension_id_is_valid(id).then(|| id.to_owned())
}

pub fn relay_request_origin_is_allowed(path: &str, headers: &HeaderMap) -> bool {
    if matches!(path, "/relay/v1/connect/request" | "/relay/v1/connect/poll")
        && std::env::var_os("CHAT_GATEWAY_EXTENSION_ID").is_none()
    {
        return requested_extension_id(headers).is_some();
    }
    relay_origin_is_allowed(headers)
}

fn extension_origin_matches(id: &str, origin: &str) -> bool {
    extension_id_is_valid(id) && origin == format!("chrome-extension://{id}")
}

/// The extension origin must match one configured Chrome extension ID exactly.
pub fn relay_origin_is_allowed(headers: &HeaderMap) -> bool {
    let Some(id) = extension_id() else {
        return false;
    };
    headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| extension_origin_matches(&id, origin))
}

fn issue_install_token(relay: &RelayState) -> Result<String, Box<Response>> {
    let mut slot = relay.install_token_hash.lock().map_err(|_| {
        Box::new(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "relay_unavailable",
            "relay state is unavailable",
        ))
    })?;
    let token = random_capability()?;
    let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    persist_install_token_hash(&hash).map_err(|_| {
        Box::new(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "relay_unavailable",
            "could not persist extension connection",
        ))
    })?;
    *slot = Some(hash);
    Ok(token)
}

fn random_capability() -> Result<String, Box<Response>> {
    let mut bytes = [0_u8; 32];
    SystemRandom::new().fill(&mut bytes).map_err(|_| {
        Box::new(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "relay_unavailable",
            "could not generate connection capability",
        ))
    })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

// Local approval is a short-lived, single-use exchange. The extension receives
// only the polling capability; the approval CSRF value is delivered exclusively
// to a top-level local browser navigation, with framing and caching disabled.
// A new user-initiated request replaces an abandoned request, invalidating its
// poll secret and approval URL. It never approves an extension automatically.
fn replace_pending_approval(
    pending: &mut Option<PendingApproval>,
    headers: &HeaderMap,
) -> Result<(String, String), Box<Response>> {
    let requested_id = requested_extension_id(headers).unwrap_or_default();
    // A second extension must not be able to cancel another extension's
    // unexpired approval. Only the same extension can explicitly restart.
    if pending.as_ref().is_some_and(|current| {
        current.expires > Instant::now() && current.extension_id != requested_id
    }) {
        return Err(Box::new(json_error(
            StatusCode::CONFLICT,
            "approval_pending",
            "another extension has an unexpired approval request",
        )));
    }
    let id = random_capability()?;
    let csrf = random_capability()?;
    let poll_secret = random_capability()?;
    *pending = Some(PendingApproval {
        id: id.clone(),
        csrf,
        poll_secret: poll_secret.clone(),
        extension_id: requested_id,
        expires: Instant::now() + Duration::from_secs(300),
        approved: false,
    });
    Ok((id, poll_secret))
}

pub async fn connect_request(
    State(app): State<Arc<crate::AppState>>,
    headers: HeaderMap,
) -> Response {
    if !relay_request_origin_is_allowed("/relay/v1/connect/request", &headers)
        || crate::remote_gateway_origin().ok().flatten().is_some()
    {
        return json_error(
            StatusCode::FORBIDDEN,
            "local_approval_required",
            "local configured extension required",
        );
    }
    let Ok(mut pending) = app.relay.approval.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    match replace_pending_approval(&mut pending, &headers) {
        Ok((id, poll_secret)) => {
            Json(serde_json::json!({"request_id":id,"poll_secret":poll_secret})).into_response()
        }
        Err(response) => *response,
    }
}

pub async fn approval_page(
    State(app): State<Arc<crate::AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if crate::remote_gateway_origin().ok().flatten().is_some()
        || !approval_navigation_is_allowed(&headers)
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(pending) = app.relay.approval.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Some(request) = pending
        .as_ref()
        .filter(|r| r.id == id && r.expires > Instant::now() && !r.approved)
    else {
        return StatusCode::GONE.into_response();
    };
    // IDs, CSRF values and extension ID are generated/validated ASCII, never user HTML.
    let extension = &request.extension_id;
    let html = format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>Approve Chat Gateway extension</title></head><body><h1>Connect your extension</h1><p>Allow extension {extension} to sync the provider sessions you choose and show their status.</p><form method="post"><input type="hidden" name="csrf" value="{}"><button type="submit">Approve connection</button></form><p>Only approve a connection you just started in the Chat Gateway extension. Close this tab to leave it unapproved.</p></body></html>"#,
        request.csrf
    );
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
            ),
            (header::REFERRER_POLICY, "same-origin"),
        ],
        Html(html),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct ApprovalConfirm {
    csrf: String,
}

pub async fn approve_connection(
    State(app): State<Arc<crate::AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(body): Form<ApprovalConfirm>,
) -> Response {
    if crate::remote_gateway_origin().ok().flatten().is_some()
        || !headers.contains_key(header::ORIGIN)
        || !crate::login_origin_is_same_authority(&headers)
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(mut pending) = app.relay.approval.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Some(request) = pending
        .as_mut()
        .filter(|r| r.id == id && r.expires > Instant::now() && !r.approved)
    else {
        return StatusCode::GONE.into_response();
    };
    if !bool::from(request.csrf.as_bytes().ct_eq(body.csrf.as_bytes())) {
        return StatusCode::FORBIDDEN.into_response();
    }
    request.approved = true;
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; frame-ancestors 'none'",
            ),
        ],
        Html("Connection approved. Return to the extension; you may close this tab."),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct ApprovalPoll {
    request_id: String,
    poll_secret: String,
}

pub async fn connect_poll(
    State(app): State<Arc<crate::AppState>>,
    headers: HeaderMap,
    Json(body): Json<ApprovalPoll>,
) -> Response {
    if !relay_request_origin_is_allowed("/relay/v1/connect/poll", &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(mut pending) = app.relay.approval.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Some(request) = pending.as_ref().filter(|r| {
        extension_origin_matches(
            &r.extension_id,
            headers
                .get(header::ORIGIN)
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
        ) && r.expires > Instant::now()
            && r.id == body.request_id
            && bool::from(r.poll_secret.as_bytes().ct_eq(body.poll_secret.as_bytes()))
    }) else {
        return StatusCode::GONE.into_response();
    };
    if !request.approved {
        return (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"pending":true})),
        )
            .into_response();
    }
    if persist_private_record("relay-extension-id", request.extension_id.as_bytes()).is_err() {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "relay_unavailable",
            "could not persist approved extension identity",
        );
    }
    match issue_install_token(&app.relay) {
        Ok(token) => {
            *pending = None;
            Json(serde_json::json!({"token":token})).into_response()
        }
        Err(response) => *response,
    }
}

pub async fn pair(
    State(app): State<Arc<crate::AppState>>,
    headers: HeaderMap,
    Json(request): Json<PairRequest>,
) -> Response {
    if !relay_origin_is_allowed(&headers) {
        return json_error(
            StatusCode::FORBIDDEN,
            "extension_origin_rejected",
            "the configured browser extension origin is required",
        );
    }
    if request.code.len() != 8 || !request.code.bytes().all(|b| b.is_ascii_digit()) {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "pairing_rejected",
            "pairing code is invalid or expired",
        );
    }
    let mut attempts = match app.relay.pairing_attempts.lock() {
        Ok(value) => value,
        Err(_) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "relay_unavailable",
                "relay state is unavailable",
            )
        }
    };
    let expiry = match app.relay.pairing_expires.lock() {
        Ok(value) => *value,
        Err(_) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "relay_unavailable",
                "relay state is unavailable",
            )
        }
    };
    if *attempts >= MAX_PAIR_ATTEMPTS || Instant::now() >= expiry {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "pairing_rejected",
            "pairing code is invalid or expired",
        );
    }
    *attempts = attempts.saturating_add(1);
    let candidate: [u8; 32] = Sha256::digest(request.code.as_bytes()).into();
    let expected = match app.relay.pairing_hash.lock() {
        Ok(value) => *value,
        Err(_) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "relay_unavailable",
                "relay state is unavailable",
            )
        }
    };
    if !bool::from(candidate.ct_eq(&expected)) {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "pairing_rejected",
            "pairing code is invalid or expired",
        );
    }
    let token = match issue_install_token(&app.relay) {
        Ok(token) => token,
        Err(response) => return *response,
    };
    // A successful exchange consumes the pairing code permanently.
    *attempts = MAX_PAIR_ATTEMPTS;
    tracing::info!("browser extension paired");
    Json(serde_json::json!({"ok":true,"token":token})).into_response()
}

fn relay_rate_limit_allows(request_times: &mut VecDeque<Instant>, now: Instant) -> bool {
    request_times.retain(|time| now.duration_since(*time) < Duration::from_secs(60));
    if request_times.len() >= MAX_RELAY_REQUESTS_PER_MINUTE {
        return false;
    }
    request_times.push_back(now);
    true
}

fn authenticated_request(app: &crate::AppState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    authenticated_request_with_origin(&app.relay, headers, relay_origin_is_allowed(headers))
}

fn authenticated_request_with_origin(
    relay: &RelayState,
    headers: &HeaderMap,
    origin_allowed: bool,
) -> Result<(), Box<Response>> {
    if !origin_allowed {
        return Err(Box::new(json_error(
            StatusCode::FORBIDDEN,
            "extension_origin_rejected",
            "the configured browser extension origin is required",
        )));
    }
    // Proxies and HTTP stacks can disagree about whether the first or last
    // duplicate security header wins. Reject duplicates before authentication
    // or nonce replay checks, while preserving existing missing-credential
    // error responses for backward-compatible clients.
    for name in [
        header::AUTHORIZATION.as_str(),
        "x-relay-timestamp",
        "x-relay-nonce",
    ] {
        if headers.get_all(name).iter().count() > 1 {
            return Err(Box::new(json_error(
                StatusCode::BAD_REQUEST,
                "relay_auth_headers_ambiguous",
                "duplicate relay authentication headers are not permitted",
            )));
        }
    }
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let token = authorization.and_then(|value| value.strip_prefix("Bearer "));
    let Some(token) = token else {
        return Err(Box::new(json_error(
            StatusCode::UNAUTHORIZED,
            "relay_auth_required",
            "a paired extension token is required",
        )));
    };
    let candidate: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let token_slot = relay.install_token_hash.lock().map_err(|_| {
        Box::new(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "relay_unavailable",
            "relay state is unavailable",
        ))
    })?;
    let Some(expected) = token_slot.as_ref() else {
        return Err(Box::new(json_error(
            StatusCode::UNAUTHORIZED,
            "relay_auth_required",
            "the extension is not paired",
        )));
    };
    if !bool::from(candidate.ct_eq(expected)) {
        return Err(Box::new(json_error(
            StatusCode::UNAUTHORIZED,
            "relay_auth_required",
            "the paired extension token is invalid",
        )));
    }
    drop(token_slot);

    let timestamp = headers
        .get("x-relay-timestamp")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    if timestamp.is_none_or(|timestamp| now.abs_diff(timestamp) > REQUEST_SKEW_SECONDS as u64) {
        return Err(Box::new(json_error(
            StatusCode::UNAUTHORIZED,
            "relay_request_expired",
            "relay request timestamp is missing or expired",
        )));
    }
    let nonce = headers
        .get("x-relay-nonce")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if nonce.len() < 32 || nonce.len() > 128 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Box::new(json_error(
            StatusCode::BAD_REQUEST,
            "relay_nonce_invalid",
            "relay request nonce is invalid",
        )));
    }
    let mut nonces = relay.used_nonces.lock().map_err(|_| {
        Box::new(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "relay_unavailable",
            "relay state is unavailable",
        ))
    })?;
    nonces.retain(|_, inserted| inserted.elapsed() < NONCE_RETENTION);
    if nonces.contains_key(nonce) {
        return Err(Box::new(json_error(
            StatusCode::UNAUTHORIZED,
            "relay_nonce_reused",
            "relay request nonce has already been used",
        )));
    }
    if nonces.len() >= 10_000 {
        return Err(Box::new(json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "relay_rate_limited",
            "too many relay requests; retry shortly",
        )));
    }
    let now = Instant::now();
    let mut request_times = relay.request_times.lock().map_err(|_| {
        Box::new(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "relay_unavailable",
            "relay state is unavailable",
        ))
    })?;
    if !relay_rate_limit_allows(&mut request_times, now) {
        return Err(Box::new(json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "relay_rate_limited",
            "too many relay requests; retry shortly",
        )));
    }
    nonces.insert(nonce.to_owned(), now);
    Ok(())
}

fn provider_authentication_status(
    observed: Option<bool>,
    state: crate::provider_state::ProviderSessionState,
    manual_verification_required: bool,
) -> &'static str {
    // An earlier login-view probe may still show a stale composer while the
    // latest chat attempt has proved that credentials expired or a manual
    // challenge blocks the session. Explicit blockers take precedence over
    // opportunistic browser observations.
    if manual_verification_required {
        return "unknown";
    }
    if state == crate::provider_state::ProviderSessionState::NeedsLogin {
        return "needs_login";
    }
    match observed {
        Some(true) => "verified",
        Some(false) => "needs_login",
        // Routability alone (including public model discovery) is not proof of login.
        None => "unknown",
    }
}

/// Fixed-category runtime diagnostics. Never return upstream errors, selectors,
/// URLs, browser storage or credential material in the extension-facing API.
fn relay_provider_issue(
    linked: bool,
    enabled: bool,
    authentication: &str,
    state: crate::provider_state::ProviderSessionState,
    manual_verification_required: bool,
    last_error: Option<&str>,
) -> Option<&'static str> {
    use crate::provider_state::ProviderSessionState;
    if !enabled {
        Some("provider_disabled")
    } else if !linked {
        Some("session_not_linked")
    } else if manual_verification_required {
        Some("manual_verification_required")
    } else if last_error.is_some_and(|error| error.contains("JavaScript assets blocked (HTTP 403)")) {
        Some("provider_assets_blocked")
    } else if state == ProviderSessionState::NeedsLogin || authentication == "needs_login" {
        Some("needs_login")
    } else if state == ProviderSessionState::Degraded {
        Some("provider_degraded")
    } else if authentication != "verified" {
        Some("authentication_unverified")
    } else {
        None
    }
}

#[cfg(test)]
mod relay_provider_issue_tests {
    use super::relay_provider_issue;
    use crate::provider_state::ProviderSessionState::{Degraded, NeedsLogin, Ready, Unknown};

    #[test]
    fn diagnostics_are_fixed_categories_not_upstream_error_text() {
        assert_eq!(
            relay_provider_issue(true, true, "unknown", Degraded, false, Some("JavaScript assets blocked (HTTP 403): cookie=private")),
            Some("provider_assets_blocked")
        );
        assert_eq!(
            relay_provider_issue(true, true, "unknown", Degraded, true, Some("cookie=private")),
            Some("manual_verification_required")
        );
        assert_eq!(relay_provider_issue(true, true, "needs_login", NeedsLogin, false, None), Some("needs_login"));
        assert_eq!(relay_provider_issue(true, true, "verified", Ready, false, None), None);
        assert_eq!(relay_provider_issue(true, true, "unknown", Unknown, false, None), Some("authentication_unverified"));
        assert_eq!(relay_provider_issue(false, true, "unknown", Unknown, false, None), Some("session_not_linked"));
    }
}

async fn optional_login_observations(
    observations: impl std::future::Future<
        Output = anyhow::Result<Vec<crate::browser::pool::LoginActiveInfo>>,
    >,
) -> Vec<crate::browser::pool::LoginActiveInfo> {
    // Provider probes share the browser thread and can block its command queue.
    // The dashboard can still report saved sessions and cached lifecycle state.
    tokio::time::timeout(Duration::from_millis(500), observations)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

pub async fn status(State(app): State<Arc<crate::AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authenticated_request(&app, &headers) {
        return *response;
    }
    // A Docker installation may not have an embedded login worker. Keep the
    // saved-session dashboard available and treat live verification as unknown.
    let active = optional_login_observations(app.pool.login_status_all()).await;
    const PROVIDERS: [&str; 9] = [
        "qwen",
        "metaai",
        "claude",
        "chatgpt",
        "gemini",
        "grok",
        "mistral",
        "perplexity",
        "manus",
    ];
    // Session loading may decrypt multiple vault records and read disk. Keep
    // it off the async HTTP reactor; return presence flags only, not secrets.
    let session_presence = bounded_session_status_scan(RELAY_STATUS_SCAN_TIMEOUT, move || {
        PROVIDERS
            .iter()
            .map(|provider| crate::session::load_session_file(provider).map(|s| s.is_some()))
            .collect::<anyhow::Result<Vec<_>>>()
    })
    .await;
    let session_presence = match session_presence {
        Ok(Ok(presence)) => presence,
        Ok(Err(_)) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "session_status_unavailable",
                "provider session status could not be read; check vault configuration",
            );
        }
        Err(_) => {
            return json_error(
                StatusCode::GATEWAY_TIMEOUT,
                "session_status_timeout",
                "provider session status scan timed out; retry after checking vault storage",
            );
        }
    };
    let mut providers = Vec::new();
    let mut issues = Vec::new();
    for (provider, session_present) in PROVIDERS.into_iter().zip(session_presence) {
        let enabled = app
            .cfg
            .provider(provider)
            .is_some_and(|config| config.enabled);
        // Report the embedded browser's observed state only when available.
        // Saved cookies alone are never proof of a valid provider login.
        let observed = active
            .iter()
            .find(|item| item.provider == provider)
            .and_then(|item| item.logged_in);
        let runtime = app.provider_sessions.status(provider);
        let lifecycle = runtime.state;
        let manual_verification_required =
            app.provider_sessions.manual_challenge_required(provider);
        let authentication =
            provider_authentication_status(observed, lifecycle, manual_verification_required);
        let issue = relay_provider_issue(
            session_present,
            enabled,
            authentication,
            lifecycle,
            manual_verification_required,
            runtime.last_error.as_deref(),
        );
        if let Some(code) = issue {
            issues.push(serde_json::json!({"provider":provider,"code":code}));
        }
        providers.push(serde_json::json!({
            "provider": provider,
            "linked": session_present,
            "enabled": enabled,
            "authentication": authentication,
            "availability": lifecycle.as_str(),
            "manual_verification_required": manual_verification_required,
            "issue": issue,
        }));
    }
    Json(serde_json::json!({"ok":true,"providers":providers,"issues":issues,"vault_mode":std::env::var("CHAT_GATEWAY_SESSION_STORE").as_deref()==Ok("vault")})).into_response()
}

/// Relay import validates, encrypts and writes a credential snapshot. Those
/// operations involve blocking disk I/O and must not stall the async HTTP
/// reactor while a browser worker is also being activated.
/// A task-join failure is an infrastructure fault, not evidence that the
/// submitted credentials were malformed. Keep the public error generic.
#[derive(Debug)]
struct RelaySnapshotWorkerUnavailable;

impl std::fmt::Display for RelaySnapshotWorkerUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("relay session persistence worker unavailable")
    }
}

impl std::error::Error for RelaySnapshotWorkerUnavailable {}

async fn session_snapshot_io_off_reactor<T: Send + 'static>(
    operation: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| anyhow::Error::new(RelaySnapshotWorkerUnavailable))?
}

/// A status scan is read-only: the caller may stop waiting without making
/// another import or losing a valid vault snapshot. The blocking thread may
/// finish later, so do not apply this timeout wrapper to credential writes.
async fn bounded_session_status_scan<T: Send + 'static>(
    wait: Duration,
    operation: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<anyhow::Result<T>, tokio::time::error::Elapsed> {
    tokio::time::timeout(wait, session_snapshot_io_off_reactor(operation)).await
}

#[cfg(test)]
mod relay_persist_off_reactor_tests {
    use super::{bounded_session_status_scan, session_snapshot_io_off_reactor};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_persistence_runs_off_the_async_request_thread() {
        let request_thread = std::thread::current().id();
        let observed = Arc::new(Mutex::new(None));
        let worker_observed = Arc::clone(&observed);
        session_snapshot_io_off_reactor(move || {
            *worker_observed.lock().unwrap() = Some(std::thread::current().id());
            Ok(())
        })
        .await
        .unwrap();
        let worker_thread = observed.lock().unwrap().unwrap();
        assert_ne!(worker_thread, request_thread);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn status_presence_scan_runs_off_reactor_and_returns_only_flags() {
        let reactor_thread = std::thread::current().id();
        let (scanner_thread, flags) = session_snapshot_io_off_reactor(|| {
            Ok((std::thread::current().id(), vec![true, false, true]))
        })
        .await
        .unwrap();
        assert_ne!(reactor_thread, scanner_thread);
        assert_eq!(flags, vec![true, false, true]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stalled_vault_status_scan_times_out_without_blocking_async_requests() {
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let result = bounded_session_status_scan(Duration::from_millis(20), move || {
            blocked.recv()?;
            Ok(vec![true, false])
        })
        .await;
        assert!(result.is_err(), "status scan must have a bounded HTTP wait");
        // A previously dispatched read-only task can finish safely after the
        // caller stops waiting; do not let the test strand a blocking thread.
        release.send(()).unwrap();
    }

    #[tokio::test]
    async fn bounded_status_scan_propagates_success_and_storage_failure() {
        let success = bounded_session_status_scan(Duration::from_secs(1), || Ok(vec![true, false]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(success, vec![true, false]);
        let failure = bounded_session_status_scan::<()>(Duration::from_secs(1), || {
            anyhow::bail!("synthetic vault status failure")
        })
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(failure.to_string(), "synthetic vault status failure");
    }

    #[tokio::test]
    async fn persistence_failure_is_propagated_before_activation() {
        let failed =
            session_snapshot_io_off_reactor::<()>(|| anyhow::bail!("synthetic snapshot rejection"))
                .await
                .unwrap_err();
        assert_eq!(failed.to_string(), "synthetic snapshot rejection");
    }
}

/// Never expose arbitrary error strings returned by credential import in
/// telemetry; storage/validation errors may contain private file paths or
/// values supplied by the extension. Emit only this fixed category.
fn relay_import_failure_category(error: &anyhow::Error) -> &'static str {
    if error
        .downcast_ref::<RelaySnapshotWorkerUnavailable>()
        .is_some()
    {
        "persistence_worker_unavailable"
    } else if error
        .downcast_ref::<crate::session::RelayMissingCookiesError>()
        .is_some()
    {
        "missing_session_cookies"
    } else {
        "snapshot_rejected"
    }
}

/// Unlink errors may contain private vault paths or provider-supplied text.
/// The response is already redacted; preserve the same boundary for log data.
fn relay_unlink_failure_category(error: &anyhow::Error) -> &'static str {
    if error
        .downcast_ref::<crate::browser::pool::BrowserWorkerUnavailableError>()
        .is_some()
    {
        "browser_worker_unavailable"
    } else {
        "session_revocation_failed"
    }
}

#[cfg(test)]
mod relay_unlink_diagnostic_redaction_tests {
    #[test]
    fn unlink_error_category_never_includes_private_upstream_text() {
        let secret = anyhow::anyhow!("cookie=very-secret; bearer=private; /vault/token");
        assert_eq!(
            super::relay_unlink_failure_category(&secret),
            "session_revocation_failed"
        );
        let worker = anyhow::Error::new(crate::browser::pool::BrowserWorkerUnavailableError);
        assert_eq!(
            super::relay_unlink_failure_category(&worker),
            "browser_worker_unavailable"
        );
    }
}

#[cfg(test)]
mod relay_import_diagnostic_redaction_tests {
    #[test]
    fn worker_failure_has_distinct_redacted_category() {
        let error = anyhow::Error::new(super::RelaySnapshotWorkerUnavailable);
        let label = super::relay_import_failure_category(&error);
        assert_eq!(label, "persistence_worker_unavailable");
        assert!(!label.contains("cookie"));
    }

    #[tokio::test]
    async fn join_failure_is_typed_without_leaking_panic_payload() {
        let failure = super::session_snapshot_io_off_reactor::<()>(|| {
            panic!("synthetic sensitive-looking token");
        })
        .await
        .unwrap_err();
        assert!(failure
            .downcast_ref::<super::RelaySnapshotWorkerUnavailable>()
            .is_some());
        assert!(!failure
            .to_string()
            .contains("synthetic sensitive-looking token"));
    }

    #[test]
    fn empty_cookie_capture_has_specific_redacted_diagnostic() {
        let error = anyhow::Error::new(crate::session::RelayMissingCookiesError);
        assert_eq!(
            super::relay_import_failure_category(&error),
            "missing_session_cookies"
        );
        assert!(!super::relay_import_failure_category(&error).contains('@'));
    }

    #[test]
    fn import_error_labels_cannot_forward_secret_error_text() {
        let error = anyhow::anyhow!("cookie=very-secret; bearer=private; user=person@example.test");
        let label = super::relay_import_failure_category(&error);
        assert_eq!(label, "snapshot_rejected");
        assert!(!label.contains("very-secret"));
        assert!(!label.contains("private"));
        assert!(!label.contains('@'));
    }
}

pub async fn import_session(
    State(app): State<Arc<crate::AppState>>,
    headers: HeaderMap,
    Path(provider): Path<String>,
    Json(file): Json<crate::session::SessionFile>,
) -> Response {
    if let Err(response) = authenticated_request(&app, &headers) {
        return *response;
    }
    if !app
        .cfg
        .provider(&provider)
        .is_some_and(|config| config.enabled)
    {
        return json_error(
            StatusCode::BAD_REQUEST,
            "provider_not_enabled",
            "provider must be enabled before its browser session can be linked",
        );
    }
    let import_started = Instant::now();
    let gate = match app.relay.import_gates.for_provider(&provider) {
        Some(gate) => gate,
        None => {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "session_import_unavailable",
                "relay import queue unavailable; retry after service recovery",
            )
        }
    };
    let provider_import_guard =
        match tokio::time::timeout(RELAY_IMPORT_QUEUE_TIMEOUT, gate.lock_owned()).await {
            Ok(guard) => guard,
            Err(_) => {
                let queued_ms = import_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                app.metrics.record_relay_import_phase("queue", queued_ms);
                app.metrics.record_relay_import_phase("total", queued_ms);
                return json_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "session_import_busy",
                    "provider import is busy; retry after the earlier import completes",
                );
            }
        };
    app.metrics.record_relay_import_phase(
        "queue",
        import_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
    );
    let tail_metrics_app = Arc::clone(&app);
    let task = run_provider_mutation_detached_with_tail(provider_import_guard, async move {
        let store_started = Instant::now();
        let provider_for_persist = provider.clone();
        let imported = session_snapshot_io_off_reactor(move || {
            crate::session::import_relay_session(&provider_for_persist, &file)
        })
        .await;
        let vault_ms = store_started.elapsed().as_millis();
        app.metrics
            .record_relay_import_phase("persist", vault_ms.min(u64::MAX as u128) as u64);
        // A rejected snapshot has no activation phase. Still record one total
        // duration so import latency distributions include failed attempts.
        if imported.is_err() {
            app.metrics.record_relay_import_phase(
                "total",
                import_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            );
        }
        match imported {
            Ok(()) => {
                let activation_started = Instant::now();
                // Keep the queued/active browser refresh alive after the
                // bounded HTTP wait, including while the browser is executing
                // an already-dispatched, non-cancellable session replacement.
                let refresh_app = Arc::clone(&app);
                let refresh_provider = provider.clone();
                let mut refresh = tokio::spawn(async move {
                    refresh_app.pool.refresh_session(&refresh_provider).await
                });
                let activated =
                    bounded_session_activation(RELAY_SESSION_ACTIVATION_TIMEOUT, &mut refresh).await;
                let pending_refresh = if activated.is_err() {
                    Some(refresh)
                } else {
                    None
                };
                app.metrics.record_relay_import_phase(
                    "activate",
                    activation_started
                        .elapsed()
                        .as_millis()
                        .min(u64::MAX as u128) as u64,
                );
                // Includes activation successes, failures and bounded timeouts.
                app.metrics.record_relay_import_phase(
                    "total",
                    import_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                );
                let response = match activated {
                    Ok(Ok(Ok(()))) => {
                        tracing::info!(
                            provider,
                            vault_ms,
                            activation_ms = activation_started.elapsed().as_millis(),
                            "browser relay session persisted and activation request completed"
                        );
                        Json(serde_json::json!({"ok":true,"provider":provider,"linked":true,"active":true}))
                            .into_response()
                    }
                    Ok(Ok(Err(_))) | Ok(Err(_)) => {
                        tracing::warn!(
                            provider,
                            vault_ms,
                            activation_ms = activation_started.elapsed().as_millis(),
                            "browser relay session saved but activation failed"
                        );
                        json_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "session_refresh_failed",
                            "session was saved but could not be activated; retry or restart the gateway",
                        )
                    }
                    Err(_) => {
                        tracing::warn!(
                            provider,
                            vault_ms,
                            activation_ms = activation_started.elapsed().as_millis(),
                            "browser relay activation wait expired; saved credentials retained for retry"
                        );
                        json_error(
                            StatusCode::GATEWAY_TIMEOUT,
                            "session_activation_timeout",
                            "session saved but activation timed out; check provider status before retrying",
                        )
                    }
                };
                (response, pending_refresh)
            }
            Err(error) => {
                let category = relay_import_failure_category(&error);
                tracing::warn!(
                    provider,
                    category,
                    "browser relay session import unsuccessful"
                );
                let response = if category == "persistence_worker_unavailable" {
                    json_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "session_import_unavailable",
                        "relay persistence worker unavailable; retry after service recovery",
                    )
                } else if category == "missing_session_cookies" {
                    json_error(
                        StatusCode::BAD_REQUEST,
                        "session_missing_credentials",
                        "no provider cookies were captured; sign in to the provider and retry linking",
                    )
                } else {
                    json_error(
                        StatusCode::BAD_REQUEST,
                        "session_import_rejected",
                        "session was rejected; check vault configuration and provider permissions",
                    )
                };
                (response, None)
            }
        }
    }, move |elapsed_ms| {
        tail_metrics_app
            .metrics
            .record_relay_import_phase("activation_tail", elapsed_ms);
    })
    .await;
    match task {
        Ok(response) => response,
        Err(_) => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "session_import_unavailable",
            "relay import worker unavailable; check provider status before retrying",
        ),
    }
}

pub async fn unlink(
    State(app): State<Arc<crate::AppState>>,
    headers: HeaderMap,
    Path(provider): Path<String>,
) -> Response {
    if let Err(response) = authenticated_request(&app, &headers) {
        return *response;
    }
    // Validate before creating a gate: untrusted path parameters must not
    // grow the gate map beyond the supported provider catalog.
    if crate::providers::Provider::from_name(&provider).is_none() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "unknown_provider",
            "provider is not supported by this gateway",
        );
    }
    let gate = match app.relay.import_gates.for_provider(&provider) {
        Some(gate) => gate,
        None => {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "session_unlink_unavailable",
                "provider session revocation queue unavailable; retry",
            )
        }
    };
    // Hold the gate until the browser worker has finished deleting both the
    // live session and vault snapshot. A concurrent import must not restore
    // credentials after successful unlink.
    let provider_unlink_guard =
        match tokio::time::timeout(RELAY_IMPORT_QUEUE_TIMEOUT, gate.lock_owned()).await {
            Ok(guard) => guard,
            Err(_) => {
                return json_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "session_unlink_busy",
                    "provider session import or unlink is busy; retry revocation",
                )
            }
        };
    let task = run_provider_mutation_detached(provider_unlink_guard, async move {
        match app.pool.unlink_session(&provider).await {
            Ok(removed) => {
                app.provider_sessions.record_unlinked(&provider);
                tracing::info!(
                    provider,
                    "browser relay provider unlinked from embedded browser and persistent storage"
                );
                Json(serde_json::json!({"ok":true,"provider":provider,"unlinked":removed}))
                    .into_response()
            }
            Err(error) => {
                tracing::warn!(
                    provider,
                    category = relay_unlink_failure_category(&error),
                    "browser relay provider unlink incomplete"
                );
                json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "unlink_failed",
                    "provider session could not be fully revoked; saved data was retained when cleanup failed",
                )
            }
        }
    })
    .await;
    match task {
        Ok(response) => response,
        Err(_) => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "session_unlink_unavailable",
            "provider session revocation worker unavailable; check status and retry",
        ),
    }
}

pub async fn unpair(State(app): State<Arc<crate::AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authenticated_request(&app, &headers) {
        return *response;
    }
    if let Ok(mut pending) = app.relay.approval.lock() {
        *pending = None;
    }
    match app.relay.install_token_hash.lock() {
        Ok(mut token) => {
            if delete_install_token_hash().is_err() {
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "relay_unavailable",
                    "could not revoke persisted extension state",
                );
            }
            *token = None;
            tracing::info!("browser extension unpaired");
            Json(serde_json::json!({"ok":true,"paired":false})).into_response()
        }
        Err(_) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "relay_unavailable",
            "relay state is unavailable",
        ),
    }
}

pub async fn preflight(uri: Uri, headers: HeaderMap) -> Response {
    if !relay_request_origin_is_allowed(uri.path(), &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&origin) {
        headers.insert("access-control-allow-origin", value);
    }
    headers.insert(
        "access-control-allow-methods",
        HeaderValue::from_static("GET, POST, DELETE, OPTIONS"),
    );
    headers.insert(
        "access-control-allow-headers",
        HeaderValue::from_static("authorization, content-type, x-relay-timestamp, x-relay-nonce"),
    );
    headers.insert("vary", HeaderValue::from_static("Origin"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dashboard_remains_available_when_browser_observations_stall_or_fail() {
        let stalled = std::future::pending();
        let observations =
            tokio::time::timeout(Duration::from_secs(2), optional_login_observations(stalled))
                .await
                .expect("dashboard must not wait for a stalled browser worker");
        assert!(observations.is_empty());
        let failed =
            optional_login_observations(async { Err(anyhow::anyhow!("worker stopped")) }).await;
        assert!(failed.is_empty());
    }

    #[tokio::test]
    async fn dashboard_keeps_available_live_login_observations() {
        let observations = optional_login_observations(async {
            Ok(vec![crate::browser::pool::LoginActiveInfo {
                provider: "chatgpt".into(),
                url: "https://chatgpt.com/".into(),
                title: String::new(),
                logged_in: Some(true),
                input_found: true,
                state: "ready".into(),
            }])
        })
        .await;
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].logged_in, Some(true));
    }

    #[test]
    fn approval_requires_top_level_navigation_not_script_or_frame() {
        for (mode, destination, allowed) in [
            ("navigate", "document", true),
            ("cors", "empty", false),
            ("navigate", "iframe", false),
            ("same-origin", "empty", false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("sec-fetch-mode", HeaderValue::from_static(mode));
            headers.insert("sec-fetch-dest", HeaderValue::from_static(destination));
            assert_eq!(approval_navigation_is_allowed(&headers), allowed);
        }
        assert!(!approval_navigation_is_allowed(&HeaderMap::new()));
    }

    #[test]
    fn reconnect_replaces_stale_approval_without_reusing_capabilities() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("chrome-extension://abcdefghijklmnopabcdefghijklmnop"),
        );
        let mut pending = None;
        let (old_id, old_secret) = replace_pending_approval(&mut pending, &headers).unwrap();
        let old_csrf = pending.as_ref().unwrap().csrf.clone();
        // Even an approved but not yet claimed request must not authorize
        // the replacement request: each connection needs local approval.
        pending.as_mut().unwrap().approved = true;
        let (new_id, new_secret) = replace_pending_approval(&mut pending, &headers).unwrap();
        let current = pending.as_ref().unwrap();
        assert_ne!(old_id, new_id);
        assert_ne!(old_secret, new_secret);
        assert_ne!(old_csrf, current.csrf);
        assert_eq!(current.id, new_id);
        assert_eq!(current.poll_secret, new_secret);
        assert!(!current.approved);
        assert_eq!(current.extension_id, "abcdefghijklmnopabcdefghijklmnop");
        assert!(current.expires > Instant::now());
    }

    #[test]
    fn reconnect_cannot_displace_another_extension_approval() {
        let mut original = HeaderMap::new();
        original.insert(
            header::ORIGIN,
            HeaderValue::from_static("chrome-extension://abcdefghijklmnopabcdefghijklmnop"),
        );
        let mut other = HeaderMap::new();
        other.insert(
            header::ORIGIN,
            HeaderValue::from_static("chrome-extension://bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        );
        let mut pending = None;
        let (first_id, first_secret) = replace_pending_approval(&mut pending, &original).unwrap();
        assert!(replace_pending_approval(&mut pending, &other).is_err());
        assert_eq!(pending.as_ref().unwrap().id, first_id);
        assert_eq!(pending.as_ref().unwrap().poll_secret, first_secret);
        pending.as_mut().unwrap().expires = Instant::now() - Duration::from_secs(1);
        assert!(replace_pending_approval(&mut pending, &other).is_ok());
    }

    #[test]
    fn requested_identity_only_accepts_exact_chrome_extension_origins() {
        for (origin, allowed) in [
            ("chrome-extension://abcdefghijklmnopabcdefghijklmnop", true),
            ("https://abcdefghijklmnopabcdefghijklmnop", false),
            (
                "chrome-extension://abcdefghijklmnopabcdefghijklmnop.evil",
                false,
            ),
            ("chrome-extension://short", false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::ORIGIN, HeaderValue::from_static(origin));
            assert_eq!(requested_extension_id(&headers).is_some(), allowed);
        }
    }

    #[test]
    fn provider_status_uses_runtime_login_failures_without_claiming_readiness_is_authentication() {
        use crate::provider_state::ProviderSessionState::{Degraded, NeedsLogin, Ready, Unknown};
        for (observed, state, challenge, expected) in [
            (None, NeedsLogin, false, "needs_login"),
            (Some(true), NeedsLogin, false, "needs_login"),
            (None, Ready, false, "unknown"),
            (None, Degraded, false, "unknown"),
            (None, Unknown, false, "unknown"),
            (Some(true), Ready, false, "verified"),
            (Some(false), Ready, false, "needs_login"),
            (Some(true), Degraded, true, "unknown"),
            (Some(false), Degraded, true, "unknown"),
        ] {
            assert_eq!(
                provider_authentication_status(observed, state, challenge),
                expected
            );
        }
    }

    #[test]
    fn challenge_blocker_does_not_falsely_report_expired_authentication() {
        let registry = crate::provider_state::ProviderSessionRegistry::default();
        registry.record_gateway_error(
            "gemini",
            &crate::chat_backend::GatewayError::challenge("manual verification required"),
        );
        let state = registry.status("gemini").state;
        assert!(registry.manual_challenge_required("gemini"));
        assert_eq!(provider_authentication_status(None, state, true), "unknown");
        assert_eq!(
            provider_authentication_status(Some(true), state, true),
            "unknown",
            "stale composer observations must not override manual verification"
        );
        assert!(!registry.is_available("gemini"));
    }

    fn relay_state_with_token(token: Option<&str>) -> RelayState {
        let install_token_hash = token.map(|token| {
            let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
            hash
        });
        RelayState {
            approval: Mutex::new(None),
            pairing_hash: Mutex::new([0; 32]),
            pairing_expires: Mutex::new(Instant::now() + PAIRING_LIFETIME),
            pairing_attempts: Mutex::new(0),
            install_token_hash: Mutex::new(install_token_hash),
            used_nonces: Mutex::new(HashMap::new()),
            request_times: Mutex::new(VecDeque::new()),
            import_gates: ProviderImportGates::default(),
        }
    }

    fn valid_auth_headers(token: &str, nonce: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        headers.insert(
            "x-relay-timestamp",
            HeaderValue::from_str(&now.to_string()).unwrap(),
        );
        headers.insert("x-relay-nonce", HeaderValue::from_str(nonce).unwrap());
        headers
    }

    #[test]
    fn unpaired_install_is_rejected_even_with_bearer_header() {
        let state = relay_state_with_token(None);
        let headers = valid_auth_headers("unpaired-token", "0123456789abcdef0123456789abcdef");
        assert!(authenticated_request_with_origin(&state, &headers, true).is_err());
    }

    #[test]
    fn revoked_install_token_is_rejected_immediately() {
        let token = "paired-install-token";
        let state = relay_state_with_token(Some(token));
        let first = valid_auth_headers(token, "11111111111111111111111111111111");
        assert!(authenticated_request_with_origin(&state, &first, true).is_ok());

        *state.install_token_hash.lock().unwrap() = None;
        let after_revoke = valid_auth_headers(token, "22222222222222222222222222222222");
        assert!(authenticated_request_with_origin(&state, &after_revoke, true).is_err());
    }

    #[test]
    fn extreme_timestamp_is_rejected_without_integer_overflow() {
        let token = "paired-install-token";
        let state = relay_state_with_token(Some(token));
        let mut headers = valid_auth_headers(token, "33333333333333333333333333333333");
        headers.insert(
            "x-relay-timestamp",
            HeaderValue::from_static("-9223372036854775808"),
        );
        assert!(authenticated_request_with_origin(&state, &headers, true).is_err());
    }

    #[test]
    fn duplicate_relay_security_headers_fail_closed_before_nonce_is_consumed() {
        let token = "paired-install-token";
        let state = relay_state_with_token(Some(token));
        for name in [
            header::AUTHORIZATION.as_str(),
            "x-relay-timestamp",
            "x-relay-nonce",
        ] {
            let mut headers = valid_auth_headers(token, "abcdef0123456789abcdef0123456789");
            let repeated = headers.get(name).unwrap().clone();
            headers.append(name, repeated);
            let response = authenticated_request_with_origin(&state, &headers, true)
                .expect_err("even identical duplicate relay security headers must be rejected");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
        }

        // A rejected ambiguous request must not burn its otherwise unused
        // nonce. Re-trying with a single copy must still succeed.
        let valid = valid_auth_headers(token, "abcdef0123456789abcdef0123456789");
        assert!(authenticated_request_with_origin(&state, &valid, true).is_ok());
    }

    #[test]
    fn missing_relay_credentials_keep_their_existing_response_statuses() {
        let token = "paired-install-token";
        let state = relay_state_with_token(Some(token));
        for (name, expected) in [
            (header::AUTHORIZATION.as_str(), StatusCode::UNAUTHORIZED),
            ("x-relay-timestamp", StatusCode::UNAUTHORIZED),
            ("x-relay-nonce", StatusCode::BAD_REQUEST),
        ] {
            let mut headers = valid_auth_headers(token, "abcdef0123456789abcdef0123456789");
            headers.remove(name);
            let response = authenticated_request_with_origin(&state, &headers, true)
                .expect_err("missing required relay credential must be rejected");
            assert_eq!(response.status(), expected, "{name}");
        }
    }

    #[test]
    fn replayed_authenticated_nonce_is_rejected() {
        let token = "paired-install-token";
        let state = relay_state_with_token(Some(token));
        let headers = valid_auth_headers(token, "abcdef0123456789abcdef0123456789");
        assert!(authenticated_request_with_origin(&state, &headers, true).is_ok());
        assert!(authenticated_request_with_origin(&state, &headers, true).is_err());
    }

    #[test]
    fn pairing_code_is_numeric_and_state_has_a_short_expiry() {
        let (state, code) = RelayState::new().unwrap();
        assert_eq!(code.len(), 8);
        assert!(code.bytes().all(|byte| byte.is_ascii_digit()));
        assert!(*state.pairing_expires.lock().unwrap() > Instant::now());
    }

    #[test]
    fn relay_rate_limit_allows_120_requests_then_rejects_until_window_expires() {
        let now = Instant::now();
        let mut times = VecDeque::new();
        for offset in 0..MAX_RELAY_REQUESTS_PER_MINUTE {
            assert!(relay_rate_limit_allows(
                &mut times,
                now + Duration::from_millis(offset as u64)
            ));
        }
        assert!(!relay_rate_limit_allows(
            &mut times,
            now + Duration::from_secs(30)
        ));
        assert!(relay_rate_limit_allows(
            &mut times,
            now + Duration::from_secs(61)
        ));
    }

    #[test]
    fn extension_origin_requires_exact_valid_chrome_extension_origin() {
        let id = "abcdefghijklmnopabcdefghijklmnop";
        assert!(extension_origin_matches(
            id,
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop"
        ));
        assert!(!extension_origin_matches(
            id,
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop.attacker"
        ));
        assert!(!extension_origin_matches(
            id,
            "https://abcdefghijklmnopabcdefghijklmnop"
        ));
        assert!(!extension_origin_matches(
            "invalid-extension-id",
            "chrome-extension://invalid-extension-id"
        ));
    }

    #[test]
    fn extension_origin_is_rejected_when_not_configured() {
        let headers = HeaderMap::new();
        assert!(!relay_origin_is_allowed(&headers));
    }
}

#[cfg(test)]
mod relay_activation_timeout_tests {
    use super::bounded_session_activation;
    use std::{cell::Cell, future, rc::Rc, time::Duration};

    struct CancellationObserved(Rc<Cell<bool>>);
    impl Drop for CancellationObserved {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    #[tokio::test]
    async fn timed_out_activation_drops_the_future_before_retry() {
        let cancelled = Rc::new(Cell::new(false));
        let marker = cancelled.clone();
        let blocked = async move {
            let _marker = CancellationObserved(marker);
            future::pending::<anyhow::Result<()>>().await
        };
        let result = bounded_session_activation(Duration::from_millis(10), blocked).await;
        assert!(result.is_err());
        assert!(cancelled.get(), "queued activation must be cancellable");
        let retry = bounded_session_activation(Duration::from_secs(1), async {
            Ok::<_, anyhow::Error>(())
        })
        .await;
        assert!(retry.unwrap().is_ok());
    }
}
