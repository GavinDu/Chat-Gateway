use super::*;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::{
    rand::{SecureRandom, SystemRandom},
    signature,
};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{oneshot, Mutex};

const TOKEN_ENDPOINT: &str = "https://auth.openai.com/api/accounts/oauth/token";
fn random() -> Result<String, GatewayError> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| failure("Secure random generator unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn profile_valid(profile: &str) -> bool {
    !profile.is_empty()
        && profile.len() <= 32
        && profile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
fn auth_url(url: &str) -> Result<reqwest::Url, GatewayError> {
    let parsed =
        reqwest::Url::parse(url).map_err(|_| failure("Invalid OpenAI discovery endpoint"))?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("auth.openai.com")
        || parsed.port_or_known_default() != Some(443)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(failure("Invalid OpenAI discovery endpoint"));
    }
    Ok(parsed)
}
async fn discovery(client: &reqwest::Client) -> Result<Value, GatewayError> {
    let response = client
        .get(format!("{AUTH_ORIGIN}/.well-known/openid-configuration"))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|_| failure("OpenAI identity discovery failed"))?;
    if !response.status().is_success() {
        return Err(failure("OpenAI identity discovery failed"));
    }
    let document = bounded_json(response).await?;
    if document["issuer"] != AUTH_ORIGIN {
        return Err(failure("Invalid OpenAI identity issuer"));
    }
    Ok(document)
}
fn verify_id_token(
    token: &str,
    keys: &Value,
    client_id: &str,
    nonce: Option<&str>,
    subject: Option<&str>,
) -> Result<(String, String), GatewayError> {
    let invalid = || failure("OpenAI identity token could not be verified");
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 || token.len() > 32768 {
        return Err(invalid());
    }
    let header: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).map_err(|_| invalid())?)
            .map_err(|_| invalid())?;
    if header["alg"] != "RS256"
        || header
            .get("crit")
            .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
    {
        return Err(invalid());
    }
    let kid = header["kid"].as_str().ok_or_else(invalid)?;
    let candidates: Vec<_> = keys["keys"]
        .as_array()
        .ok_or_else(invalid)?
        .iter()
        .filter(|k| {
            k["kid"] == kid
                && k["kty"] == "RSA"
                && k.get("use").is_none_or(|v| v == "sig")
                && k.get("alg").is_none_or(|v| v == "RS256")
        })
        .collect();
    if candidates.len() != 1 {
        return Err(invalid());
    }
    let key = candidates[0];
    let n = URL_SAFE_NO_PAD
        .decode(key["n"].as_str().ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
    let e = URL_SAFE_NO_PAD
        .decode(key["e"].as_str().ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
    let sig = URL_SAFE_NO_PAD.decode(parts[2]).map_err(|_| invalid())?;
    signature::RsaPublicKeyComponents { n, e }
        .verify(
            &signature::RSA_PKCS1_2048_8192_SHA256,
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &sig,
        )
        .map_err(|_| invalid())?;
    let claims: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).map_err(|_| invalid())?)
            .map_err(|_| invalid())?;
    let audience = claims["aud"].as_str() == Some(client_id)
        || claims["aud"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == client_id));
    let multiple_audiences = claims["aud"].as_array().is_some_and(|a| a.len() > 1);
    if multiple_audiences && claims["azp"].as_str() != Some(client_id) {
        return Err(invalid());
    }
    let sub = claims["sub"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(invalid)?;
    if claims["iss"] != AUTH_ORIGIN
        || !audience
        || claims["exp"].as_u64().is_none_or(|expiry| expiry <= now())
        || claims["nbf"]
            .as_u64()
            .is_some_and(|start| start > now() + 30)
        || subject.is_some_and(|expected| expected != sub)
    {
        return Err(invalid());
    }
    if let Some(nonce) = nonce {
        use subtle::ConstantTimeEq;
        if claims["nonce"]
            .as_str()
            .is_none_or(|actual| actual.as_bytes().ct_eq(nonce.as_bytes()).unwrap_u8() != 1)
        {
            return Err(invalid());
        }
    }
    Ok((sub.into(), claims["email"].as_str().unwrap_or("").into()))
}
async fn identity(
    client: &reqwest::Client,
    token: &str,
    client_id: &str,
    nonce: Option<&str>,
    subject: Option<&str>,
) -> Result<(String, String), GatewayError> {
    let document = discovery(client).await?;
    let url = auth_url(
        document["jwks_uri"]
            .as_str()
            .ok_or_else(|| failure("OpenAI signing keys unavailable"))?,
    )?;
    let response = client
        .get(url)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|_| failure("OpenAI signing keys unavailable"))?;
    if !response.status().is_success() {
        return Err(failure("OpenAI signing keys unavailable"));
    }
    verify_id_token(
        token,
        &bounded_json(response).await?,
        client_id,
        nonce,
        subject,
    )
}
fn parsed_scopes(value: &Value) -> Result<Vec<String>, GatewayError> {
    let scopes: Vec<String> = value["scope"]
        .as_str()
        .ok_or_else(login_required)?
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    Ok(scopes)
}
fn required_token(value: &Value, key: &str) -> Result<String, GatewayError> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 32768 && !s.contains(['\r', '\n']))
        .map(str::to_owned)
        .ok_or_else(|| failure("OpenAI token response is incomplete"))
}
fn expiry(value: &Value) -> Result<u64, GatewayError> {
    let seconds = value["expires_in"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= 604800)
        .ok_or_else(|| failure("OpenAI token expiry is invalid"))?;
    now()
        .checked_add(seconds)
        .ok_or_else(|| failure("OpenAI token expiry is invalid"))
}

struct Callback {
    state: String,
    host: String,
    authorization_url: String,
    result: Mutex<Option<oneshot::Sender<HashMap<String, String>>>>,
}
async fn connect_page(
    State(pending): State<Arc<Callback>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if headers.get("host").and_then(|h| h.to_str().ok()) != Some(pending.host.as_str()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let url = pending
        .authorization_url
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let html = format!(
        r#"<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Connect ChatGPT · Chat Gateway</title><style>body{{font:18px system-ui;max-width:36rem;margin:15vh auto;padding:1.5rem}}a{{display:inline-block;background:#111;color:white;padding:1rem 1.5rem;border-radius:.7rem;text-decoration:none}}</style><h1>Connect ChatGPT</h1><p>Connect your account to Chat Gateway. You’ll review permission to use your ChatGPT plan.</p><a href="{url}">Continue with ChatGPT</a><p>You can manage access and usage in ChatGPT settings.</p></html>"#
    );
    ([(axum::http::header::CACHE_CONTROL,"no-store"),(axum::http::header::CONTENT_SECURITY_POLICY,"default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; frame-ancestors 'none'"),(axum::http::header::REFERRER_POLICY,"no-referrer"),(axum::http::header::X_CONTENT_TYPE_OPTIONS,"nosniff")],axum::response::Html(html)).into_response()
}

async fn callback(
    State(pending): State<Arc<Callback>>,
    headers: HeaderMap,
    Query(values): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    use subtle::ConstantTimeEq;
    let state_matches = values
        .get("state")
        .is_some_and(|s| s.as_bytes().ct_eq(pending.state.as_bytes()).unwrap_u8() == 1);
    if !state_matches
        || headers.get("host").and_then(|h| h.to_str().ok()) != Some(pending.host.as_str())
    {
        return (
            StatusCode::BAD_REQUEST,
            "Invalid or expired sign-in response.",
        );
    }
    if let Some(result) = pending.result.lock().await.take() {
        let _ = result.send(values);
        (
            StatusCode::OK,
            "Sign-in received. You can close this tab and return to Chat Gateway.",
        )
    } else {
        (
            StatusCode::BAD_REQUEST,
            "This sign-in response was already used.",
        )
    }
}

pub(crate) async fn connect(port: u16, profile: &str, enable_plan: bool) -> anyhow::Result<()> {
    if !profile_valid(profile) || port == 0 {
        anyhow::bail!("Invalid ChatGPT profile or callback port");
    }
    let client = http_client()?;
    let guard = storage::lock().await?;
    let store = storage::load(&guard)?;
    storage::save(&guard, &store)?; // Persist the installation's host ID before registration.
    let registration = store.profiles.get(profile).cloned().unwrap_or_default();
    let host_id = store.host_id.clone();
    drop(guard);
    let state = random()?;
    let nonce = random()?;
    let verifier = zeroize::Zeroizing::new(random()?);
    let redirect = format!("http://127.0.0.1:{port}/auth/callback");
    let bind = if std::env::var("CHAT_GATEWAY_DOCKER_LOCAL_BIND").as_deref() == Ok("1") {
        std::net::Ipv4Addr::UNSPECIFIED
    } else {
        std::net::Ipv4Addr::LOCALHOST
    };
    let listener = tokio::net::TcpListener::bind((bind, port))
        .await
        .map_err(|_| failure("ChatGPT callback port unavailable"))?;
    let mut url = reqwest::Url::parse(&format!("{AUTH_ORIGIN}/api/accounts/authorize"))?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair(
                "client_id",
                if registration.client_id.is_empty() {
                    "dynamic_agent_client"
                } else {
                    &registration.client_id
                },
            )
            .append_pair("ext_agent_host_id", &host_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect)
            .append_pair(
                "scope",
                "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
            )
            .append_pair("resource", RESOURCE)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair(
                "code_challenge",
                &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            );
        if registration.client_id.is_empty() {
            query.append_pair("agent_name_hint", "Chat Gateway");
        }
        if let Some(credential) = &registration.credential {
            if !credential.id_token.is_empty() {
                query.append_pair("id_token_hint", &credential.id_token);
            }
            if !credential.email.is_empty() {
                query.append_pair("login_hint", &credential.email);
            }
        }
        if enable_plan {
            query.append_pair("prompt", "consent");
        }
    }
    let (tx, rx) = oneshot::channel();
    let pending = Arc::new(Callback {
        state,
        host: format!("127.0.0.1:{port}"),
        authorization_url: url.to_string(),
        result: Mutex::new(Some(tx)),
    });
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/", get(connect_page))
                .route("/auth/callback", get(callback))
                .with_state(pending),
        )
        .with_graceful_shutdown(async {
            let _ = stopped.await;
        })
        .await
    });
    // No ID/access/refresh token is placed in this URL, output or logs.
    println!("http://127.0.0.1:{port}/");
    let received = tokio::time::timeout(Duration::from_secs(600), rx).await;
    let _ = stop.send(());
    let _ = server.await;
    let values = received
        .map_err(|_| failure("ChatGPT sign-in expired"))?
        .map_err(|_| failure("ChatGPT sign-in interrupted"))?;
    if values.contains_key("error") {
        anyhow::bail!("ChatGPT sign-in was declined or could not be completed");
    }
    let code = zeroize::Zeroizing::new(
        values
            .get("code")
            .filter(|s| !s.is_empty() && s.len() <= 4096)
            .cloned()
            .ok_or_else(|| failure("ChatGPT authorization code missing"))?,
    );
    let issued = if registration.client_id.is_empty() {
        values
            .get("client_id")
            .filter(|s| !s.is_empty() && s.len() <= 512 && s.as_str() != "dynamic_agent_client")
            .cloned()
            .ok_or_else(|| failure("ChatGPT registration incomplete"))?
    } else {
        if values
            .get("client_id")
            .is_some_and(|id| id != &registration.client_id)
        {
            return Err(failure("ChatGPT registration changed during sign-in").into());
        }
        registration.client_id.clone()
    };
    let guard = storage::lock().await?;
    let mut latest = storage::load(&guard)?;
    if latest
        .profiles
        .get(profile)
        .is_some_and(|entry| !entry.client_id.is_empty() && entry.client_id != issued)
    {
        return Err(failure("ChatGPT registration changed during sign-in").into());
    }
    latest.profiles.entry(profile.into()).or_default().client_id = issued.clone();
    storage::save(&guard, &latest)?;
    drop(guard);
    let response = client
        .post(TOKEN_ENDPOINT)
        .timeout(Duration::from_secs(20))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", issued.as_str()),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("resource", RESOURCE),
        ])
        .send()
        .await
        .map_err(|_| {
            failure("ChatGPT code exchange failed; reconnect using the saved registration")
        })?;
    if !response.status().is_success() {
        return Err(failure(
            "ChatGPT code exchange failed; reconnect using the saved registration",
        )
        .into());
    }
    let data = bounded_json(response).await?;
    let id_token = required_token(&data, "id_token")?;
    let subject = if !registration.subject.is_empty() {
        Some(registration.subject.as_str())
    } else {
        registration.credential.as_ref().map(|c| c.subject.as_str())
    };
    let (subject, email) = identity(&client, &id_token, &issued, Some(&nonce), subject).await?;
    let credential = Credential {
        authorization_id: uuid::Uuid::new_v4().to_string(),
        issuer: AUTH_ORIGIN.into(),
        subject,
        email,
        access_token: required_token(&data, "access_token")?,
        refresh_token: if data.get("refresh_token").is_some() {
            required_token(&data, "refresh_token")?
        } else {
            String::new()
        },
        id_token,
        scopes: parsed_scopes(&data)?,
        expires_at: expiry(&data)?,
    };
    if data["token_type"]
        .as_str()
        .is_none_or(|v| !v.eq_ignore_ascii_case("Bearer"))
    {
        return Err(failure("Invalid OpenAI token type").into());
    }
    let guard = storage::lock().await?;
    let mut latest = storage::load(&guard)?;
    let selected = latest
        .profiles
        .get_mut(profile)
        .ok_or_else(|| failure("ChatGPT registration changed during sign-in"))?;
    if selected.client_id != issued
        || (!selected.subject.is_empty() && selected.subject != credential.subject)
        || (!selected.issuer.is_empty() && selected.issuer != credential.issuer)
        || selected
            .credential
            .as_ref()
            .is_some_and(|c| c.subject != credential.subject || c.issuer != credential.issuer)
    {
        return Err(failure("ChatGPT account changed during sign-in").into());
    }
    let enabled = credential.plan_enabled();
    selected.subject = credential.subject.clone();
    selected.issuer = credential.issuer.clone();
    selected.credential = Some(credential);
    if enabled {
        latest.active = profile.into();
    }
    storage::save(&guard, &latest)?;
    if !enabled {
        println!("Account signed in; ChatGPT plan usage is disabled or incomplete. Use chatgpt-enable-plan to review access.");
        return Err(login_required().into());
    }
    println!("ChatGPT account connected; plan usage is enabled. Verify a completed request before relying on this connection.");
    println!("Manage plan access and usage: https://chatgpt.com/settings/usage");
    Ok(())
}

pub(super) async fn access_token(
    client: &reqwest::Client,
    force: bool,
) -> Result<zeroize::Zeroizing<String>, GatewayError> {
    let guard = storage::lock().await?;
    let key = crate::session::vault_key_from_env()
        .map_err(|_| failure("ChatGPT vault key unavailable"))?;
    access_token_locked(client, force, guard, &key, TOKEN_ENDPOINT).await
}
async fn access_token_locked(
    client: &reqwest::Client,
    force: bool,
    guard: storage::Guard,
    key: &[u8; 32],
    endpoint: &str,
) -> Result<zeroize::Zeroizing<String>, GatewayError> {
    let mut store = storage::load_with_key(&guard, key)?;
    let active = store.active.clone();
    let registration = store.profiles.get_mut(&active).ok_or_else(login_required)?;
    let current = registration
        .credential
        .as_ref()
        .ok_or_else(login_required)?;
    if !current.plan_enabled() {
        return Err(login_required());
    }
    if !force && current.expires_at > now() + 60 {
        return Ok(zeroize::Zeroizing::new(current.access_token.clone()));
    }
    let response = client
        .post(endpoint)
        .timeout(Duration::from_secs(20))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", registration.client_id.as_str()),
            ("refresh_token", current.refresh_token.as_str()),
            ("resource", RESOURCE),
        ])
        .send()
        .await
        .map_err(|_| failure("ChatGPT token renewal unavailable"))?;
    if !response.status().is_success() {
        let status = response.status();
        if status == reqwest::StatusCode::BAD_REQUEST {
            let error = bounded_json(response).await?;
            if matches!(
                error["error"].as_str(),
                Some(
                    "invalid_grant"
                        | "invalid_refresh_token"
                        | "token_expired"
                        | "refresh_token_expired"
                        | "refresh_token_invalidated"
                        | "refresh_token_reused"
                )
            ) {
                registration.credential = None;
                storage::save_with_key(&guard, &store, key)?;
                return Err(login_required());
            }
        }
        return Err(status_error(status));
    }
    let data = bounded_json(response).await?;
    if data["token_type"]
        .as_str()
        .is_none_or(|v| !v.eq_ignore_ascii_case("Bearer"))
    {
        return Err(failure("Invalid OpenAI token type"));
    }
    let mut updated = current.clone();
    if let Some(token) = data["id_token"].as_str() {
        let (subject, email) = identity(
            client,
            token,
            &registration.client_id,
            None,
            Some(&current.subject),
        )
        .await?;
        updated.subject = subject;
        updated.email = email;
        updated.id_token = token.into();
    }
    updated.access_token = required_token(&data, "access_token")?;
    updated.refresh_token = required_token(&data, "refresh_token")?;
    if data.get("scope").is_some() {
        updated.scopes = parsed_scopes(&data)?;
    }
    updated.expires_at = expiry(&data)?;
    let enabled = updated.plan_enabled();
    let token = zeroize::Zeroizing::new(updated.access_token.clone());
    registration.credential = Some(updated);
    storage::save_with_key(&guard, &store, key)?;
    if !enabled {
        return Err(login_required());
    }
    Ok(token)
}
pub(crate) async fn status() -> anyhow::Result<()> {
    let guard = storage::lock().await?;
    let store = storage::load(&guard)?;
    for (name, registration) in &store.profiles {
        println!(
            "profile={} active={} connected={} plan_usage={}",
            name,
            name == &store.active,
            registration.credential.is_some(),
            registration
                .credential
                .as_ref()
                .is_some_and(Credential::plan_enabled)
        );
    }
    if store.profiles.is_empty() {
        println!("ChatGPT account connection required.");
    }
    Ok(())
}
pub(crate) async fn logout(profile: &str) -> anyhow::Result<()> {
    if !profile_valid(profile) {
        anyhow::bail!("Invalid ChatGPT profile");
    }
    let client = http_client()?;
    let guard = storage::lock().await?;
    let mut store = storage::load(&guard)?;
    let registration = store.profiles.get_mut(profile).ok_or_else(login_required)?;
    let mut revoked = false;
    if let Some(credential) = &registration.credential {
        if let Ok(document) = discovery(&client).await {
            if let Some(url) = document["revocation_endpoint"]
                .as_str()
                .and_then(|u| auth_url(u).ok())
            {
                for delay in [0, 200, 1000] {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    match client
                        .post(url.clone())
                        .timeout(Duration::from_secs(5))
                        .form(&[
                            ("token", credential.refresh_token.as_str()),
                            ("token_type_hint", "refresh_token"),
                            ("client_id", registration.client_id.as_str()),
                        ])
                        .send()
                        .await
                    {
                        Ok(response) if response.status() == reqwest::StatusCode::OK => {
                            revoked = true;
                            break;
                        }
                        Ok(response) if !response.status().is_server_error() => break,
                        _ => {}
                    }
                }
            }
        }
    } else {
        revoked = true;
    }
    registration.credential = None;
    storage::save(&guard, &store)?;
    println!("ChatGPT credentials cleared; remote revocation confirmed={revoked}. Registration and host identity retained.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn rotating_refresh_is_serialized_and_failures_preserve_registration() {
        use axum::{routing::post, Json};
        use std::sync::atomic::{AtomicUsize, Ordering};
        for mode in [0u8, 1, 2, 3] {
            let root = std::env::temp_dir().join(format!("cg-refresh-{}", uuid::Uuid::new_v4()));
            let key = [31u8; 32];
            let guard = storage::try_lock_at(&root).unwrap();
            let mut store = Store::default();
            store.profiles.insert(
                "default".into(),
                super::super::Registration {
                    client_id: "fixture-client".into(),
                    subject: "fixture-subject".into(),
                    issuer: AUTH_ORIGIN.into(),
                    credential: Some(Credential {
                        authorization_id: "fixture-grant".into(),
                        issuer: AUTH_ORIGIN.into(),
                        subject: "fixture-subject".into(),
                        email: String::new(),
                        access_token: "fixture-old-access".into(),
                        refresh_token: "fixture-old-refresh".into(),
                        id_token: String::new(),
                        scopes: vec![DIRECT_SCOPE.into(), "resource.invoke".into()],
                        expires_at: now() - 1,
                    }),
                },
            );
            storage::save_with_key(&guard, &store, &key).unwrap();
            drop(guard);
            let calls = Arc::new(AtomicUsize::new(0));
            let recorded = calls.clone();
            let router = Router::new().route("/token", post(move |axum::Form(form): axum::Form<HashMap<String,String>>| {
                let calls = recorded.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(form.len(), 4);
                    assert_eq!(form["grant_type"], "refresh_token");
                    assert_eq!(form["refresh_token"], "fixture-old-refresh");
                    assert_eq!(form["client_id"], "fixture-client");
                    assert_eq!(form["resource"], RESOURCE);
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    match mode {
                        1 => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"temporary"}))),
                        2 => (StatusCode::BAD_REQUEST, Json(json!({"error":"invalid_grant"}))),
                        3 => (StatusCode::BAD_REQUEST, Json(json!({"error":"invalid_client"}))),
                        _ => (StatusCode::OK, Json(json!({"access_token":"fixture-new-access","refresh_token":"fixture-new-refresh","token_type":"Bearer","expires_in":3600}))),
                    }
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let client = reqwest::Client::new();
            let attempt = || async {
                let guard = storage::lock_at(&root).await.unwrap();
                access_token_locked(&client, false, guard, &key, &endpoint).await
            };
            if mode == 0 {
                let (a, b) = tokio::join!(attempt(), attempt());
                assert_eq!(a.unwrap().as_str(), "fixture-new-access");
                assert_eq!(b.unwrap().as_str(), "fixture-new-access");
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    1,
                    "only one rotating token may be consumed"
                );
            } else {
                assert!(attempt().await.is_err());
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            }
            let guard = storage::try_lock_at(&root).unwrap();
            let saved = storage::load_with_key(&guard, &key).unwrap();
            let registration = &saved.profiles["default"];
            assert_eq!(registration.client_id, "fixture-client");
            assert_eq!(registration.subject, "fixture-subject");
            assert_eq!(saved.host_id, store.host_id);
            if mode == 2 {
                assert!(registration.credential.is_none());
            } else {
                let credential = registration.credential.as_ref().unwrap();
                assert_eq!(
                    credential.refresh_token,
                    if mode == 0 {
                        "fixture-new-refresh"
                    } else {
                        "fixture-old-refresh"
                    }
                );
                assert_eq!(credential.authorization_id, "fixture-grant");
            }
            server.abort();
            drop(guard);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    fn signed(claims: &Value, algorithm: &str) -> String {
        let key = signature::RsaKeyPair::from_pkcs8(include_bytes!("fixtures/test-only-key.pk8"))
            .unwrap();
        let header = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&json!({"alg":algorithm,"kid":"fixture-key"})).unwrap());
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
        let message = format!("{header}.{payload}");
        let mut signature = vec![0; key.public().modulus_len()];
        key.sign(
            &signature::RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            message.as_bytes(),
            &mut signature,
        )
        .unwrap();
        format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature))
    }
    #[test]
    fn signed_identity_requires_issuer_audience_nonce_expiry_and_original_account() {
        let keys: Value =
            serde_json::from_str(include_str!("fixtures/test-only-jwk.json")).unwrap();
        let valid = json!({"iss":AUTH_ORIGIN,"aud":"fixture-client","sub":"fixture-subject","nonce":"fixture-nonce","exp":now()+300,"email":"fixture@example.test"});
        let token = signed(&valid, "RS256");
        assert_eq!(
            verify_id_token(
                &token,
                &keys,
                "fixture-client",
                Some("fixture-nonce"),
                Some("fixture-subject")
            )
            .unwrap()
            .0,
            "fixture-subject"
        );
        for (field, value) in [
            ("iss", json!("https://other.invalid")),
            ("aud", json!("another-client")),
            ("nonce", json!("another-nonce")),
            ("sub", json!("another-account")),
            ("exp", json!(now() - 1)),
            ("nbf", json!(now() + 600)),
        ] {
            let mut claims = valid.clone();
            claims[field] = value;
            assert!(
                verify_id_token(
                    &signed(&claims, "RS256"),
                    &keys,
                    "fixture-client",
                    Some("fixture-nonce"),
                    Some("fixture-subject")
                )
                .is_err(),
                "{field}"
            );
        }
        assert!(verify_id_token(
            &signed(&valid, "none"),
            &keys,
            "fixture-client",
            Some("fixture-nonce"),
            None
        )
        .is_err());
        let mut tampered = token.into_bytes();
        let last = tampered.len() - 2;
        tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
        assert!(verify_id_token(
            std::str::from_utf8(&tampered).unwrap(),
            &keys,
            "fixture-client",
            Some("fixture-nonce"),
            None
        )
        .is_err());
        let mut multi = valid;
        multi["aud"] = json!(["fixture-client", "other"]);
        assert!(verify_id_token(
            &signed(&multi, "RS256"),
            &keys,
            "fixture-client",
            Some("fixture-nonce"),
            None
        )
        .is_err());
        multi["azp"] = json!("fixture-client");
        assert!(verify_id_token(
            &signed(&multi, "RS256"),
            &keys,
            "fixture-client",
            Some("fixture-nonce"),
            None
        )
        .is_ok());
    }
    #[test]
    fn scope_and_endpoint_checks_do_not_accept_identity_only_or_lookalikes() {
        assert!(!plan_scopes(
            &parsed_scopes(&json!({"scope":"openid profile email"})).unwrap()
        ));
        assert!(plan_scopes(
            &parsed_scopes(&json!({"scope":"openid resource.invoke chatgpt.tokens.use.direct"}))
                .unwrap()
        ));
        for url in [
            "http://auth.openai.com/key",
            "https://auth.openai.com.evil.test/key",
            "https://user@auth.openai.com/key",
            "https://auth.openai.com:8443/key",
        ] {
            assert!(auth_url(url).is_err());
        }
        assert!(auth_url("https://auth.openai.com/.well-known/jwks.json").is_ok());
    }
    #[tokio::test]
    async fn connection_page_is_local_private_and_does_not_embed_script() {
        let (tx, _) = oneshot::channel();
        let pending = Arc::new(Callback {
            state: "fixture-state".into(),
            host: "127.0.0.1:14845".into(),
            authorization_url: "https://auth.openai.com/api/accounts/authorize?scope=a&state=b"
                .into(),
            result: Mutex::new(Some(tx)),
        });
        let mut headers = HeaderMap::new();
        headers.insert("host", "other.test".parse().unwrap());
        assert_eq!(
            connect_page(State(pending.clone()), headers.clone())
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        headers.insert("host", "127.0.0.1:14845".parse().unwrap());
        let response = connect_page(State(pending), headers).await;
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert!(response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'"));
        let body = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("Continue with ChatGPT"));
        assert!(html.contains("&amp;state=b"));
        assert!(!html.contains("<script"));
    }
    #[tokio::test]
    async fn wrong_state_wrong_host_and_replayed_callbacks_are_rejected() {
        let (tx, mut rx) = oneshot::channel();
        let pending = Arc::new(Callback {
            state: "expected".into(),
            host: "127.0.0.1:14845".into(),
            authorization_url: "https://auth.openai.com/api/accounts/authorize?fixture=1".into(),
            result: Mutex::new(Some(tx)),
        });
        let mut headers = HeaderMap::new();
        headers.insert("host", "localhost:14845".parse().unwrap());
        let values = HashMap::from([
            ("state".into(), "expected".into()),
            ("code".into(), "fixture-code".into()),
        ]);
        assert_eq!(
            callback(
                State(pending.clone()),
                headers.clone(),
                Query(values.clone())
            )
            .await
            .into_response()
            .status(),
            StatusCode::BAD_REQUEST
        );
        headers.insert("host", "127.0.0.1:14845".parse().unwrap());
        let mut wrong = values.clone();
        wrong.insert("state".into(), "wrong".into());
        assert_eq!(
            callback(State(pending.clone()), headers.clone(), Query(wrong))
                .await
                .into_response()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(
            callback(
                State(pending.clone()),
                headers.clone(),
                Query(values.clone())
            )
            .await
            .into_response()
            .status(),
            StatusCode::OK
        );
        assert_eq!(rx.await.unwrap()["code"], "fixture-code");
        assert_eq!(
            callback(State(pending), headers, Query(values))
                .await
                .into_response()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
}
