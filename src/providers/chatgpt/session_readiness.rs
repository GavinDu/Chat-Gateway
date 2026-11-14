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
        "(() => {{ const p=globalThis.__chatGatewayChatgptSessionReadiness; return p?.attempt === {attempt_json} ? {{state:p.state,http:p.http}} : null; }})()"
    );
    let cleanup = format!(
        "(() => {{ const p=globalThis.__chatGatewayChatgptSessionReadiness; if(p?.attempt === {attempt_json}) {{ clearTimeout(p.timer); if(p.state === 'pending') {{ try {{p.controller.abort();}} catch(_) {{}} }} delete globalThis.__chatGatewayChatgptSessionReadiness; }} }})()"
    );
    let deadline = tokio::time::Instant::now() + timeout;
    let result = loop {
        let value = page.evaluate(&read);
        match value["state"].as_str() {
            Some("verified") => {
                tracing::debug!(
                    provider = "chatgpt",
                    stage = "authenticated_session_readiness",
                    outcome = "verified",
                    "provider readiness check completed"
                );
                break Ok(());
            }
            Some("needs_login") => {
                tracing::warn!(
                    provider = "chatgpt",
                    stage = "authenticated_session_readiness",
                    outcome = "needs_login",
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
                    http = value["http"].as_u64(),
                    "provider readiness check completed"
                );
                break Err(anyhow!(
                    "ChatGPT authenticated session readiness unavailable"
                ));
            }
            Some("timeout") => {
                break Err(anyhow!("ChatGPT authenticated session readiness timed out"))
            }
            _ => {}
        }
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            break Err(anyhow!("ChatGPT authenticated session readiness timed out"));
        };
        crate::providers::common::drive_page(page, remaining.min(Duration::from_millis(100))).await;
    };
    page.evaluate(&cleanup);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
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
    async fn send_sequence_never_types_or_sends_before_authenticated_readiness() {
        for status in [200, 401] {
            let mut page=page_with_fetch(r#"(() => {
                document.documentElement.setAttribute('data-chat-gateway-test-fixture','');
                document.body.innerHTML='<form><textarea id="prompt-textarea"></textarea><button data-testid="send-button" aria-label="Send prompt" type="button">Send</button></form>';
                globalThis.fixtureSessionReady=false;globalThis.fixtureEarlySend=false;globalThis.fixtureSends=0;
                globalThis.fetch=async()=>{
                    await new Promise(resolve=>setTimeout(resolve,20));
                    fixtureSessionReady=fixtureStatus===200;
                    return new Response(fixtureSessionReady?JSON.stringify({user:{fixture:true},accessToken:'not-a-real-token',expires:new Date(Date.now()+60000).toISOString()}):'{}',{status:fixtureStatus});
                };
                document.querySelector('button').addEventListener('click',()=>{fixtureEarlySend=!fixtureSessionReady;fixtureSends++;});
            })()"#).await;
            page.evaluate(&format!("globalThis.fixtureStatus={status}"));
            let adapter = crate::providers::chatgpt::ChatGPTAdapter {
                cfg: serde_yaml::from_str("name: chatgpt\nmodels: [chatgpt]").unwrap(),
            };
            let result =
                crate::providers::SiteAdapter::send_prompt(&adapter, &mut page, "fixture prompt")
                    .await;
            if status == 200 {
                result.unwrap();
                assert_eq!(page.evaluate("fixtureSends").as_f64(), Some(1.0));
                assert_eq!(page.evaluate("fixtureEarlySend").as_bool(), Some(false));
            } else {
                assert!(result
                    .unwrap_err()
                    .downcast_ref::<crate::browser::pool::ProviderSessionExpiredError>()
                    .is_some());
                assert_eq!(page.evaluate("fixtureSends").as_f64(), Some(0.0));
                assert_eq!(
                    page.evaluate("document.querySelector('textarea').value")
                        .as_str(),
                    Some("")
                );
            }
        }
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
}
