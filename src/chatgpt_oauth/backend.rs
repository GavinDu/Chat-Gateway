use super::{auth, bounded_json, failure, http_client, protocol::Decoder, status_error, RESOURCE};
use crate::{
    browser::pool::ObscuraPool,
    chat_backend::{
        BackendCapabilities, BackendId, ChatBackend, ChatEvent, ChatRequest, GatewayError,
        GatewayErrorKind, ProviderId,
    },
    config::{AppConfig, ProviderConfig, ProviderTransport},
};
use futures_util::StreamExt;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

pub(crate) struct RoutedBackend {
    browser: Arc<ObscuraPool>,
    oauth: Option<Arc<OAuthBackend>>,
}
impl RoutedBackend {
    pub(crate) fn new(cfg: &AppConfig, browser: Arc<ObscuraPool>) -> Result<Self, GatewayError> {
        let oauth = cfg
            .provider("chatgpt")
            .filter(|p| p.enabled && p.transport == ProviderTransport::ChatgptOauth)
            .map(|config| OAuthBackend::new(config.clone()).map(Arc::new))
            .transpose()?;
        Ok(Self { browser, oauth })
    }
}
#[async_trait::async_trait]
impl ChatBackend for RoutedBackend {
    fn id(&self) -> BackendId {
        BackendId::new("provider_router").expect("static id")
    }
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            streaming: true,
            model_selection: true,
            system_role: false,
            addressable_threads: false,
        }
    }
    async fn send_stream(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError> {
        if request.provider.as_str() == "chatgpt" {
            if let Some(oauth) = &self.oauth {
                return oauth.send_stream(request).await;
            }
        }
        ChatBackend::send_stream(self.browser.as_ref(), request).await
    }
    async fn models(&self, provider: ProviderId) -> Result<Vec<String>, GatewayError> {
        if provider.as_str() == "chatgpt" {
            if let Some(oauth) = &self.oauth {
                return oauth.models().await;
            }
        }
        ChatBackend::models(self.browser.as_ref(), provider).await
    }
}
#[derive(Clone)]
pub(super) struct OAuthBackend {
    client: reqwest::Client,
    config: ProviderConfig,
    api_base: String,
    #[cfg(test)]
    fixture_tokens: Option<Arc<std::sync::atomic::AtomicUsize>>,
}
impl OAuthBackend {
    pub(super) fn new(config: ProviderConfig) -> Result<Self, GatewayError> {
        Ok(Self {
            client: http_client()?,
            config,
            api_base: RESOURCE.into(),
            #[cfg(test)]
            fixture_tokens: None,
        })
    }
    async fn token(&self, force: bool) -> Result<zeroize::Zeroizing<String>, GatewayError> {
        #[cfg(test)]
        if let Some(counter) = &self.fixture_tokens {
            use std::sync::atomic::Ordering;
            if force {
                counter.fetch_add(1, Ordering::SeqCst);
            }
            return Ok(zeroize::Zeroizing::new(format!(
                "fixture-token-{}",
                counter.load(Ordering::SeqCst) + 1
            )));
        }
        auth::access_token(&self.client, force).await
    }
    async fn models_with_token(&self, token: &str) -> Result<Vec<String>, GatewayError> {
        let response = self
            .client
            .get(format!("{}/models", self.api_base))
            .bearer_auth(token)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|_| failure("ChatGPT model catalog unavailable"))?;
        if !response.status().is_success() {
            return Err(status_error(response.status()));
        }
        let data = bounded_json(response).await?;
        let models = data["models"]
            .as_array()
            .ok_or_else(|| failure("ChatGPT model catalog is invalid"))?
            .iter()
            .filter(|m| m["visibility"] == "list")
            .filter_map(|m| m["slug"].as_str())
            .filter(|s| {
                !s.is_empty()
                    && s.len() <= 128
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if models.is_empty() {
            return Err(failure("No ChatGPT plan models available"));
        }
        Ok(models)
    }
    pub(super) async fn models(&self) -> Result<Vec<String>, GatewayError> {
        let mut token = self.token(false).await?;
        match self.models_with_token(&token).await {
            Err(error) if error.kind == GatewayErrorKind::SessionExpired => {
                token = self.token(true).await?;
                self.models_with_token(&token).await
            }
            result => result,
        }
    }
    async fn run(
        &self,
        request: ChatRequest,
        tx: &mpsc::Sender<Result<ChatEvent, GatewayError>>,
    ) -> Result<(), GatewayError> {
        let mut token = self.token(false).await?;
        let models = match self.models_with_token(&token).await {
            Err(error) if error.kind == GatewayErrorKind::SessionExpired => {
                token = self.token(true).await?;
                self.models_with_token(&token).await?
            }
            result => result?,
        };
        let model = select_model(&request.model, self.config.oauth_model.as_deref(), &models)?;
        // The gateway already supplies the complete bounded, owner-scoped
        // transcript. Never send a browser thread or previous_response_id.
        let body = json!({"model":model,"input":[{"role":"user","content":request.prompt}],"store":false,"stream":true});
        let mut response = self
            .client
            .post(format!("{}/responses", self.api_base))
            .bearer_auth(token.as_str())
            .json(&body)
            .send()
            .await
            .map_err(|_| failure("ChatGPT generation request failed"))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            // A rejected authorization has not begun inference. Renew once;
            // never replay an accepted or partially streamed generation.
            token = self.token(true).await?;
            response = self
                .client
                .post(format!("{}/responses", self.api_base))
                .bearer_auth(token.as_str())
                .json(&body)
                .send()
                .await
                .map_err(|_| failure("ChatGPT generation request failed"))?;
        }
        if !response.status().is_success() {
            return Err(status_error(response.status()));
        }
        if response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_none_or(|v| {
                !v.split(';')
                    .next()
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
            })
        {
            return Err(failure("ChatGPT did not return a Responses event stream"));
        }
        let mut stream = response.bytes_stream();
        let mut decoder = Decoder::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| failure("ChatGPT response stream interrupted"))?;
            for event in decoder.push(&chunk)? {
                if tx.send(Ok(event)).await.is_err() {
                    return Ok(());
                }
            }
            if decoder.completed() {
                return Ok(());
            }
        }
        decoder.finish()
    }
    async fn send_stream_owned(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError> {
        let (tx, rx) = mpsc::channel(32);
        let backend = self.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                _=tx.closed()=>return,
                result=tokio::time::timeout(Duration::from_secs(backend.config.timeout_seconds),backend.run(request,&tx))=>result,
            };
            let error = match result {
                Ok(Ok(())) => return,
                Ok(Err(error)) => error,
                Err(_) => GatewayError::timeout("ChatGPT request deadline expired"),
            };
            let _ = tx.send(Err(error)).await;
        });
        Ok(rx)
    }
}
#[async_trait::async_trait]
impl ChatBackend for OAuthBackend {
    fn id(&self) -> BackendId {
        BackendId::new("chatgpt_oauth").expect("static id")
    }
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            streaming: true,
            model_selection: true,
            system_role: false,
            addressable_threads: false,
        }
    }
    async fn send_stream(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError> {
        if request.provider.as_str() != "chatgpt" {
            return Err(failure(
                "ChatGPT OAuth backend cannot serve another provider",
            ));
        }
        self.send_stream_owned(request).await
    }
    async fn models(&self, provider: ProviderId) -> Result<Vec<String>, GatewayError> {
        if provider.as_str() != "chatgpt" {
            return Err(failure(
                "ChatGPT OAuth backend cannot serve another provider",
            ));
        }
        OAuthBackend::models(self).await
    }
}

fn select_model<'a>(
    requested: &str,
    configured: Option<&str>,
    catalog: &'a [String],
) -> Result<&'a str, GatewayError> {
    let wanted = if matches!(requested, "chatgpt" | "gpt") {
        configured
    } else {
        Some(requested)
    };
    match wanted {
        Some(slug) => catalog
            .iter()
            .find(|m| m.as_str() == slug)
            .map(String::as_str)
            .ok_or_else(|| {
                GatewayError::typed(
                    GatewayErrorKind::ModelNotFound,
                    "Requested model is unavailable to this ChatGPT account",
                    false,
                )
            }),
        None => catalog
            .first()
            .map(String::as_str)
            .ok_or_else(|| failure("No ChatGPT model available")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{OriginalUri, State},
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::{get, post},
        Json, Router,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;
    struct Fixture {
        mode: u8,
        calls: AtomicUsize,
        requests: Mutex<Vec<serde_json::Value>>,
        auth: Mutex<Vec<String>>,
    }
    async fn fixture_models() -> impl IntoResponse {
        Json(
            json!({"models":[{"slug":"fixture-model","visibility":"list"},{"slug":"hidden-model","visibility":"hidden"}]}),
        )
    }
    async fn fixture_response(
        State(f): State<Arc<Fixture>>,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> Response {
        assert!(uri.query().is_none());
        let call = f.calls.fetch_add(1, Ordering::SeqCst);
        f.auth
            .lock()
            .await
            .push(headers["authorization"].to_str().unwrap().into());
        f.requests.lock().await.push(body);
        if f.mode == 1 && call == 0 {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        if f.mode == 4 {
            std::future::pending::<()>().await;
        }
        if f.mode == 5 {
            return StatusCode::FORBIDDEN.into_response();
        }
        if f.mode == 6 {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
        let wire = if f.mode == 2 {
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n"
        } else if f.mode == 3 {
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"subscription_sharing_usage_limit_exceeded\",\"message\":\"private diagnostic fixture\"}}}\n\n"
        } else {
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"fixture answer\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
        };
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            wire,
        )
            .into_response()
    }
    async fn fixture(mode: u8) -> (Arc<OAuthBackend>, Arc<Fixture>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let f = Arc::new(Fixture {
            mode,
            calls: AtomicUsize::new(0),
            requests: Mutex::new(vec![]),
            auth: Mutex::new(vec![]),
        });
        let app = Router::new()
            .route("/v1/models", get(fixture_models))
            .route("/v1/responses", post(fixture_response))
            .with_state(f.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let config = serde_yaml::from_str(
            "name: chatgpt\ntransport: chatgpt_oauth\nmodels: [chatgpt]\ntimeout_seconds: 1\n",
        )
        .unwrap();
        let backend = Arc::new(OAuthBackend {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            config,
            api_base: format!("http://{address}/v1"),
            fixture_tokens: Some(Arc::new(AtomicUsize::new(0))),
        });
        (backend, f, server)
    }
    fn request() -> ChatRequest {
        ChatRequest {
            provider: ProviderId::new("chatgpt").unwrap(),
            model: "chatgpt".into(),
            prompt: "bounded fixture transcript".into(),
            has_history: true,
            thread_url: Some("https://chatgpt.com/c/old-browser-thread".into()),
        }
    }
    async fn gateway(backend: Arc<OAuthBackend>) -> (Router, Arc<crate::AppState>) {
        let (old, state) =
            crate::real_chat_router_scripted_backend_tests::fixture(&["chatgpt"], vec![], None);
        drop(old);
        let mut state = Arc::try_unwrap(state).ok().expect("unique test state");
        let mut cfg = (*state.cfg).clone();
        cfg.providers[0].transport = ProviderTransport::ChatgptOauth;
        cfg.providers[0].models = vec!["chatgpt".into()];
        cfg.providers[0].timeout_seconds = 3;
        state.cfg = Arc::new(cfg);
        state.router = crate::ProviderRouter::new(state.cfg.clone());
        state.chat_service = crate::ChatService::new(backend);
        let state = Arc::new(state);
        let app = Router::new()
            .route("/v1/chat/completions", post(crate::chat_completions))
            .with_state(state.clone());
        (app, state)
    }
    async fn call(app: Router, stream: bool, conversation: &str) -> Response {
        use tower::ServiceExt;
        app.oneshot(axum::http::Request::builder().method("POST").uri("/v1/chat/completions").header("content-type","application/json").header("x-conversation-id",conversation).body(axum::body::Body::from(json!({"model":"chatgpt","stream":stream,"messages":[{"role":"user","content":"fixture prompt"}]}).to_string())).unwrap()).await.unwrap()
    }
    #[tokio::test]
    async fn actual_http_transport_and_gateway_json_sse_complete_and_commit_once() {
        for stream in [false, true] {
            let (backend, f, server) = fixture(0).await;
            let (app, state) = gateway(backend).await;
            let conversation = if stream {
                "oauth_stream_001"
            } else {
                "oauth_json_001"
            };
            let response = call(app, stream, conversation).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap();
            if stream {
                let text = std::str::from_utf8(&body).unwrap();
                assert!(text.contains("fixture answer"));
                assert!(text.contains("data: [DONE]"));
                assert!(text.contains("\"finish_reason\":\"stop\""));
            } else {
                let data: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(data["choices"][0]["message"]["content"], "fixture answer");
            }
            assert_eq!(
                state
                    .memory
                    .conversation_messages_for_owner("local", conversation)
                    .unwrap()
                    .len(),
                2
            );
            let requests = f.requests.lock().await;
            assert_eq!(requests.len(), 1);
            let body = &requests[0];
            assert_eq!(body["store"], false);
            assert_eq!(body["stream"], true);
            assert_eq!(body["model"], "fixture-model");
            assert!(body["input"].is_array());
            assert_eq!(body.as_object().unwrap().len(), 4);
            assert_eq!(f.auth.lock().await.as_slice(), ["Bearer fixture-token-1"]);
            assert!(body["input"][0]["content"]
                .as_str()
                .unwrap()
                .contains("fixture prompt"));
            server.abort();
        }
    }
    #[tokio::test]
    async fn rejected_authorization_renews_once_but_accepted_generation_is_not_replayed() {
        let (backend, f, server) = fixture(1).await;
        let service = crate::ChatService::new(backend.clone());
        let events = ChatBackend::send_stream(backend.as_ref(), request())
            .await
            .unwrap();
        let output = service
            .collect_with_timeout(events, std::time::Instant::now(), Duration::from_secs(3))
            .await;
        assert!(output.error.is_none());
        assert_eq!(output.content, "fixture answer");
        assert_eq!(f.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            f.auth.lock().await.as_slice(),
            ["Bearer fixture-token-1", "Bearer fixture-token-2"]
        );
        server.abort();
    }
    #[tokio::test]
    async fn incomplete_or_failed_inference_does_not_commit_and_is_never_replayed() {
        for mode in [2, 3, 4, 5, 6] {
            let (backend, f, server) = fixture(mode).await;
            let (app, state) = gateway(backend.clone()).await;
            let response = call(app, false, "oauth_failed_001").await;
            assert!(!response.status().is_success());
            if mode == 5 {
                assert_eq!(response.status(), StatusCode::FORBIDDEN);
            }
            if mode == 3 || mode == 6 {
                assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
                assert!(!response.headers().contains_key("retry-after"));
            }
            let bytes = axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("private diagnostic"));
            assert!(state
                .memory
                .conversation_messages_for_owner("local", "oauth_failed_001")
                .unwrap()
                .is_empty());
            assert_eq!(f.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                backend
                    .fixture_tokens
                    .as_ref()
                    .unwrap()
                    .load(Ordering::SeqCst),
                0
            );
            server.abort();
        }
    }
    #[tokio::test]
    async fn streaming_partial_failure_never_emits_success_or_commits() {
        let (backend, f, server) = fixture(2).await;
        let (app, state) = gateway(backend).await;
        let response = call(app, true, "oauth_partial_stream").await;
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("error"));
        assert!(!text.contains("\"finish_reason\":\"stop\""));
        assert!(state
            .memory
            .conversation_messages_for_owner("local", "oauth_partial_stream")
            .unwrap()
            .is_empty());
        assert_eq!(f.calls.load(Ordering::SeqCst), 1);
        server.abort();
    }
    #[tokio::test]
    async fn repeat_turn_replays_owned_history_without_browser_thread_or_remote_state() {
        let (backend, f, server) = fixture(0).await;
        let (app, state) = gateway(backend).await;
        for _ in 0..2 {
            let response = call(app.clone(), false, "oauth_history").await;
            assert_eq!(response.status(), StatusCode::OK);
            axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap();
        }
        assert_eq!(
            state
                .memory
                .conversation_messages_for_owner("local", "oauth_history")
                .unwrap()
                .len(),
            4
        );
        let requests = f.requests.lock().await;
        let second = requests[1]["input"][0]["content"].as_str().unwrap();
        assert!(second.contains("fixture answer"));
        assert_eq!(second.matches("fixture prompt").count(), 2);
        assert!(requests[1].get("previous_response_id").is_none());
        assert_eq!(requests[1]["store"], false);
        server.abort();
    }
    #[test]
    fn aliases_use_the_account_catalog_without_silent_model_substitution() {
        let models = vec!["account-preferred".into(), "specific-model".into()];
        assert_eq!(
            select_model("chatgpt", None, &models).unwrap(),
            "account-preferred"
        );
        assert_eq!(
            select_model("chatgpt", Some("specific-model"), &models).unwrap(),
            "specific-model"
        );
        assert_eq!(
            select_model("specific-model", None, &models).unwrap(),
            "specific-model"
        );
        assert!(select_model("missing", None, &models).is_err());
        assert!(select_model("chatgpt", Some("missing"), &models).is_err());
    }
}
