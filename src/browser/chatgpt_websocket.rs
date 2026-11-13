//! Private, page-scoped transport for ChatGPT's WebSocket client.
//!
//! The pinned Obscura WebSocket opens synthetically and discards send(). This
//! bridge fulfills private same-origin fetches in-process; it opens no listener,
//! writes no credentials, and forwards only validated first-party WSS requests.
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{SinkExt, StreamExt};
use obscura_core::{Browser, InterceptResolution, Page};
use serde_json::{json, Value};
use std::{collections::HashMap, net::IpAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, Mutex},
};
use tokio_tungstenite::{
    tungstenite::{
        client::IntoClientRequest,
        protocol::{CloseFrame, WebSocketConfig},
        Message,
    },
    WebSocketStream,
};

const BRIDGE_PATH: &str = "/.well-known/chat-gateway-websocket";
const MAX_COMMAND: usize = 1_048_576;
const MAX_SOCKETS: usize = 16;

struct SocketHandle {
    outgoing: mpsc::Sender<Message>,
    incoming: Arc<Mutex<mpsc::Receiver<Value>>>,
}
#[derive(Default)]
struct State {
    sockets: Mutex<HashMap<u64, SocketHandle>>,
    next_id: std::sync::atomic::AtomicU64,
    generation: std::sync::atomic::AtomicU64,
}

fn allowed_target(target: &str) -> Option<(String, String)> {
    let uri = target.parse::<axum::http::Uri>().ok()?;
    let authority = uri.authority()?;
    let host = uri.host()?.to_ascii_lowercase();
    if uri.scheme_str() != Some("wss")
        || authority.as_str().contains('@')
        || authority.port_u16().is_some_and(|p| p != 443)
        || !(host == "chatgpt.com" || host.ends_with(".chatgpt.com"))
        || target.contains('#')
    {
        return None;
    }
    // CookieStore applies its normal secure/path/domain/HttpOnly policy.
    let cookie_url = format!("https://{}{}", authority, uri.path());
    Some((host, cookie_url))
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            !v.is_loopback()
                && !v.is_private()
                && !v.is_link_local()
                && !v.is_unspecified()
                && !v.is_multicast()
                && !v.is_broadcast()
                && !v.is_documentation()
                && o[0] != 0
                && !(o[0] == 100 && (64..=127).contains(&o[1]))
                && !(o[0] == 198 && (o[1] == 18 || o[1] == 19))
                && o[0] < 240
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            !v.is_loopback()
                && !v.is_unspecified()
                && !v.is_multicast()
                && !v.is_unique_local()
                && !v.is_unicast_link_local()
                && !(v.segments()[0] == 0x2001 && v.segments()[1] == 0x0db8)
        }
    }
}

fn bridge_origin(url: &str) -> Option<&'static str> {
    if url == format!("https://chatgpt.com{BRIDGE_PATH}") {
        Some("https://chatgpt.com")
    } else if url == format!("https://www.chatgpt.com{BRIDGE_PATH}") {
        Some("https://www.chatgpt.com")
    } else {
        None
    }
}

async fn register_socket<S>(
    state: &Arc<State>,
    socket: WebSocketStream<S>,
    generation: u64,
) -> Option<u64>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut sockets = state.sockets.lock().await;
    if sockets.len() >= MAX_SOCKETS
        || state.generation.load(std::sync::atomic::Ordering::SeqCst) != generation
    {
        return None;
    }
    let id = state
        .next_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1;
    let (outgoing, mut outgoing_rx) = mpsc::channel::<Message>(16);
    let (incoming_tx, incoming_rx) = mpsc::channel::<Value>(8);
    sockets.insert(
        id,
        SocketHandle {
            outgoing,
            incoming: Arc::new(Mutex::new(incoming_rx)),
        },
    );
    drop(sockets);
    tokio::spawn(async move {
        let (mut write, mut read) = socket.split();
        loop {
            tokio::select! {
                command = outgoing_rx.recv() => {
                    let Some(command) = command else { break };
                    tracing::debug!(provider="chatgpt", frame_bytes=command.len(), "ChatGPT WebSocket sending frame");
                    if !matches!(tokio::time::timeout(Duration::from_secs(10), write.send(command)).await, Ok(Ok(()))) {
                        let _ = incoming_tx.try_send(json!({"type":"close","code":1006,"reason":"","clean":false}));
                        break;
                    }
                }
                frame = read.next() => {
                    let packet = match frame {
                        Some(Ok(Message::Text(text))) => {
                            let json_error=serde_json::from_str::<Value>(&text).ok().is_some_and(|v|v.get("error").is_some() || v["type"]=="error");
                            tracing::debug!(provider="chatgpt", frame_bytes=text.len(), json_error, "ChatGPT WebSocket received text");
                            json!({"type":"text","data":text})
                        },
                        Some(Ok(Message::Binary(bytes))) => {
                            tracing::debug!(provider="chatgpt", frame_bytes=bytes.len(), "ChatGPT WebSocket received binary");
                            json!({"type":"binary","data":STANDARD.encode(bytes)})
                        },
                        Some(Ok(Message::Close(frame))) => {
                            let (code, reason) = frame.map(|f|(u16::from(f.code), f.reason.into_owned())).unwrap_or((1000,String::new()));
                            let _ = incoming_tx.try_send(json!({"type":"close","code":code,"reason":reason,"clean":true}));
                            break;
                        }
                        Some(Ok(Message::Ping(bytes))) => {
                            if write.send(Message::Pong(bytes)).await.is_err() { break; }
                            continue;
                        }
                        Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => continue,
                        _ => {
                            let _ = incoming_tx.try_send(json!({"type":"close","code":1006,"reason":"","clean":false}));
                            break;
                        }
                    };
                    // Bound both queues; never buffer unlimited provider data.
                    if !matches!(tokio::time::timeout(Duration::from_secs(15), incoming_tx.send(packet)).await, Ok(Ok(()))) {
                        tracing::warn!(provider="chatgpt", category="websocket_receive_stalled", "ChatGPT transport receiver stalled");
                        let _ = write.close().await;
                        break;
                    }
                }
            }
        }
    });
    Some(id)
}

async fn command(
    state: &Arc<State>,
    cookies: &obscura_core::CookieStore,
    origin: &str,
    input: Value,
) -> Value {
    let fail = || json!({"ok":false});
    match input["op"].as_str().unwrap_or("") {
        "reset" => {
            let mut sockets = state.sockets.lock().await;
            state
                .generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            sockets.clear();
            json!({"ok":true})
        }
        "open" => {
            let Some(target) = input["url"].as_str() else {
                return fail();
            };
            let Some((host, cookie_url)) = allowed_target(target) else {
                return fail();
            };
            let Some(ua) = input["ua"].as_str().filter(|s| s.len() <= 1024) else {
                return fail();
            };
            if state.sockets.lock().await.len() >= MAX_SOCKETS {
                return fail();
            }
            let generation = state.generation.load(std::sync::atomic::Ordering::SeqCst);
            let attempt = async {
                let addresses: Vec<_> = tokio::net::lookup_host((host.as_str(), 443))
                    .await
                    .ok()?
                    .collect();
                if addresses.is_empty() || addresses.iter().any(|a| !public_ip(a.ip())) {
                    return None;
                }
                // Race a bounded set of addresses so an unreachable IPv6
                // route cannot consume the SPA's socket-open deadline before
                // a working IPv4 route is tried (or vice versa).
                let mut addresses = addresses;
                addresses.sort_by_key(|address| address.is_ipv6());
                let mut attempts: futures_util::stream::FuturesUnordered<_> = addresses
                    .into_iter()
                    .take(16)
                    .enumerate()
                    .map(|(index, address)| async move {
                        tokio::time::sleep(Duration::from_millis(150 * index as u64)).await;
                        tokio::time::timeout(
                            Duration::from_secs(3),
                            tokio::net::TcpStream::connect(address),
                        )
                        .await
                        .ok()
                        .and_then(Result::ok)
                    })
                    .collect();
                let mut tcp = None;
                while let Some(result) = attempts.next().await {
                    if result.is_some() {
                        tcp = result;
                        break;
                    }
                }
                drop(attempts);
                let tcp = tcp?;
                let mut request = target.into_client_request().ok()?;
                let headers = request.headers_mut();
                headers.insert("Origin", origin.parse().ok()?);
                headers.insert("User-Agent", ua.parse().ok()?);
                let values = cookies.get_for_url(&cookie_url).ok()?;
                let cookie = values
                    .iter()
                    .map(|c| format!("{}={}", c.name, c.value))
                    .collect::<Vec<_>>()
                    .join("; ");
                if !cookie.is_empty() {
                    headers.insert("Cookie", cookie.parse().ok()?);
                }
                let protocols: Vec<&str> = input["protocols"]
                    .as_array()?
                    .iter()
                    .map(Value::as_str)
                    .collect::<Option<_>>()?;
                if protocols.len() > 16 || protocols.iter().any(|s| s.len() > 256) {
                    return None;
                }
                if !protocols.is_empty() {
                    headers.insert("Sec-WebSocket-Protocol", protocols.join(", ").parse().ok()?);
                }
                let config = WebSocketConfig {
                    max_message_size: Some(4 * MAX_COMMAND),
                    max_frame_size: Some(4 * MAX_COMMAND),
                    ..Default::default()
                };
                let (socket, response) = match tokio_tungstenite::client_async_tls_with_config(
                    request,
                    tcp,
                    Some(config),
                    None,
                )
                .await
                {
                    Ok(result) => result,
                    Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                        tracing::warn!(
                            provider = "chatgpt",
                            status = response.status().as_u16(),
                            "ChatGPT WebSocket handshake rejected"
                        );
                        return None;
                    }
                    Err(_) => return None,
                };
                tracing::info!(
                    provider = "chatgpt",
                    host = host.as_str(),
                    "ChatGPT WebSocket connected"
                );
                let protocol = response
                    .headers()
                    .get("Sec-WebSocket-Protocol")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned();
                let id = register_socket(state, socket, generation).await?;
                Some(json!({"ok":true,"id":id,"protocol":protocol}))
            };
            match tokio::time::timeout(Duration::from_secs(15), attempt).await {
                Ok(Some(value)) => value,
                _ => {
                    tracing::warn!(
                        provider = "chatgpt",
                        category = "websocket_connect_failed",
                        "ChatGPT transport connection failed"
                    );
                    fail()
                }
            }
        }
        "read" => {
            let Some(id) = input["id"].as_u64() else {
                return fail();
            };
            let reader = state
                .sockets
                .lock()
                .await
                .get(&id)
                .map(|s| s.incoming.clone());
            let Some(reader) = reader else { return fail() };
            let mut reader = reader.lock().await;
            let packet = match tokio::time::timeout(Duration::from_secs(5), reader.recv()).await {
                Ok(Some(packet)) => packet,
                Ok(None) => json!({"type":"close","code":1006,"reason":"","clean":false}),
                Err(_) => json!({"type":"idle"}),
            };
            json!({"ok":true,"packet":packet})
        }
        "send" => {
            let Some(id) = input["id"].as_u64() else {
                return fail();
            };
            let Some(data) = input["data"].as_str().filter(|v| v.len() <= MAX_COMMAND) else {
                return fail();
            };
            let frame = if input["binary"].as_bool() == Some(true) {
                let Ok(bytes) = STANDARD.decode(data) else {
                    return fail();
                };
                Message::Binary(bytes)
            } else {
                Message::Text(data.to_owned())
            };
            let sender = state
                .sockets
                .lock()
                .await
                .get(&id)
                .map(|s| s.outgoing.clone());
            json!({"ok":sender.is_some_and(|s|s.try_send(frame).is_ok())})
        }
        "close" => {
            let Some(id) = input["id"].as_u64() else {
                return fail();
            };
            let code = input["code"].as_u64().unwrap_or(1000) as u16;
            if code != 1000 && !(3000..=4999).contains(&code) {
                return fail();
            }
            let reason = input["reason"].as_str().unwrap_or("");
            if reason.len() > 123 {
                return fail();
            }
            if let Some(sender) = state
                .sockets
                .lock()
                .await
                .get(&id)
                .map(|s| s.outgoing.clone())
            {
                let _ = sender.try_send(Message::Close(Some(CloseFrame {
                    code: code.into(),
                    reason: reason.to_owned().into(),
                })));
            }
            json!({"ok":true})
        }
        "release" => {
            if let Some(id) = input["id"].as_u64() {
                state.sockets.lock().await.remove(&id);
            }
            json!({"ok":true})
        }
        _ => fail(),
    }
}

pub(super) fn attach(browser: &Browser, page: &mut Page) {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let config = json!({"token":token,"path":BRIDGE_PATH});
    page.add_preload_script(
        &include_str!("chatgpt_websocket.js").replace("__BRIDGE_CONFIG__", &config.to_string()),
    );
    let mut requests = page.enable_interception();
    let cookies = Arc::new(browser.cookies());
    let state = Arc::new(State::default());
    tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            let Some(origin) = bridge_origin(&request.url) else {
                let _ = request.resolver.send(InterceptResolution::Continue {
                    url: None,
                    method: None,
                    headers: None,
                    body: None,
                });
                continue;
            };
            let supplied = request
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("x-chat-gateway-ws-token"))
                .map(|(_, v)| v);
            let encoded = request
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("x-chat-gateway-ws-command"))
                .map(|(_, v)| v);
            let parsed = supplied
                .filter(|s| *s == &token)
                .and(encoded)
                .filter(|s| s.len() <= MAX_COMMAND * 2)
                .and_then(|s| STANDARD.decode(s).ok())
                .filter(|v| v.len() <= MAX_COMMAND)
                .and_then(|v| serde_json::from_slice::<Value>(&v).ok());
            let state = state.clone();
            let cookies = cookies.clone();
            tokio::spawn(async move {
                let output = if let Some(input) = parsed {
                    command(&state, &cookies, origin, input).await
                } else {
                    json!({"ok":false})
                };
                let opened_id = output.get("id").and_then(Value::as_u64);
                if request
                    .resolver
                    .send(InterceptResolution::Fulfill {
                        status: 200,
                        headers: HashMap::from([
                            ("content-type".into(), "application/json".into()),
                            ("cache-control".into(), "no-store".into()),
                        ]),
                        body: output.to_string(),
                        body_base64: String::new(),
                    })
                    .is_err()
                {
                    // A navigation can cancel an open while its TLS handshake
                    // is finishing. Do not retain a socket nobody can read.
                    if let Some(id) = opened_id {
                        state.sockets.lock().await.remove(&id);
                    }
                }
            });
        }
        let mut sockets = state.sockets.lock().await;
        state
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        sockets.clear();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn targets_reject_other_origins_credentials_ports_and_plaintext() {
        for url in [
            "wss://ws.chatgpt.com/subscribe?token=fixture",
            "wss://chatgpt.com/socket",
        ] {
            assert!(allowed_target(url).is_some());
        }
        for url in [
            "ws://ws.chatgpt.com/socket",
            "wss://chatgpt.com.attacker.invalid/socket",
            "wss://user@ws.chatgpt.com/socket",
            "wss://127.0.0.1/socket",
            "wss://ws.chatgpt.com:444/socket",
            "wss://ws.chatgpt.com/socket#fragment",
        ] {
            assert!(allowed_target(url).is_none(), "{url}");
        }
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "fc00::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()));
        }
        assert!(public_ip("1.1.1.1".parse().unwrap()));
    }

    #[tokio::test]
    async fn native_chatgpt_lazy_stream_pulls_serially_after_start() {
        let browser = Browser::builder().stealth(false).build().unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<body>stream fixture</body>")
            .await
            .unwrap();
        let source = include_str!("chatgpt_streams.js").replace(
            "if (!['chatgpt.com', 'www.chatgpt.com'].includes(location.hostname)) return;",
            "",
        );
        page.evaluate(&source);
        page.evaluate(r#"(() => {globalThis.fixtureResult=null;(async()=>{
            let started=false,pulls=0,active=0,maxActive=0;
            const stream=new ReadableStream({
                async start(){await Promise.resolve();started=true;},
                async pull(controller){if(!started)throw Error('early pull');active++;maxActive=Math.max(maxActive,active);await Promise.resolve();controller.enqueue(++pulls);active--;if(pulls===3)controller.close();}
            });
            const reader=stream.getReader();const results=await Promise.all([reader.read(),reader.read(),reader.read(),reader.read()]);
            fixtureResult=JSON.stringify({values:results.map(r=>r.value??null),done:results[3].done,pulls,maxActive});
        })().catch(()=>fixtureResult='error');})()"#);
        page.settle(200).await;
        let result: Value =
            serde_json::from_str(page.evaluate("fixtureResult").as_str().unwrap_or("null"))
                .unwrap();
        assert_eq!(
            result,
            json!({"values":[1,2,3,null],"done":true,"pulls":3,"maxActive":1}),
            "{}",page.evaluate("JSON.stringify({stream:typeof globalThis.ReadableStream,response:typeof globalThis.Response,result:globalThis.fixtureResult})")
        );
    }

    #[tokio::test]
    async fn native_browser_dispatches_each_received_message_once() {
        let browser = Browser::builder().stealth(false).build().unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body>transport fixture</body></html>")
            .await
            .unwrap();
        page.evaluate(r#"(() => {globalThis.fixturePackets=[];globalThis.fixtureTake=null;
            const push = packet => {if(fixtureTake){const take=fixtureTake;fixtureTake=null;take(packet);}else fixturePackets.push(packet);};
            globalThis.fetch=async (_url,options)=>{
                const cmd=JSON.parse(atob(options.headers['X-Chat-Gateway-WS-Command']));
                let result={ok:true};
                if(cmd.op==='open')result={ok:true,id:1,protocol:''};
                if(cmd.op==='send')push({type:'text',data:cmd.data});
                if(cmd.op==='close')push({type:'close',code:1000,reason:'',clean:true});
                if(cmd.op==='read')result={ok:true,packet:fixturePackets.length?fixturePackets.shift():await new Promise(resolve=>fixtureTake=resolve)};
                return {ok:true,json:async()=>result};
            };})()"#);
        let source = include_str!("chatgpt_websocket.js")
            .replace(
                "if (!['chatgpt.com', 'www.chatgpt.com'].includes(location.hostname)) return;",
                "",
            )
            .replace(
                "__BRIDGE_CONFIG__",
                r#"{"path":"/fixture","token":"fixture"}"#,
            );
        page.evaluate(&source);
        page.evaluate(
            r#"(() => {globalThis.fixtureMessages=0;globalThis.fixtureClosed=false;
            const socket=new WebSocket('wss://ws.chatgpt.com/fixture');
            socket.onopen=()=>socket.send('fixture');
            socket.onmessage=e=>{fixtureMessages++;globalThis.fixtureData=e.data;socket.close();};
            socket.onclose=()=>fixtureClosed=true;})()"#,
        );
        page.settle(300).await;
        assert_eq!(page.evaluate("fixtureMessages").as_f64(), Some(1.0));
        assert_eq!(page.evaluate("fixtureData").as_str(), Some("fixture"));
        assert_eq!(page.evaluate("fixtureClosed").as_bool(), Some(true));
    }

    #[tokio::test]
    async fn burst_frames_are_backpressured_without_being_dropped() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            for index in 0..64 {
                socket
                    .send(Message::Text(format!("frame-{index}")))
                    .await
                    .unwrap();
            }
            while let Some(Ok(_)) = socket.next().await {}
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let state = Arc::new(State::default());
        let id = register_socket(&state, socket, 0).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let browser = Browser::builder().stealth(false).build().unwrap();
        let cookies = browser.cookies();
        for index in 0..64 {
            let reply = command(
                &state,
                &cookies,
                "https://chatgpt.com",
                json!({"op":"read","id":id}),
            )
            .await;
            assert_eq!(reply["packet"]["data"], format!("frame-{index}"));
        }
        command(
            &state,
            &cookies,
            "https://chatgpt.com",
            json!({"op":"reset"}),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn real_socket_forwards_text_binary_and_releases_on_reset() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
                socket.send(message).await.unwrap();
            }
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let state = Arc::new(State::default());
        let id = register_socket(&state, socket, 0).await.unwrap();
        let browser = Browser::builder().stealth(false).build().unwrap();
        let cookies = browser.cookies();
        for (binary, data) in [(false, "fixture"), (true, "AAEC/w==")] {
            assert_eq!(
                command(
                    &state,
                    &cookies,
                    "https://chatgpt.com",
                    json!({"op":"send","id":id,"binary":binary,"data":data})
                )
                .await["ok"],
                true
            );
            let reply = command(
                &state,
                &cookies,
                "https://chatgpt.com",
                json!({"op":"read","id":id}),
            )
            .await;
            assert_eq!(reply["packet"]["data"], data);
        }
        command(
            &state,
            &cookies,
            "https://chatgpt.com",
            json!({"op":"reset"}),
        )
        .await;
        assert!(state.sockets.lock().await.is_empty());
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}
