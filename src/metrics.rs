use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

#[derive(Default)]
struct MetricData {
    requests: BTreeMap<(String, u16), u64>,
    handler_ms: BTreeMap<String, Vec<u64>>,
    queue_ms: BTreeMap<String, Vec<u64>>,
    ttft_ms: BTreeMap<String, Vec<u64>>,
    provider_work_ms: BTreeMap<String, Vec<u64>>,
    chat_total_ms: BTreeMap<String, Vec<u64>>,
    gateway_overhead_ms: BTreeMap<String, Vec<u64>>,
    provider_outcomes: BTreeMap<(String, String), u64>,
}

static DATA: OnceLock<Mutex<MetricData>> = OnceLock::new();

fn data() -> &'static Mutex<MetricData> {
    DATA.get_or_init(|| Mutex::new(MetricData::default()))
}

pub fn record_http_request(route: &str, status: u16, duration_ms: u64) {
    if let Ok(mut data) = data().lock() {
        *data.requests.entry((route.to_owned(), status)).or_default() += 1;
        data.handler_ms
            .entry(route.to_owned())
            .or_default()
            .push(duration_ms);
    }
}

pub fn record_provider_timings(
    provider: &str,
    outcome: &str,
    queue_ms: u64,
    ttft_ms: Option<u64>,
    provider_work_ms: u64,
    total_ms: u64,
) {
    if let Ok(mut data) = data().lock() {
        data.queue_ms
            .entry(provider.to_owned())
            .or_default()
            .push(queue_ms);
        if let Some(ttft) = ttft_ms {
            data.ttft_ms
                .entry(provider.to_owned())
                .or_default()
                .push(ttft);
        }
        data.provider_work_ms
            .entry(provider.to_owned())
            .or_default()
            .push(provider_work_ms);
        data.chat_total_ms
            .entry(provider.to_owned())
            .or_default()
            .push(total_ms);
        data.gateway_overhead_ms
            .entry(provider.to_owned())
            .or_default()
            .push(total_ms.saturating_sub(queue_ms.saturating_add(provider_work_ms)));
        *data
            .provider_outcomes
            .entry((provider.to_owned(), outcome.to_owned()))
            .or_default() += 1;
    }
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn emit_histogram(
    out: &mut String,
    metric: &str,
    label: &str,
    observations: &BTreeMap<String, Vec<u64>>,
) {
    const BOUNDS: &[u64] = &[5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000];
    use std::fmt::Write as _;
    let _ = writeln!(out, "# TYPE {metric} histogram");
    for (key, values) in observations {
        let escaped = escape_label(key);
        for bound in BOUNDS {
            let count = values.iter().filter(|value| **value <= *bound).count();
            let _ = writeln!(
                out,
                "{metric}_bucket{{{label}=\"{escaped}\",le=\"{bound}\"}} {count}"
            );
        }
        let _ = writeln!(
            out,
            "{metric}_bucket{{{label}=\"{escaped}\",le=\"+Inf\"}} {}",
            values.len()
        );
        let sum: u128 = values.iter().map(|value| *value as u128).sum();
        let _ = writeln!(out, "{metric}_sum{{{label}=\"{escaped}\"}} {sum}");
        let _ = writeln!(
            out,
            "{metric}_count{{{label}=\"{escaped}\"}} {}",
            values.len()
        );
    }
}

pub fn render() -> String {
    use std::fmt::Write as _;
    let Ok(data) = data().lock() else {
        return String::new();
    };
    let mut out = String::new();
    let _ = writeln!(out, "# TYPE gateway_http_requests_total counter");
    for ((route, status), count) in &data.requests {
        let _ = writeln!(
            out,
            "gateway_http_requests_total{{route=\"{}\",status=\"{}\"}} {count}",
            escape_label(route),
            status
        );
    }
    emit_histogram(
        &mut out,
        "gateway_http_handler_duration_ms",
        "route",
        &data.handler_ms,
    );
    emit_histogram(
        &mut out,
        "gateway_provider_queue_duration_ms",
        "provider",
        &data.queue_ms,
    );
    emit_histogram(
        &mut out,
        "gateway_provider_ttft_ms",
        "provider",
        &data.ttft_ms,
    );
    emit_histogram(
        &mut out,
        "gateway_provider_work_duration_ms",
        "provider",
        &data.provider_work_ms,
    );
    emit_histogram(
        &mut out,
        "gateway_chat_total_duration_ms",
        "provider",
        &data.chat_total_ms,
    );
    emit_histogram(
        &mut out,
        "gateway_overhead_ms",
        "provider",
        &data.gateway_overhead_ms,
    );
    let _ = writeln!(out, "# TYPE gateway_provider_requests_total counter");
    for ((provider, outcome), count) in &data.provider_outcomes {
        let _ = writeln!(
            out,
            "gateway_provider_requests_total{{provider=\"{}\",outcome=\"{}\"}} {count}",
            escape_label(provider),
            escape_label(outcome)
        );
    }
    out
}

pub fn normalize_route(path: &str) -> String {
    if path.starts_with("/v1/conversations/") {
        "/v1/conversations/:id".to_owned()
    } else if path.starts_with("/login/api/") {
        "/login/api/*".to_owned()
    } else if path.starts_with("/relay/") {
        "/relay/*".to_owned()
    } else {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_route;

    #[test]
    fn dynamic_paths_are_normalized_before_becoming_metric_labels() {
        assert_eq!(
            normalize_route("/v1/conversations/private-id"),
            "/v1/conversations/:id"
        );
        assert_eq!(normalize_route("/login/api/chatgpt/act"), "/login/api/*");
        assert_eq!(normalize_route("/health"), "/health");
    }

    #[test]
    fn renders_request_and_provider_histograms_without_high_cardinality_ids() {
        super::record_http_request("/v1/chat/completions", 200, 42);
        super::record_provider_timings("chatgpt", "success", 5, Some(12), 25, 42);
        let output = super::render();
        assert!(output.contains("gateway_http_requests_total"));
        assert!(output.contains("gateway_provider_queue_duration_ms"));
        assert!(output.contains("gateway_provider_ttft_ms"));
        assert!(output.contains("gateway_provider_work_duration_ms"));
        assert!(output.contains("gateway_chat_total_duration_ms"));
        assert!(output.contains("gateway_overhead_ms"));
        assert!(output.contains(r#"provider="chatgpt""#));
        assert!(!output.contains("private-conversation-id"));
    }
}
