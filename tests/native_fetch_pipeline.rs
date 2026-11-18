use obscura_browser::{BrowserContext, Page};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, Notify},
};

async fn settle_until(page: &mut Page, expression: &str) -> bool {
    for _ in 0..100 {
        page.settle(20).await;
        if page.evaluate(expression).as_bool() == Some(true) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test(flavor = "current_thread")]
async fn native_fetch_metadata_incremental_stream_clone_cancel_and_limit() -> anyhow::Result<()> {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let gate = Arc::new(Notify::new());
    let observed = Arc::new(Mutex::new(None::<HashMap<String, String>>));
    let cancelled = Arc::new(AtomicBool::new(false));
    let server_gate = gate.clone();
    let captured = observed.clone();
    let closed = cancelled.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let gate = server_gate.clone();
            let captured = captured.clone();
            let closed = closed.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                while request.len() < 32768 {
                    let Ok(n) = socket.read(&mut buffer).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..n]);
                    if request.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&request);
                let path = text
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                if path == "/echo" {
                    *captured.lock().await = Some(
                        text.lines()
                            .skip(1)
                            .take_while(|line| !line.is_empty())
                            .filter_map(|line| line.split_once(':'))
                            .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
                            .collect(),
                    );
                }
                if matches!(path, "/events" | "/cancel" | "/oversized") {
                    let first = "data: first\n\n";
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{first}\r\n",first.len());
                    if socket.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                    if path == "/cancel" {
                        let mut byte = [0u8; 1];
                        if matches!(
                            tokio::time::timeout(Duration::from_secs(3), socket.read(&mut byte))
                                .await,
                            Ok(Ok(0))
                        ) {
                            closed.store(true, Ordering::SeqCst);
                        }
                    } else if path == "/events" {
                        gate.notified().await;
                        let _ = socket.write_all(b"0\r\n\r\n").await;
                    } else {
                        let _ = socket.write_all(b"0\r\n\r\n").await;
                    }
                    return;
                }
                let (kind, body) = if path == "/echo" {
                    ("application/json", r#"{"ok":true}"#)
                } else {
                    ("text/html", "<html><body>fixture</body></html>")
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nSet-Cookie: fixture=one; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    let context = Arc::new(BrowserContext::with_storage_and_network(
        "synthetic-pipeline".into(),
        None,
        true,
        None,
        None,
        true,
    ));
    let mut page = Page::new("fixture".into(), context);
    page.navigate(&format!("{origin}/fixture")).await?;
    assert_eq!(page.evaluate("typeof Deno==='undefined' && typeof __obscuraCore==='undefined' && typeof _createNativeStreamBody==='undefined'").as_bool(),Some(true));
    assert_eq!(
        page.evaluate("navigator.userAgent.includes('Chrome/145.')")
            .as_bool(),
        Some(true)
    );
    page.evaluate(r#"(() => {
        globalThis.jsonOK=false;
        fetch('/echo',{method:'POST',headers:{'content-type':'application/json'},credentials:'include',body:'{}'})
          .then(r=>r.json()).then(body=>globalThis.jsonOK=body.ok===true);
        return true;
    })()"#);
    assert!(settle_until(&mut page, "globalThis.jsonOK").await);
    let headers = observed.lock().await.take().expect("request captured");
    assert_eq!(
        headers.get("sec-fetch-site").map(String::as_str),
        Some("same-origin")
    );
    assert_eq!(
        headers.get("sec-fetch-mode").map(String::as_str),
        Some("cors")
    );
    assert_eq!(headers.get("sec-fetch-dest").map(String::as_str), Some(""));
    assert_eq!(headers.get("referer"), Some(&format!("{origin}/")));
    assert_eq!(headers.get("origin"), Some(&origin));
    assert!(headers
        .get("cookie")
        .is_some_and(|value| value.contains("fixture=one")));
    assert_eq!(headers.get("accept").map(String::as_str), Some("*/*"));

    page.evaluate(r#"(() => {
        globalThis.omitOK=false;
        fetch('/echo',{method:'POST',credentials:'omit',body:'{}'}).then(r=>r.json()).then(()=>globalThis.omitOK=true);
        return true;
    })()"#);
    assert!(settle_until(&mut page, "globalThis.omitOK").await);
    let omitted = observed.lock().await.take().expect("omit request captured");
    assert!(
        !omitted.contains_key("cookie"),
        "credentials omit must remain cookie-free"
    );
    let cross = origin.replace("127.0.0.1", "localhost");
    page.evaluate(&format!(
        r#"(() => {{
        globalThis.corsBlocked=false;
        fetch({}).then(()=>globalThis.corsBlocked=false).catch(()=>globalThis.corsBlocked=true);
        return true;
    }})()"#,
        serde_json::to_string(&format!("{cross}/echo"))?
    ));
    assert!(
        settle_until(&mut page, "globalThis.corsBlocked").await,
        "cross-origin body without CORS permission must be blocked"
    );

    page.evaluate(r#"(() => {
        globalThis.firstBeforeEOF=false;globalThis.cloneOK=false;
        fetch('/events').then(async response=>{
            response.clone().text().then(text=>globalThis.cloneOK=text==='data: first\n\n');
            const part=await response.body.getReader().read();
            globalThis.firstBeforeEOF=!part.done && new TextDecoder().decode(part.value)==='data: first\n\n';
        });
        return true;
    })()"#);
    let before_eof = settle_until(&mut page, "globalThis.firstBeforeEOF").await;
    gate.notify_one();
    assert!(before_eof, "first event must arrive before server EOF");
    assert!(settle_until(&mut page, "globalThis.cloneOK").await);

    page.evaluate(
        r#"(() => {
        globalThis.cancelOK=false;
        fetch('/cancel').then(async response=>{
            const reader=response.body.getReader();
            const part=await reader.read();
            if(part.done)throw new Error('missing first chunk');
            await reader.cancel();globalThis.cancelOK=true;
        });
        return true;
    })()"#,
    );
    assert!(settle_until(&mut page, "globalThis.cancelOK").await);
    for _ in 0..100 {
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
        page.settle(20).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        cancelled.load(Ordering::SeqCst),
        "cancel must close native response"
    );

    std::env::set_var("OBSCURA_FETCH_MAX_BODY_BYTES", "8");
    page.evaluate(
        r#"(() => {
        globalThis.limitRejected=false;globalThis.oversizedExposed=false;
        fetch('/oversized').then(async response=>{
            const part=await response.body.getReader().read();
            globalThis.oversizedExposed=!part.done;
        }).catch(()=>globalThis.limitRejected=true);
        return true;
    })()"#,
    );
    let rejected = settle_until(&mut page, "globalThis.limitRejected").await;
    std::env::remove_var("OBSCURA_FETCH_MAX_BODY_BYTES");
    std::env::remove_var("OBSCURA_ALLOW_PRIVATE_NETWORK");
    assert!(rejected, "oversized chunk must reject the reader");
    assert_eq!(
        page.evaluate("globalThis.oversizedExposed").as_bool(),
        Some(false)
    );
    server.abort();
    Ok(())
}
