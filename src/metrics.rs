use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

const BOUNDS: &[u64] = &[5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000];

#[derive(Default)]
struct Histogram {
    // Counts for the first matching finite bucket plus the +Inf bucket.
    buckets: [u64; 12],
    count: u64,
    sum: u128,
}

impl Histogram {
    fn observe(&mut self, value: u64) {
        let bucket = BOUNDS
            .iter()
            .position(|bound| value <= *bound)
            .unwrap_or(BOUNDS.len());
        self.buckets[bucket] += 1;
        self.count += 1;
        self.sum += value as u128;
    }

    fn render(&self, out: &mut String, metric: &str, label: &str, key: &str) {
        let escaped = escape_label(key);
        let mut cumulative = 0u64;
        for (index, bound) in BOUNDS.iter().enumerate() {
            cumulative += self.buckets[index];
            let _ = writeln!(
                out,
                "{metric}_bucket{{{label}=\"{escaped}\",le=\"{bound}\"}} {cumulative}"
            );
        }
        cumulative += self.buckets[BOUNDS.len()];
        let _ = writeln!(
            out,
            "{metric}_bucket{{{label}=\"{escaped}\",le=\"+Inf\"}} {cumulative}"
        );
        let _ = writeln!(out, "{metric}_sum{{{label}=\"{escaped}\"}} {}", self.sum);
        let _ = writeln!(
            out,
            "{metric}_count{{{label}=\"{escaped}\"}} {}",
            self.count
        );
    }
}

#[derive(Default)]
struct MetricData {
    requests: BTreeMap<(String, u16), u64>,
    handler_ms: BTreeMap<String, Histogram>,
    queue_ms: BTreeMap<String, Histogram>,
    ttft_ms: BTreeMap<String, Histogram>,
    provider_work_ms: BTreeMap<String, Histogram>,
    provider_tail_ms: BTreeMap<String, Histogram>,
    chat_total_ms: BTreeMap<String, Histogram>,
    gateway_overhead_ms: BTreeMap<String, Histogram>,
    poll_tick_ms: BTreeMap<String, Histogram>,
    poll_evaluations: BTreeMap<String, u64>,
    provider_outcomes: BTreeMap<(String, String), u64>,
    browser_worker_restarts: u64,
}

/// Per-gateway metrics. Clones share counters; independent gateways remain isolated.
#[derive(Clone, Default)]
pub struct MetricsRegistry {
    data: Arc<Mutex<MetricData>>,
}

impl MetricsRegistry {
    /// Historical successful completions in this gateway process, independent
    /// of whether a page probe currently sees an input.
    pub fn has_successful_chat(&self, provider: &str) -> bool {
        self.data
            .lock()
            .ok()
            .and_then(|data| {
                data.provider_outcomes
                    .get(&(provider.to_owned(), "success".to_owned()))
                    .copied()
            })
            .unwrap_or(0)
            > 0
    }
    pub fn record_http_request(&self, route: &str, status: u16, duration_ms: u64) {
        if let Ok(mut data) = self.data.lock() {
            *data.requests.entry((route.to_owned(), status)).or_default() += 1;
            data.handler_ms
                .entry(route.to_owned())
                .or_default()
                .observe(duration_ms);
        }
    }

    /// Record one browser polling tick and the number of DOM evaluations used.
    pub fn record_browser_poll(&self, provider: &str, evaluations: u64, duration_ms: u64) {
        if let Ok(mut data) = self.data.lock() {
            data.poll_tick_ms
                .entry(provider.to_owned())
                .or_default()
                .observe(duration_ms);
            *data
                .poll_evaluations
                .entry(provider.to_owned())
                .or_default() += evaluations;
        }
    }

    /// Record the completion tail: elapsed time from the most recent emitted delta to done.
    pub fn record_browser_worker_restart(&self) {
        if let Ok(mut data) = self.data.lock() {
            data.browser_worker_restarts += 1;
        }
    }

    pub fn record_provider_tail(&self, provider: &str, duration_ms: u64) {
        if let Ok(mut data) = self.data.lock() {
            data.provider_tail_ms
                .entry(provider.to_owned())
                .or_default()
                .observe(duration_ms);
        }
    }

    pub fn record_provider_timings(
        &self,
        provider: &str,
        outcome: &str,
        queue_ms: u64,
        ttft_ms: Option<u64>,
        provider_work_ms: u64,
        total_ms: u64,
    ) {
        if let Ok(mut data) = self.data.lock() {
            data.queue_ms
                .entry(provider.to_owned())
                .or_default()
                .observe(queue_ms);
            if let Some(ttft) = ttft_ms {
                data.ttft_ms
                    .entry(provider.to_owned())
                    .or_default()
                    .observe(ttft);
            }
            data.provider_work_ms
                .entry(provider.to_owned())
                .or_default()
                .observe(provider_work_ms);
            data.chat_total_ms
                .entry(provider.to_owned())
                .or_default()
                .observe(total_ms);
            data.gateway_overhead_ms
                .entry(provider.to_owned())
                .or_default()
                .observe(total_ms.saturating_sub(queue_ms.saturating_add(provider_work_ms)));
            *data
                .provider_outcomes
                .entry((provider.to_owned(), outcome.to_owned()))
                .or_default() += 1;
        }
    }
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Restrict metric labels to a bounded route vocabulary to avoid cardinality attacks.
pub fn normalize_route(path: &str) -> String {
    if path.starts_with("/v1/admin/providers/") && path.ends_with("/reset") {
        "/v1/admin/providers/:id/reset".to_owned()
    } else if path.starts_with("/v1/conversations/") {
        "/v1/conversations/:id".to_owned()
    } else if path.starts_with("/login/api/") {
        "/login/api/*".to_owned()
    } else if path.starts_with("/relay/v1/session/") {
        "/relay/v1/session/:provider".to_owned()
    } else if path.starts_with("/relay/approve/") {
        "/relay/approve/:id".to_owned()
    } else if matches!(
        path,
        "/relay/v1/status"
            | "/relay/v1/connect/request"
            | "/relay/v1/connect/poll"
            | "/relay/v1/pair"
            | "/relay/v1/unpair"
            | "/relay/v1/pairing-code/rotate"
    ) {
        path.to_owned()
    } else if path.starts_with("/relay/") {
        "/relay/*".to_owned()
    } else if matches!(
        path,
        "/health" | "/ready" | "/metrics" | "/v1/models" | "/v1/chat/completions" | "/login"
    ) {
        path.to_owned()
    } else {
        "/other".to_owned()
    }
}

/// Render metrics plus the current session state for each configured provider.
/// State values are emitted as a bounded one-hot gauge for dashboards.
impl MetricsRegistry {
    pub fn render_with_provider_states(&self, provider_states: &[(String, String)]) -> String {
        let Ok(data) = self.data.lock() else {
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
        for (metric, values) in [
            ("gateway_http_handler_duration_ms", &data.handler_ms),
            ("gateway_provider_queue_duration_ms", &data.queue_ms),
            ("gateway_provider_ttft_ms", &data.ttft_ms),
            ("gateway_provider_work_duration_ms", &data.provider_work_ms),
            ("gateway_provider_tail_ms", &data.provider_tail_ms),
            ("gateway_chat_total_duration_ms", &data.chat_total_ms),
            ("gateway_overhead_ms", &data.gateway_overhead_ms),
        ] {
            let _ = writeln!(out, "# TYPE {metric} histogram");
            let label = if metric == "gateway_http_handler_duration_ms" {
                "route"
            } else {
                "provider"
            };
            for (key, histogram) in values {
                histogram.render(&mut out, metric, label, key);
            }
        }
        let _ = writeln!(out, "# TYPE gateway_poll_tick_ms histogram");
        for (provider, histogram) in &data.poll_tick_ms {
            histogram.render(&mut out, "gateway_poll_tick_ms", "provider", provider);
        }
        let _ = writeln!(out, "# TYPE gateway_poll_evaluations_total counter");
        for (provider, count) in &data.poll_evaluations {
            let _ = writeln!(
                out,
                "gateway_poll_evaluations_total{{provider=\"{}\"}} {count}",
                escape_label(provider)
            );
        }
        let _ = writeln!(out, "# TYPE gateway_provider_requests_total counter");
        for ((provider, outcome), count) in &data.provider_outcomes {
            let _ = writeln!(
                out,
                "gateway_provider_requests_total{{provider=\"{}\",outcome=\"{}\"}} {count}",
                escape_label(provider),
                escape_label(outcome)
            );
        }
        let _ = writeln!(out, "# TYPE gateway_browser_worker_restarts_total counter");
        let _ = writeln!(
            out,
            "gateway_browser_worker_restarts_total {}",
            data.browser_worker_restarts
        );
        let _ = writeln!(out, "# TYPE gateway_provider_session_state gauge");
        for (provider, state) in provider_states {
            for known_state in ["unknown", "ready", "needs_login", "degraded"] {
                let value = u8::from(state == known_state);
                let _ = writeln!(
                    out,
                    "gateway_provider_session_state{{provider=\"{}\",state=\"{}\"}} {value}",
                    escape_label(provider),
                    known_state
                );
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{escape_label, normalize_route, Histogram, MetricsRegistry};

    #[test]
    fn browser_worker_restart_metric_counts_successful_restarts() {
        let registry = MetricsRegistry::default();
        registry.record_browser_worker_restart();
        assert!(registry
            .render_with_provider_states(&[])
            .contains("gateway_browser_worker_restarts_total 1"));
    }

    #[test]
    fn browser_poll_metrics_track_evaluations_and_tick_duration() {
        let registry = MetricsRegistry::default();
        registry.record_browser_poll("poll-test", 1, 7);
        let metrics = registry.render_with_provider_states(&[]);
        assert!(metrics.contains("gateway_poll_evaluations_total{provider=\"poll-test\"} 1"));
        assert!(metrics.contains("gateway_poll_tick_ms_count{provider=\"poll-test\"} 1"));
    }

    #[test]
    fn provider_completion_tail_metric_records_observations() {
        let registry = MetricsRegistry::default();
        registry.record_provider_tail("tail-test", 1_234);
        let metrics = registry.render_with_provider_states(&[]);
        assert!(metrics.contains("gateway_provider_tail_ms_count{provider=\"tail-test\"} 1"));
        assert!(metrics.contains("gateway_provider_tail_ms_sum{provider=\"tail-test\"} 1234"));
    }

    #[test]
    fn metrics_text_envelope_remains_stable_after_runtime_injection() {
        let registry = MetricsRegistry::default();
        let actual =
            registry.render_with_provider_states(&[("chatgpt".to_owned(), "ready".to_owned())]);
        let expected = concat!(
            "# TYPE gateway_http_requests_total counter\n",
            "# TYPE gateway_http_handler_duration_ms histogram\n",
            "# TYPE gateway_provider_queue_duration_ms histogram\n",
            "# TYPE gateway_provider_ttft_ms histogram\n",
            "# TYPE gateway_provider_work_duration_ms histogram\n",
            "# TYPE gateway_provider_tail_ms histogram\n",
            "# TYPE gateway_chat_total_duration_ms histogram\n",
            "# TYPE gateway_overhead_ms histogram\n",
            "# TYPE gateway_poll_tick_ms histogram\n",
            "# TYPE gateway_poll_evaluations_total counter\n",
            "# TYPE gateway_provider_requests_total counter\n",
            "# TYPE gateway_browser_worker_restarts_total counter\n",
            "gateway_browser_worker_restarts_total 0\n",
            "# TYPE gateway_provider_session_state gauge\n",
            "gateway_provider_session_state{provider=\"chatgpt\",state=\"unknown\"} 0\n",
            "gateway_provider_session_state{provider=\"chatgpt\",state=\"ready\"} 1\n",
            "gateway_provider_session_state{provider=\"chatgpt\",state=\"needs_login\"} 0\n",
            "gateway_provider_session_state{provider=\"chatgpt\",state=\"degraded\"} 0\n",
        );
        assert_eq!(actual, expected);
    }

    #[test]
    fn route_labels_have_bounded_cardinality() {
        assert_eq!(
            normalize_route("/v1/admin/providers/chatgpt/reset"),
            "/v1/admin/providers/:id/reset"
        );
        assert_eq!(
            normalize_route("/v1/conversations/secret-id"),
            "/v1/conversations/:id"
        );
        assert_eq!(
            normalize_route("/random/attacker-controlled-path"),
            "/other"
        );
    }

    #[test]
    fn relay_diagnostics_distinguish_operations_without_capabilities_or_identifiers() {
        for (path, label) in [
            ("/relay/v1/status", "/relay/v1/status"),
            ("/relay/v1/connect/poll", "/relay/v1/connect/poll"),
            (
                "/relay/v1/session/private-provider",
                "/relay/v1/session/:provider",
            ),
            ("/relay/approve/secret-capability", "/relay/approve/:id"),
            ("/relay/v1/unrecognized-secret", "/relay/*"),
            ("/relay/v1/status/secret", "/relay/*"),
        ] {
            assert_eq!(normalize_route(path), label);
        }
    }

    #[test]
    fn provider_timings_are_scoped_to_registry_instance() {
        let first = MetricsRegistry::default();
        let second = MetricsRegistry::default();
        first.record_provider_timings("chatgpt", "success", 2, Some(4), 8, 12);
        assert!(first.render_with_provider_states(&[]).contains(
            "gateway_provider_requests_total{provider=\"chatgpt\",outcome=\"success\"} 1"
        ));
        assert!(!second
            .render_with_provider_states(&[])
            .contains("gateway_provider_requests_total{provider=\"chatgpt\""));
    }

    #[test]
    fn clones_share_registry_data() {
        let first = MetricsRegistry::default();
        first.clone().record_http_request("/health", 200, 3);
        assert!(first
            .render_with_provider_states(&[])
            .contains("gateway_http_requests_total{route=\"/health\",status=\"200\"} 1"));
    }

    #[test]
    fn histogram_observes_finite_and_infinite_buckets() {
        let mut histogram = Histogram::default();
        histogram.observe(5);
        histogram.observe(20_000);
        let mut out = String::new();
        histogram.render(&mut out, "test_metric", "provider", "test");
        assert!(out.contains("le=\"5\"} 1"));
        assert!(out.contains("le=\"+Inf\"} 2"));
        assert!(out.contains("test_metric_count{provider=\"test\"} 2"));
    }

    #[test]
    fn labels_are_escaped() {
        assert_eq!(escape_label("a\\b\"c\nd"), "a\\\\b\\\"c\\nd");
    }
}

#[cfg(test)]
mod verified_chat_tests {
    use super::*;
    #[test]
    fn configured_models_or_failed_requests_do_not_establish_successful_chat() {
        let metrics = MetricsRegistry::default();
        assert!(!metrics.has_successful_chat("grok"));
        metrics.record_provider_timings("grok", "error", 0, None, 10, 10);
        assert!(!metrics.has_successful_chat("grok"));
        metrics.record_provider_timings("grok", "success", 0, Some(1), 10, 10);
        assert!(metrics.has_successful_chat("grok"));
        assert!(!metrics.has_successful_chat("mistral"));
    }
}
