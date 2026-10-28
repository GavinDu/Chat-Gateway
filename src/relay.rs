use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
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

pub struct RelayState {
    pairing_hash: [u8; 32],
    pairing_expires: Instant,
    pairing_attempts: Mutex<u8>,
    install_token_hash: Mutex<Option<[u8; 32]>>,
    used_nonces: Mutex<HashMap<String, Instant>>,
    request_times: Mutex<VecDeque<Instant>>,
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
                pairing_hash: hash,
                pairing_expires: Instant::now() + PAIRING_LIFETIME,
                pairing_attempts: Mutex::new(0),
                install_token_hash: Mutex::new(load_install_token_hash()?),
                used_nonces: Mutex::new(HashMap::new()),
                request_times: Mutex::new(VecDeque::new()),
            },
            code,
        ))
    }
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
    fs::create_dir_all("sessions")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions("sessions", fs::Permissions::from_mode(0o700))?;
    }
    let temporary = format!(
        "sessions/relay-install-token.sha256.tmp-{}",
        uuid::Uuid::new_v4()
    );
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    if let Err(error) = (|| -> anyhow::Result<()> {
        file.write_all(hash)?;
        file.sync_all()?;
        Ok(())
    })() {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    if let Err(error) = fs::rename(&temporary, install_token_hash_path()) {
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
    let id = std::env::var("CHAT_GATEWAY_EXTENSION_ID").ok()?;
    extension_id_is_valid(&id).then_some(id)
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
    if *attempts >= MAX_PAIR_ATTEMPTS || Instant::now() >= app.relay.pairing_expires {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "pairing_rejected",
            "pairing code is invalid or expired",
        );
    }
    *attempts = attempts.saturating_add(1);
    let candidate: [u8; 32] = Sha256::digest(request.code.as_bytes()).into();
    if !bool::from(candidate.ct_eq(&app.relay.pairing_hash)) {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "pairing_rejected",
            "pairing code is invalid or expired",
        );
    }
    let mut token_slot = match app.relay.install_token_hash.lock() {
        Ok(value) => value,
        Err(_) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "relay_unavailable",
                "relay state is unavailable",
            )
        }
    };
    let mut random = [0_u8; 32];
    if SystemRandom::new().fill(&mut random).is_err() {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "relay_unavailable",
            "could not create install token",
        );
    }
    let token = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    if persist_install_token_hash(&token_hash).is_err() {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "relay_unavailable",
            "could not persist paired extension state",
        );
    }
    *token_slot = Some(token_hash);
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
    if timestamp.is_none_or(|timestamp| (now - timestamp).abs() > REQUEST_SKEW_SECONDS) {
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

pub async fn status(State(app): State<Arc<crate::AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authenticated_request(&app, &headers) {
        return *response;
    }
    let mut providers = Vec::new();
    for provider in [
        "qwen",
        "metaai",
        "claude",
        "chatgpt",
        "gemini",
        "grok",
        "mistral",
        "perplexity",
    ] {
        let session_present = match crate::session::load_session_file(provider) {
            Ok(session) => session.is_some(),
            Err(_) => {
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "session_status_unavailable",
                    "provider session status could not be read; check vault configuration",
                );
            }
        };
        providers.push(serde_json::json!({"provider":provider,"linked":session_present}));
    }
    Json(serde_json::json!({"ok":true,"providers":providers,"vault_mode":std::env::var("CHAT_GATEWAY_SESSION_STORE").as_deref()==Ok("vault")})).into_response()
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
    match crate::session::import_relay_session(&provider, &file) {
        Ok(()) => {
            Json(serde_json::json!({"ok":true,"provider":provider,"linked":true})).into_response()
        }
        Err(error) => {
            tracing::warn!(provider, error = %error, "browser relay session import rejected");
            json_error(
                StatusCode::BAD_REQUEST,
                "session_import_rejected",
                "session was rejected; check vault configuration and provider permissions",
            )
        }
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
    match app.pool.unlink_session(&provider).await {
        Ok(removed) => {
            tracing::info!(
                provider,
                "browser relay provider unlinked from embedded browser and persistent storage"
            );
            Json(serde_json::json!({"ok":true,"provider":provider,"unlinked":removed}))
                .into_response()
        }
        Err(error) => {
            tracing::warn!(provider, error = %error, "browser relay provider unlink incomplete");
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unlink_failed",
                "provider session could not be fully revoked; saved data was retained when cleanup failed",
            )
        }
    }
}

pub async fn unpair(State(app): State<Arc<crate::AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authenticated_request(&app, &headers) {
        return *response;
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

pub async fn preflight(headers: HeaderMap) -> Response {
    if !relay_origin_is_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    let origin = format!("chrome-extension://{}", extension_id().unwrap_or_default());
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

    fn relay_state_with_token(token: Option<&str>) -> RelayState {
        let install_token_hash = token.map(|token| {
            let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
            hash
        });
        RelayState {
            pairing_hash: [0; 32],
            pairing_expires: Instant::now() + PAIRING_LIFETIME,
            pairing_attempts: Mutex::new(0),
            install_token_hash: Mutex::new(install_token_hash),
            used_nonces: Mutex::new(HashMap::new()),
            request_times: Mutex::new(VecDeque::new()),
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
        assert!(state.pairing_expires > Instant::now());
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
