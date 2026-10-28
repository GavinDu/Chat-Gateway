//! Repeatable provider latency/RSS probe. Fake mode is safe for offline CI;
//! live mode talks only to an explicitly configured local gateway URL.
use axum::{
    extract::Json,
    response::sse::{Event, Sse},
    routing::post,
    Router,
};
use serde_json::{json, Value};
use std::{
    convert::Infallible,
    env,
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const FAKE_UPSTREAM_MS: u64 = 30;

async fn fake_chat(Json(_request): Json<Value>) -> impl axum::response::IntoResponse {
    let stream = async_stream::stream! {
        yield Ok::<Event, Infallible>(Event::default().data(json!({"choices":[{"delta":{"role":"assistant"},"index":0}]}).to_string()));
        tokio::time::sleep(Duration::from_millis(FAKE_UPSTREAM_MS)).await;
        yield Ok::<Event, Infallible>(Event::default().data(json!({"choices":[{"delta":{"content":"benchmark-token"},"index":0}]}).to_string()));
        yield Ok::<Event, Infallible>(Event::default().data("[DONE]"));
    };
    Sse::new(stream)
}

fn option(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn rss_kib(pid: &str) -> Option<u64> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", pid])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[derive(Debug)]
struct Sample {
    provider: String,
    total_ms: f64,
    ttft_ms: Option<f64>,
    rss_kib: Option<u64>,
}

fn request(
    url: &str,
    provider: &str,
    model: &str,
    api_key: Option<&str>,
    pid: Option<&str>,
) -> anyhow::Result<Sample> {
    let payload = json!({
        "model": model,
        "messages": [{"role":"user","content":"benchmark"}],
        "stream": true
    })
    .to_string();
    let mut command = Command::new("curl");
    command
        .arg("--silent")
        .arg("--show-error")
        .arg("--no-buffer")
        .arg("--request")
        .arg("POST")
        .arg("--header")
        .arg("Content-Type: application/json")
        .arg("--data-binary")
        .arg(&payload);
    if let Some(key) = api_key {
        command
            .arg("--header")
            .arg(format!("Authorization: Bearer {key}"));
    }
    let mut child = command
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| {
            anyhow::anyhow!("could not start curl; install curl to run this benchmark: {error}")
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("curl stdout was not captured"))?;
    let started = Instant::now();
    let mut ttft_ms = None;
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        if ttft_ms.is_none() && line.strip_prefix("data:").is_some_and(|data| has_content_delta(data.trim())) {
            ttft_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
        }
    }
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("curl failed for provider {provider} with status {status}");
    }
    Ok(Sample {
        provider: provider.to_owned(),
        total_ms: started.elapsed().as_secs_f64() * 1000.0,
        ttft_ms,
        rss_kib: pid.and_then(rss_kib),
    })
}

/// Detect any non-empty OpenAI-compatible streamed content delta. Live providers
/// never promise the synthetic token used by the fake upstream.
fn has_content_delta(data: &str) -> bool {
    let Ok(payload) = serde_json::from_str::<Value>(data) else {
        return false;
    };
    payload
        .pointer("/choices/0/delta/content")
        .and_then(Value::as_str)
        .is_some_and(|content| !content.is_empty())
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    Some(values[values.len() / 2])
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let fake = args.iter().any(|arg| arg == "--fake");
    let smoke = args.iter().any(|arg| arg == "--smoke");
    let iterations = option(&args, "--iterations")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(if smoke { 3 } else { 20 })
        .clamp(1, 10_000);
    let providers = option(&args, "--providers")
        .unwrap_or_else(|| "chatgpt,claude".to_owned())
        .split(',')
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if providers.is_empty() {
        anyhow::bail!("--providers must contain at least one provider name");
    }
    let api_key = option(&args, "--api-key");
    let model_name = option(&args, "--model").unwrap_or_else(|| "auto".to_owned());
    if !fake && model_name == "auto" {
        anyhow::bail!(
            "live mode requires --model <configured-model-name> so each provider can be pinned"
        );
    }
    let pid = option(&args, "--pid");
    let url = if fake {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let app = Router::new().route("/v1/chat/completions", post(fake_chat));
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                eprintln!("fake benchmark server failed: {error}");
            }
        });
        format!("http://{address}/v1/chat/completions")
    } else {
        option(&args, "--url")
            .unwrap_or_else(|| "http://127.0.0.1:8080/v1/chat/completions".to_owned())
    };

    println!(
        "mode={} url={} iterations={} providers={}",
        if fake { "fake" } else { "live" },
        url,
        iterations,
        providers.join(",")
    );
    if fake {
        println!(
            "fake_upstream_delay_ms={FAKE_UPSTREAM_MS}; gateway_overhead is TTFT minus this configured delay (synthetic, not a production estimate)"
        );
    } else {
        println!(
            "live measurements include gateway and upstream work; use /metrics to separate queue/provider timing. No prompts are logged."
        );
    }

    let mut samples = Vec::with_capacity(iterations);
    let mut switch_ttft = Vec::new();
    let mut previous: Option<String> = None;
    for index in 0..iterations {
        let provider = &providers[index % providers.len()];
        let model = if fake {
            "auto".to_owned()
        } else {
            format!("{provider}/{model_name}")
        };
        let sample = request(&url, provider, &model, api_key.as_deref(), pid.as_deref())?;
        let gateway_overhead = if fake {
            sample
                .ttft_ms
                .map(|ttft| (ttft - FAKE_UPSTREAM_MS as f64).max(0.0))
        } else {
            None
        };
        println!(
            "sample={} provider={} total_ms={:.2} ttft_ms={} fake_gateway_overhead_ms={} rss_kib={}",
            index + 1,
            sample.provider,
            sample.total_ms,
            sample
                .ttft_ms
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "n/a".to_owned()),
            gateway_overhead
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "n/a".to_owned()),
            sample
                .rss_kib
                .map(|v| v.to_string())
                .unwrap_or_else(|| "n/a".to_owned()),
        );
        if previous.as_deref().is_some_and(|last| last != provider) {
            if let Some(ttft) = sample.ttft_ms {
                switch_ttft.push(ttft);
            }
        }
        previous = Some(provider.clone());
        samples.push(sample);
    }

    let mut totals = samples
        .iter()
        .map(|sample| sample.total_ms)
        .collect::<Vec<_>>();
    let mut ttfts = samples
        .iter()
        .filter_map(|sample| sample.ttft_ms)
        .collect::<Vec<_>>();
    let rss = samples
        .iter()
        .filter_map(|sample| sample.rss_kib)
        .collect::<Vec<_>>();
    let mut rss_samples = rss.iter().map(|value| *value as f64).collect::<Vec<_>>();
    println!(
        "summary total_ms_median={:?} ttft_ms_median={:?} provider_switch_ttft_ms_median={:?} rss_kib_median={:?}",
        median(&mut totals),
        median(&mut ttfts),
        median(&mut switch_ttft),
        median(&mut rss_samples)
    );
    if fake && !samples.iter().any(|sample| sample.ttft_ms.is_some()) {
        anyhow::bail!("fake benchmark did not observe a streamed content token");
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::has_content_delta;

    #[test]
    fn ttft_detection_accepts_arbitrary_live_provider_content() {
        assert!(has_content_delta(r#"{"choices":[{"delta":{"content":"hello"}}]}"#));
        assert!(has_content_delta(r#"{"choices":[{"delta":{"content":"benchmark-token"}}]}"#));
    }

    #[test]
    fn ttft_detection_ignores_role_only_and_empty_chunks() {
        assert!(!has_content_delta(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#));
        assert!(!has_content_delta(r#"{"choices":[{"delta":{"content":""}}]}"#));
        assert!(!has_content_delta("[DONE]"));
    }
}
