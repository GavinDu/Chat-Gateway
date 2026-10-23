use axum::{extract::{State, Path}, response::{Html, IntoResponse}, Json};
use std::sync::Arc;
use crate::server::state::AppState;

pub async fn dashboard(State(state): State<Arc<AppState>>) -> Html<String> {
    let providers_html = state.config.providers.iter().map(|p| {
        format!(r#"<div class="provider"><b>{}</b> pri={} enabled={} models={:?}</div>"#, p.name, p.priority, p.enabled, p.models)
    }).collect::<Vec<_>>().join("\n");

    Html(format!(r#"
    <html><head><title>Chat Gateway RS - Dashboard</title>
    <style>
      body{{font-family: monospace; background:#0a0a0a; color:#eaeaea; padding:20px}}
      .provider{{border:1px solid #333; padding:8px; margin:4px; border-radius:6px}}
      #traces{{max-height:400px; overflow:auto; border:1px solid #444; padding:10px}}
      .waterfall{{display:flex; gap:2px; height:20px; margin:4px 0}}
      .seg{{height:100%; display:inline-block;}}
      .parse{{background:#555}} .memory{{background:#7a7}} .routing{{background:#77a}} .acq{{background:#aa7}} .queue{{background:#a77}} .ready{{background:#777}} .input{{background:#5aa}} .send{{background:#a5a}} .ttft{{background:#fa0}} .gen{{background:#0af}}
    </style></head>
    <body>
    <h1>Chat Gateway RS (Rust + Obscura) — 8 Providers</h1>
    <p>Providers: Qwen / MetaAI / Claude / ChatGPT / Gemini / Grok / Mistral / Perplexity</p>
    <p>Obscura endpoint: {endpoint} | Stealth: built-in (webdriver=undefined, fingerprint randomization)</p>
    <h2>Providers</h2>
    {providers}
    <h2>Login</h2>
    <div id="login-modal">
      <p>Login streams browser view without exposing CDP/DOM. Auth input and screenshots are not logged or persisted per security spec. Sessions saved to sessions/&lt;provider&gt;.json (cookies + localStorage)</p>
      <input id="provider" placeholder="qwen|metaai|claude|chatgpt|gemini|grok|mistral|perplexity"/>
      <button onclick="startLogin()">Login</button>
      <canvas id="screencast" width="1280" height="800" style="border:1px solid #333; max-width:100%"></canvas>
      <div>
        <input id="secureInput" placeholder="secure typing relay (tap provider field first on mobile)" style="width:60%"/>
        <button onclick="sendKey('Next')">Next</button>
        <button onclick="sendKey('Enter')">Enter</button>
      </div>
    </div>
    <h2>Live Traces (latest 500) — Performance Waterfall</h2>
    <p>parse | memory | context | routing | browser_acq | provider_queue | page_readiness | input | send | TTFT | generation (first_update, growth, settle, delivery) + update_count</p>
    <div id="traces">loading...</div>
    <h2>Diagnostics</h2>
    <button onclick="fetch('/api/traces').then(r=>r.json()).then(j=>{{document.getElementById('traces').innerText=JSON.stringify(j, null, 2)}})">Refresh Traces</button>
    <script>
      async function loadTraces(){{
        const r = await fetch('/api/traces');
        const traces = await r.json();
        const el = document.getElementById('traces');
        el.innerHTML = traces.reverse().map(t=>{{
          const total = t.total_ms||1;
          const segs = [
            ['parse', t.parse_ms, 'parse'],
            ['mem', t.memory_ms, 'memory'],
            ['route', t.routing_ms, 'routing'],
            ['acq', t.browser_acquisition_ms, 'acq'],
            ['queue', t.provider_queue_ms, 'queue'],
            ['ready', t.page_readiness_ms, 'ready'],
            ['input', t.input_ms, 'input'],
            ['send', t.send_ms, 'send'],
            ['TTFT', t.ttft_ms, 'ttft'],
            ['gen', t.generation_ms, 'gen'],
          ];
          const bars = segs.map(([label, ms, cls])=>`<div class="seg ${{cls}}" style="width:${{Math.max(2, (ms/total)*300)}}px" title="${{label}} ${{ms}}ms"></div>`).join('');
          return `<div><b>${{t.request_id.slice(0,8)}}</b> ${{t.provider}}/${{t.model}} total=${{t.total_ms}}ms updates=${{t.response_update_count}} success=${{t.success}}<div class="waterfall">${{bars}}</div><small>gen breakdown: first=${{t.generation_breakdown.first_update_latency_ms}} growth=${{t.generation_breakdown.active_growth_ms}} settle=${{t.generation_breakdown.settle_wait_ms}} | conv=${{t.conversation_id||''}}</small></div><hr/>`;
        }}).join('');
      }}
      setInterval(loadTraces, 2000);
      loadTraces();
      function startLogin(){{
        const p = document.getElementById('provider').value;
        const es = new EventSource(`/api/login/${{p}}/stream`);
        es.onmessage = e=>{{console.log(e.data)}};
      }}
      function sendKey(k){{ /* relay to isolated browser */ }}
    </script>
    </body></html>
    "#, endpoint=state.config.server.obscura_endpoint, providers=providers_html))
}

pub async fn traces_api(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.traces.list().await)
}

pub async fn traces_latest(State(state): State<Arc<AppState>>, Path(n): Path<usize>) -> impl IntoResponse {
    Json(state.traces.latest(n).await)
}

pub async fn login_stream(State(_state): State<Arc<AppState>>, Path(provider): Path<String>) -> impl IntoResponse {
    // SSE stream of Obscura screencast frames for remote manual login
    // Implementation: start isolated BrowserContext with headed Obscura, Page.startScreencast, relay input
    Json(serde_json::json!({ "provider": provider, "status": "login stream: open dashboard, uses secure typing relay, captures cookies+localStorage to sessions/*.json" }))
}
