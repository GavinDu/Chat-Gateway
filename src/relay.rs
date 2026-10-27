use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use ring::{constant_time, rand::{SecureRandom, SystemRandom}};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const PAIRING_LIFETIME: Duration = Duration::from_secs(10 * 60);
const REQUEST_SKEW_SECONDS: i64 = 60;
const NONCE_RETENTION: Duration = Duration::from_secs(5 * 60);
const MAX_PAIR_ATTEMPTS: u8 = 5;

pub struct RelayState {
    pairing_hash: [u8; 32],
    pairing_expires: Instant,
    pairing_attempts: Mutex<u8>,
    install_token_hash: Mutex<Option<[u8; 32]>>,
    used_nonces: Mutex<HashMap<String, Instant>>,
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
                install_token_hash: Mutex::new(None),
                used_nonces: Mutex::new(HashMap::new()),
            },
            code,
        ))
    }
}

#[derive(Deserialize)]
pub struct PairRequest {
    code: String,
}

fn json_error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (status, Json(serde_json::json!({"error":{"code":code,"message":message}}))).into_response()
}

fn extension_id() -> Option<String> {
    let id = std::env::var("CHAT_GATEWAY_EXTENSION_ID").ok()?;
    if id.len() != 32 || !id.bytes().all(|b| (b'a'..=b'p').contains(&b)) {
        return None;
    }
    Some(id)
}

/// The extension origin must match one configured Chrome extension ID exactly.
pub fn relay_origin_is_allowed(headers: &HeaderMap) -> bool {
    let Some(id) = extension_id() else { return false; };
    let expected = format!("chrome-extension://{id}");
    headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| origin == expected)
}

pub async fn pair(
    State(app): State<Arc<crate::AppState>>,
    headers: HeaderMap,
    Json(request): Json<PairRequest>,
) -> Response {
    if !relay_origin_is_allowed(&headers) {
        return json_error(StatusCode::FORBIDDEN, "extension_origin_rejected", "the configured browser extension origin is required");
    }
    if request.code.len() != 8 || !request.code.bytes().all(|b| b.is_ascii_digit()) {
        return json_error(StatusCode::UNAUTHORIZED, "pairing_rejected", "pairing code is invalid or expired");
    }
    let mut attempts = match app.relay.pairing_attempts.lock() {
        Ok(value) => value,
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "relay_unavailable", "relay state is unavailable"),
    };
    if *attempts >= MAX_PAIR_ATTEMPTS || Instant::now() >= app.relay.pairing_expires {
        return json_error(StatusCode::UNAUTHORIZED, "pairing_rejected", "pairing code is invalid or expired");
    }
    *attempts = attempts.saturating_add(1);
    let candidate: [u8; 32] = Sha256::digest(request.code.as_bytes()).into();
    if constant_time::verify_slices_are_equal(&candidate, &app.relay.pairing_hash).is_err() {
        return json_error(StatusCode::UNAUTHORIZED, "pairing_rejected", "pairing code is invalid or expired");
    }
    let mut token_slot = match app.relay.install_token_hash.lock() {
        Ok(value) => value,
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "relay_unavailable", "relay state is unavailable"),
    };
    if token_slot.is_some() {
        return json_error(StatusCode::CONFLICT, "already_paired", "this gateway already has a paired extension");
    }
    let mut random = [0_u8; 32];
    if SystemRandom::new().fill(&mut random).is_err() {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "relay_unavailable", "could not create install token");
    }
    let token = random.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    *token_slot = Some(token_hash);
    // A successful exchange consumes the pairing code permanently.
    *attempts = MAX_PAIR_ATTEMPTS;
    Json(serde_json::json!({"ok":true,"token":token})).into_response()
}

fn authenticated_request(app: &crate::AppState, headers: &HeaderMap) -> Result<(), Response> {
    if !relay_origin_is_allowed(headers) {
        return Err(json_error(StatusCode::FORBIDDEN, "extension_origin_rejected", "the configured browser extension origin is required"));
    }
    let authorization = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    let token = authorization.and_then(|value| value.strip_prefix("Bearer "));
    let Some(token) = token else {
        return Err(json_error(StatusCode::UNAUTHORIZED, "relay_auth_required", "a paired extension token is required"));
    };
    let candidate: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let token_slot = app.relay.install_token_hash.lock().map_err(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "relay_unavailable", "relay state is unavailable"))?;
    let Some(expected) = token_slot.as_ref() else {
        return Err(json_error(StatusCode::UNAUTHORIZED, "relay_auth_required", "the extension is not paired"));
    };
    if constant_time::verify_slices_are_equal(&candidate, expected).is_err() {
        return Err(json_error(StatusCode::UNAUTHORIZED, "relay_auth_required", "the paired extension token is invalid"));
    }
    drop(token_slot);

    let timestamp = headers.get("x-relay-timestamp").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok());
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    if timestamp.map_or(true, |timestamp| (now - timestamp).abs() > REQUEST_SKEW_SECONDS) {
        return Err(json_error(StatusCode::UNAUTHORIZED, "relay_request_expired", "relay request timestamp is missing or expired"));
    }
    let nonce = headers.get("x-relay-nonce").and_then(|v| v.to_str().ok()).unwrap_or("");
    if nonce.len() < 32 || nonce.len() > 128 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(json_error(StatusCode::BAD_REQUEST, "relay_nonce_invalid", "relay request nonce is invalid"));
    }
    let mut nonces = app.relay.used_nonces.lock().map_err(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "relay_unavailable", "relay state is unavailable"))?;
    nonces.retain(|_, inserted| inserted.elapsed() < NONCE_RETENTION);
    if nonces.contains_key(nonce) {
        return Err(json_error(StatusCode::UNAUTHORIZED, "relay_nonce_reused", "relay request nonce has already been used"));
    }
    nonces.insert(nonce.to_owned(), Instant::now());
    Ok(())
}

pub async fn status(State(app): State<Arc<crate::AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authenticated_request(&app, &headers) { return response; }
    let providers = ["qwen", "metaai", "claude", "chatgpt", "gemini", "grok", "mistral", "perplexity"]
        .iter()
        .map(|provider| {
            let session_present = crate::session::load_session_file(provider).ok().flatten().is_some();
            serde_json::json!({"provider":provider,"linked":session_present})
        })
        .collect::<Vec<_>>();
    Json(serde_json::json!({"ok":true,"providers":providers,"vault_mode":std::env::var("CHAT_GATEWAY_SESSION_STORE").as_deref()==Ok("vault")})).into_response()
}

pub async fn import_session(
    State(app): State<Arc<crate::AppState>>,
    headers: HeaderMap,
    Path(provider): Path<String>,
    Json(file): Json<crate::session::SessionFile>,
) -> Response {
    if let Err(response) = authenticated_request(&app, &headers) { return response; }
    match crate::session::import_relay_session(&provider, &file) {
        Ok(()) => Json(serde_json::json!({"ok":true,"provider":provider,"linked":true})).into_response(),
        Err(error) => {
            tracing::warn!(provider, error = %error, "browser relay session import rejected");
            json_error(StatusCode::BAD_REQUEST, "session_import_rejected", "session was rejected; check vault configuration and provider permissions")
        }
    }
}

pub async fn unlink(
    State(app): State<Arc<crate::AppState>>,
    headers: HeaderMap,
    Path(provider): Path<String>,
) -> Response {
    if let Err(response) = authenticated_request(&app, &headers) { return response; }
    match crate::session::delete_session_snapshot(&provider) {
        Ok(removed) => Json(serde_json::json!({"ok":true,"provider":provider,"unlinked":removed})).into_response(),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "unlink_failed", "provider session could not be removed"),
    }
}

pub async fn unpair(State(app): State<Arc<crate::AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authenticated_request(&app, &headers) { return response; }
    match app.relay.install_token_hash.lock() {
        Ok(mut token) => {
            *token = None;
            Json(serde_json::json!({"ok":true,"paired":false})).into_response()
        }
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "relay_unavailable", "relay state is unavailable"),
    }
}

pub async fn preflight(headers: HeaderMap) -> Response {
    if !relay_origin_is_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    let origin = format!("chrome-extension://{}", extension_id().unwrap_or_default());
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&origin) { headers.insert("access-control-allow-origin", value); }
    headers.insert("access-control-allow-methods", HeaderValue::from_static("GET, POST, DELETE, OPTIONS"));
    headers.insert("access-control-allow-headers", HeaderValue::from_static("authorization, content-type, x-relay-timestamp, x-relay-nonce"));
    headers.insert("vary", HeaderValue::from_static("Origin"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_code_is_numeric_and_state_has_a_short_expiry() {
        let (state, code) = RelayState::new().unwrap();
        assert_eq!(code.len(), 8);
        assert!(code.bytes().all(|byte| byte.is_ascii_digit()));
        assert!(state.pairing_expires > Instant::now());
    }

    #[test]
    fn extension_origin_is_rejected_when_not_configured() {
        let headers = HeaderMap::new();
        assert!(!relay_origin_is_allowed(&headers));
    }
}
