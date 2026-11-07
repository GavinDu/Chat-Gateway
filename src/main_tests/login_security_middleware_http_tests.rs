use super::*;
use axum::{body::Body, routing::get};
use tower::ServiceExt;

fn app() -> Router {
    Router::new()
        .route("/login/api/status", get(|| async { "ok" }))
        .route("/health", get(|| async { "ok" }))
        .layer(middleware::from_fn(protect_login_api))
        .layer(middleware::from_fn(attach_request_id))
}

async fn request(uri: &str, headers: &[(&str, &str)]) -> Response {
    let mut builder = Request::builder().uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    app()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn remote_app(peer: Option<SocketAddr>, configured: bool) -> Router {
    let trust = configured.then(|| {
        RemoteProxyTrust::parse(
            "127.0.0.1",
            "edge-only-secret-0123456789abcdef0123456789",
            "api",
        )
        .unwrap()
    });
    let app = Router::new()
        .route(
            "/health",
            get(|headers: HeaderMap| async move {
                assert!(!headers.contains_key("x-chat-gateway-proxy-key"));
                StatusCode::OK
            }),
        )
        .route("/login", get(|| async { StatusCode::OK }))
        .layer(middleware::from_fn(|req, next| {
            protect_login_api_for_origin(
                req,
                next,
                Some((
                    "https://gateway.example.com".into(),
                    "gateway.example.com".into(),
                )),
            )
        }))
        .layer(Extension(trust));
    if let Some(peer) = peer {
        app.layer(Extension(ConnectInfo(peer)))
    } else {
        app
    }
}

#[tokio::test]
async fn remote_boundary_checks_real_peer_secret_headers_and_login_prohibition() {
    for (peer, configured, secret, tls, host, origin, path, expected) in [
        (
            Some("127.0.0.1:12345"),
            true,
            Some("edge-only-secret-0123456789abcdef0123456789"),
            "https",
            "gateway.example.com",
            "https://gateway.example.com",
            "/health",
            StatusCode::OK,
        ),
        (
            None,
            true,
            Some("edge-only-secret-0123456789abcdef0123456789"),
            "https",
            "gateway.example.com",
            "https://gateway.example.com",
            "/health",
            StatusCode::FORBIDDEN,
        ),
        (
            Some("192.0.2.1:12345"),
            true,
            Some("edge-only-secret-0123456789abcdef0123456789"),
            "https",
            "gateway.example.com",
            "https://gateway.example.com",
            "/health",
            StatusCode::FORBIDDEN,
        ),
        (
            Some("127.0.0.1:12345"),
            false,
            Some("edge-only-secret-0123456789abcdef0123456789"),
            "https",
            "gateway.example.com",
            "https://gateway.example.com",
            "/health",
            StatusCode::FORBIDDEN,
        ),
        (
            Some("127.0.0.1:12345"),
            true,
            None,
            "https",
            "gateway.example.com",
            "https://gateway.example.com",
            "/health",
            StatusCode::FORBIDDEN,
        ),
        (
            Some("127.0.0.1:12345"),
            true,
            Some("wrong"),
            "https",
            "gateway.example.com",
            "https://gateway.example.com",
            "/health",
            StatusCode::FORBIDDEN,
        ),
        (
            Some("127.0.0.1:12345"),
            true,
            Some("edge-only-secret-0123456789abcdef0123456789"),
            "http",
            "gateway.example.com",
            "https://gateway.example.com",
            "/health",
            StatusCode::FORBIDDEN,
        ),
        (
            Some("127.0.0.1:12345"),
            true,
            Some("edge-only-secret-0123456789abcdef0123456789"),
            "https",
            "evil.example",
            "https://gateway.example.com",
            "/health",
            StatusCode::FORBIDDEN,
        ),
        (
            Some("127.0.0.1:12345"),
            true,
            Some("edge-only-secret-0123456789abcdef0123456789"),
            "https",
            "gateway.example.com",
            "https://evil.example",
            "/health",
            StatusCode::FORBIDDEN,
        ),
        (
            Some("127.0.0.1:12345"),
            true,
            Some("edge-only-secret-0123456789abcdef0123456789"),
            "https",
            "gateway.example.com",
            "https://gateway.example.com",
            "/login",
            StatusCode::FORBIDDEN,
        ),
    ] {
        let mut request = Request::builder()
            .uri(path)
            .header("host", host)
            .header("origin", origin)
            .header("x-forwarded-proto", tls)
            .header("x-forwarded-for", "127.0.0.1");
        if let Some(secret) = secret {
            request = request.header("x-chat-gateway-proxy-key", secret);
        }
        let response = remote_app(peer.map(|peer| peer.parse().unwrap()), configured)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            expected,
            "peer={peer:?}, configured={configured}, path={path}"
        );
    }
}

#[tokio::test]
async fn strips_edge_credential_before_local_handlers() {
    let app = Router::new()
        .route(
            "/health",
            get(|headers: HeaderMap| async move {
                assert!(!headers.contains_key("x-chat-gateway-proxy-key"));
                StatusCode::OK
            }),
        )
        .layer(middleware::from_fn(protect_login_api));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .header("host", "127.0.0.1:8080")
                .header("x-chat-gateway-proxy-key", "should-never-reach-handler")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn rejects_non_loopback_host_at_http_boundary() {
    let response = request("/login/api/status", &[("host", "attacker.example")]).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response.headers().contains_key("x-request-id"));
}

#[test]
fn server_bind_defaults_to_loopback_and_requires_explicit_docker_opt_in() {
    assert_eq!(
        server_bind_addr(8080, false),
        SocketAddr::from(([127, 0, 0, 1], 8080))
    );
    assert_eq!(
        server_bind_addr(8080, true),
        SocketAddr::from(([0, 0, 0, 0], 8080))
    );
}

#[tokio::test]
async fn rejects_mismatched_origin_at_http_boundary() {
    let response = request(
        "/login/api/status",
        &[
            ("host", "127.0.0.1:8080"),
            ("origin", "https://attacker.example"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], "unsafe_local_request");
}

#[tokio::test]
async fn rejects_cross_site_login_mutation_metadata() {
    let response = request(
        "/login/api/status",
        &[("host", "127.0.0.1:8080"), ("sec-fetch-site", "cross-site")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rejects_duplicate_local_host_and_origin_at_real_http_boundary() {
    let duplicate_host = request(
        "/login/api/status",
        &[
            ("host", "127.0.0.1:8080"),
            ("host", "attacker.example:8080"),
        ],
    )
    .await;
    assert_eq!(duplicate_host.status(), StatusCode::FORBIDDEN);

    let duplicate_origin = request(
        "/login/api/status",
        &[
            ("host", "127.0.0.1:8080"),
            ("origin", "http://127.0.0.1:8080"),
            ("origin", "https://attacker.example"),
        ],
    )
    .await;
    assert_eq!(duplicate_origin.status(), StatusCode::FORBIDDEN);

    let same_host_twice = request(
        "/health",
        &[("host", "localhost:8080"), ("host", "localhost:8080")],
    )
    .await;
    assert_eq!(same_host_twice.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn allows_matching_loopback_origin_and_originless_local_clients() {
    let matching = request(
        "/login/api/status",
        &[
            ("host", "127.0.0.1:8080"),
            ("origin", "http://127.0.0.1:8080"),
        ],
    )
    .await;
    assert_eq!(matching.status(), StatusCode::OK);

    let originless = request("/login/api/status", &[("host", "localhost:8080")]).await;
    assert_eq!(originless.status(), StatusCode::OK);
}

#[tokio::test]
async fn rejects_missing_host_and_dns_rebinding_even_when_origin_matches() {
    let missing_host = request("/login/api/status", &[]).await;
    assert_eq!(missing_host.status(), StatusCode::FORBIDDEN);

    let rebinding = request(
        "/login/api/status",
        &[
            ("host", "attacker.example:8080"),
            ("origin", "http://attacker.example:8080"),
        ],
    )
    .await;
    assert_eq!(rebinding.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn protects_every_session_control_action_from_cross_origin_requests() {
    for (method, path) in [
        ("POST", "/login/api/openai/start"),
        ("POST", "/login/api/openai/cancel"),
        ("POST", "/login/api/openai/complete"),
        ("GET", "/login/api/openai/view"),
        ("POST", "/login/api/openai/act"),
    ] {
        let response = app()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("host", "127.0.0.1:8080")
                    .header("origin", "https://attacker.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
        assert!(
            response.headers().contains_key("x-request-id"),
            "{method} {path}"
        );
    }
}

#[test]
fn server_listener_is_loopback_only_by_default() {
    assert!(server_bind_addr(8080, false).ip().is_loopback());
    assert_eq!(server_bind_addr(8080, false).port(), 8080);
}
#[tokio::test]
async fn host_protection_also_applies_to_non_login_routes() {
    let response = request("/health", &[("host", "attacker.example")]).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}