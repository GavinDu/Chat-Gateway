//! A visible editor can precede usable authenticated client state.
use anyhow::{anyhow, Result};
use obscura_core::Page;
use std::time::Duration;

const ORIGIN: &str = "https://chatgpt.com";
const TIMEOUT: Duration = Duration::from_secs(10);

pub(super) async fn wait(page: &mut Page) -> Result<()> {
    #[cfg(test)]
    if page.url().starts_with("data:text/html,")
        && page
            .evaluate("document.documentElement.hasAttribute('data-chat-gateway-test-fixture')")
            .as_bool()
            == Some(true)
    {
        // Same in-memory fixture restriction as the adapter route guard.
        // Fixtures must still return a verified session through the real check.
        return wait_on_origin(page, "null", TIMEOUT).await;
    }
    wait_on_origin(page, ORIGIN, TIMEOUT).await
}

fn safe_reason(value: Option<&str>) -> &'static str {
    match value {
        Some("pending") => "pending",
        Some("verified") => "verified",
        Some("explicit_unauthenticated") => "explicit_unauthenticated",
        Some("off_origin") => "off_origin",
        Some("page_changed") => "page_changed",
        Some("deadline") => "deadline",
        Some("response_redirected") => "response_redirected",
        Some("response_url_missing") => "response_url_missing",
        Some("response_endpoint_mismatch") => "response_endpoint_mismatch",
        Some("http_unavailable") => "http_unavailable",
        Some("invalid_json_or_body") => "invalid_json_or_body",
        Some("invalid_session_schema") => "invalid_session_schema",
        Some("session_reported_error") => "session_reported_error",
        Some("user_missing") => "user_missing",
        Some("access_token_missing") => "access_token_missing",
        Some("access_token_invalid") => "access_token_invalid",
        Some("access_token_expired") => "access_token_expired",
        Some("session_expiry_invalid") => "session_expiry_invalid",
        Some("session_expiry_expired") => "session_expiry_expired",
        Some("request_failed") => "request_failed",
        _ => "unknown",
    }
}
#[test]
fn test_safe_reason() {
    assert_eq!(safe_reason(Some("pending")), "pending");
    assert_eq!(safe_reason(Some("verified")), "verified");
    assert_eq!(safe_reason(Some("request_failed")), "request_failed");
    assert_eq!(safe_reason(None), "unknown");
    assert_eq!(safe_reason(Some("sk-live-123")), "unknown");
    assert_eq!(
        safe_reason(Some("https://example.com/?token=abc")),
        "unknown"
    );
    assert_eq!(safe_reason(Some("session=abc; HttpOnly")), "unknown");
}

struct ReadinessAttempt<'a> {
    page: &'a mut Page,
    cleanup: String,
}
impl<'a> ReadinessAttempt<'a> {
    fn new(page: &'a mut Page, cleanup: String) -> Self {
        Self { page, cleanup }
    }
    fn page(&mut self) -> &mut Page {
        self.page
    }
}
impl Drop for ReadinessAttempt<'_> {
    fn drop(&mut self) {
        let _ = self.page.evaluate(&self.cleanup);
    }
}

async fn wait_on_origin(page: &mut Page, origin: &str, timeout: Duration) -> Result<()> {
    let attempt = uuid::Uuid::new_v4().to_string();
    let attempt_json = serde_json::to_string(&attempt)?;
    let script = include_str!("session_readiness.js")
        .replace("__ATTEMPT__", &attempt_json)
        .replace("__ORIGIN__", &serde_json::to_string(origin)?)
        .replace("__TIMEOUT_MS__", &timeout.as_millis().max(1).to_string());
    let initial = page.evaluate(&script);
    if initial["state"] == "off_origin" {
        return Err(anyhow!(
            "ChatGPT session check requires the expected origin"
        ));
    }
    let read = format!(
        "(() => {{ const p=globalThis.__chatGatewayChatgptSessionReadiness; return p?.attempt === {attempt_json} ? {{state:p.state,http:p.http,reason:p.reason}} : null; }})()"
    );
    let cleanup = format!(
        "(() => {{ const p=globalThis.__chatGatewayChatgptSessionReadiness; if(p?.attempt === {attempt_json}) {{ clearTimeout(p.timer); if(p.state === 'pending') {{ try {{p.controller.abort();}} catch(_) {{}} }} delete globalThis.__chatGatewayChatgptSessionReadiness; }} }})()"
    );
    let deadline = tokio::time::Instant::now() + timeout;
    let mut attempt = ReadinessAttempt::new(page, cleanup);
    let result = loop {
        let value = attempt.page().evaluate(&read);
        let reason = safe_reason(value["reason"].as_str());
        match value["state"].as_str() {
            Some("verified") => {
                tracing::debug!(
                    provider = "chatgpt",
                    stage = "authenticated_session_readiness",
                    outcome = "verified",
                    reason,
                    "provider readiness check completed"
                );
                break Ok(());
            }
            Some("needs_login") => {
                tracing::warn!(
                    provider = "chatgpt",
                    stage = "authenticated_session_readiness",
                    outcome = "needs_login",
                    reason,
                    "provider readiness check completed"
                );
                break Err(anyhow::Error::new(
                    crate::browser::pool::ProviderSessionExpiredError,
                ));
            }
            Some("unavailable") => {
                tracing::warn!(
                    provider = "chatgpt",
                    stage = "authenticated_session_readiness",
                    outcome = "unavailable",
                    reason,
                    http = value["http"].as_u64(),
                    "provider readiness check completed"
                );
                break Err(anyhow!(
                    "ChatGPT authenticated session readiness unavailable"
                ));
            }
            Some("timeout") => {
                tracing::warn!(
                    provider = "chatgpt",
                    stage = "authenticated_session_readiness",
                    outcome = "timeout",
                    reason = "deadline",
                    "provider readiness check completed"
                );
                break Err(anyhow!("ChatGPT authenticated session readiness timed out"));
            }
            _ => {}
        }
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            tracing::warn!(
                provider = "chatgpt",
                stage = "authenticated_session_readiness",
                outcome = "timeout",
                reason = "deadline",
                "provider readiness check completed"
            );
            break Err(anyhow!("ChatGPT authenticated session readiness timed out"));
        };
        crate::providers::common::drive_page(
            attempt.page(),
            remaining.min(Duration::from_millis(100)),
        )
        .await;
    };
    drop(attempt);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    async fn page_with_fetch(source: &str) -> Page {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<body>session fixture</body>")
            .await
            .unwrap();
        page.evaluate(source);
        page
    }
    #[tokio::test]
    async fn waits_for_verified_session_and_keeps_values_inside_page() {
        let mut page = page_with_fetch(r#"(() => {globalThis.fetchCalls=0;globalThis.fetchOptionsValid=false;globalThis.fetch=async(url,options)=>{
            fetchCalls++;fetchOptionsValid=url==='/api/auth/session'&&options.credentials==='include'&&options.cache==='no-store';
            await new Promise(resolve=>setTimeout(resolve,10));
            return new Response(JSON.stringify({user:{fixture:true},accessToken:'private-fixture-not-a-real-token',expires:new Date(Date.now()+60000).toISOString()}));
        };})()"#).await;
        wait_on_origin(&mut page, "null", Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(page.evaluate("fetchCalls").as_f64(), Some(1.0));
        assert_eq!(page.evaluate("fetchOptionsValid").as_bool(), Some(true));
        assert_eq!(
            page.evaluate("typeof globalThis.__chatGatewayChatgptSessionReadiness")
                .as_str(),
            Some("undefined")
        );
    }

    #[tokio::test]
    async fn failed_refresh_and_expired_access_token_cannot_verify_a_future_session() {
        let future = (chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339();
        let jwt = |expiry| {
            format!(
                "e30.{}.synthetic-signature",
                URL_SAFE_NO_PAD.encode(serde_json::json!({"exp": expiry}).to_string())
            )
        };
        for (token, error, verified) in [
            (
                "synthetic-opaque".to_owned(),
                Some("RefreshAccessTokenError"),
                false,
            ),
            (jwt(1), None, false),
            (jwt(chrono::Utc::now().timestamp() + 60), None, true),
        ] {
            let mut session = serde_json::json!({
                "user": {"fixture": true}, "accessToken": token, "expires": future
            });
            if let Some(error) = error {
                session["error"] = serde_json::json!(error);
            }
            let body = serde_json::to_string(&session).unwrap();
            let mut page = page_with_fetch(&format!(
                "globalThis.fetch=async()=>new Response({});",
                serde_json::to_string(&body).unwrap()
            ))
            .await;
            let result = wait_on_origin(&mut page, "null", Duration::from_secs(1)).await;
            assert_eq!(result.is_ok(), verified);
            if let Err(error) = result {
                assert!(error
                    .downcast_ref::<crate::browser::pool::ProviderSessionExpiredError>()
                    .is_none());
                assert!(!error.to_string().contains("RefreshAccessTokenError"));
                assert!(!error.to_string().contains("synthetic-signature"));
            }
        }
    }
    #[tokio::test]
    async fn only_explicit_unauthenticated_results_require_login() {
        for source in [
            "(() => {globalThis.fetch=async()=>new Response('{}',{status:401});})()",
            "(() => {globalThis.fetch=async()=>new Response('{}');})()",
        ] {
            let mut page = page_with_fetch(source).await;
            let error = wait_on_origin(&mut page, "null", Duration::from_secs(1))
                .await
                .unwrap_err();
            assert!(error
                .downcast_ref::<crate::browser::pool::ProviderSessionExpiredError>()
                .is_some());
        }
        for source in [
            "(() => {globalThis.fetch=async()=>new Response('private fixture detail',{status:403});})()",
            "(() => {globalThis.fetch=async()=>new Response('private fixture detail',{status:503});})()",
            "(() => {globalThis.fetch=async()=>new Response('private fixture detail');})()",
            "(() => {globalThis.fetch=async()=>new Response(JSON.stringify({user:{},accessToken:'private fixture',expires:'2000-01-01T00:00:00Z'}));})()",
        ] {
            let mut page=page_with_fetch(source).await;
            let error=wait_on_origin(&mut page,"null",Duration::from_secs(1)).await.unwrap_err();
            assert!(error.downcast_ref::<crate::browser::pool::ProviderSessionExpiredError>().is_none());
            assert!(!error.to_string().contains("private fixture"));
        }
    }
    #[tokio::test]
    async fn empty_user_object_with_token_and_future_expiry_is_not_account_proof() {
        // A fake token and expiry alongside an empty user object must never
        // make Obscura report an authenticated account. Do not infer logout.
        let mut partial = page_with_fetch(
            r#"(() => {globalThis.fetch=async()=>new Response(JSON.stringify({
                user:{}, accessToken:'private-test-token',
                expires:new Date(Date.now()+60000).toISOString()
            }),{status:200});})()"#,
        )
        .await;
        let failure = wait_on_origin(&mut partial, "null", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(failure
            .downcast_ref::<crate::browser::pool::ProviderSessionExpiredError>()
            .is_none());
        assert!(!failure.to_string().contains("private-test-token"));
        assert_eq!(
            partial
                .evaluate("typeof globalThis.__chatGatewayChatgptSessionReadiness")
                .as_str(),
            Some("undefined")
        );
    }

    #[tokio::test]
    async fn timeout_aborts_own_fetch_and_foreign_origins_are_not_contacted() {
        let mut page=page_with_fetch(r#"(() => {globalThis.aborted=false;globalThis.fetchCalls=0;globalThis.fetch=(url,options)=>{
            fetchCalls++;options.signal.addEventListener('abort',()=>aborted=true);return new Promise(()=>{});
        };})()"#).await;
        let error = wait_on_origin(&mut page, "null", Duration::from_millis(40))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert_eq!(page.evaluate("aborted").as_bool(), Some(true));
        wait(&mut page).await.unwrap_err();
        assert_eq!(page.evaluate("fetchCalls").as_f64(), Some(1.0));
    }

    #[tokio::test]
    async fn cancels_pending_fetch_and_clears_probe() {
        let mut page = page_with_fetch(r#"(() => { globalThis.capturedSignal = null; globalThis.fetch = (url, options) => { globalThis.capturedSignal = options.signal; return new Promise(() => {}); }; })()"#).await;
        let outcome = tokio::time::timeout(
            Duration::from_millis(150),
            wait_on_origin(&mut page, "null", Duration::from_secs(5)),
        )
        .await;
        assert!(outcome.is_err(), "{outcome:?}");
        let aborted: bool = page
            .evaluate("globalThis.capturedSignal.aborted")
            .as_bool()
            .unwrap();
        assert!(aborted);
        let probe: String = page
            .evaluate("typeof globalThis.__chatGatewayChatgptSessionReadiness")
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(probe, "undefined");
    }

    #[tokio::test]
    async fn older_cleanup_preserves_newer_attempt() {
        let newer = "newer-attempt-1600";
        let raw = include_str!("session_readiness.js");
        let script = raw
            .replace("__ATTEMPT__", &serde_json::to_string(newer).unwrap())
            .replace("__ORIGIN__", &serde_json::to_string("null").unwrap())
            .replace("__TIMEOUT_MS__", "5000");
        let mut source = String::from("(() => { globalThis.nextScript = ");
        source.push_str(&serde_json::to_string(&script).unwrap());
        source.push_str("; globalThis.fetchCalls = 0; globalThis.oldSignal = null; globalThis.newSignal = null; globalThis.fetch = (url, options) => { globalThis.fetchCalls += 1; if (globalThis.fetchCalls === 1) { globalThis.oldSignal = options.signal; setTimeout(() => eval(globalThis.nextScript), 0); } else { globalThis.newSignal = options.signal; } return new Promise(() => {}); };");
        source.push_str("})()");
        let mut page = page_with_fetch(&source).await;
        let outcome = tokio::time::timeout(
            Duration::from_millis(150),
            wait_on_origin(&mut page, "null", Duration::from_secs(5)),
        )
        .await;
        assert!(outcome.is_err(), "{outcome:?}");

        let attempt: String = page
            .evaluate("globalThis.__chatGatewayChatgptSessionReadiness.attempt")
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(attempt, newer);
        let old_aborted: bool = page
            .evaluate("globalThis.oldSignal.aborted")
            .as_bool()
            .unwrap();
        assert!(old_aborted);
        let new_aborted: bool = page
            .evaluate("globalThis.newSignal.aborted")
            .as_bool()
            .unwrap();
        assert!(!new_aborted);
    }
}
