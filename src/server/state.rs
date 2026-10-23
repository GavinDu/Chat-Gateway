use std::sync::Arc;
use crate::{config::AppConfig, browser::{pool::BrowserPool, ObscuraSupervisor}, session::SessionStore, memory::UnifiedMemory, models::cache::ModelCache};
use tokio::sync::RwLock;
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceTrace {
    pub request_id: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub conversation_id: Option<String>,
    pub model: String,
    pub provider: String,
    pub provider_attempts: Vec<String>,

    // Waterfall phases (ms) - mirrors Go's original: parse, memory, context, routing, browser_acq, provider_queue, page_readiness, input, send, TTFT, generation, total
    pub parse_ms: u128,
    pub memory_ms: u128,
    pub context_ms: u128,
    pub routing_ms: u128,
    pub browser_acquisition_ms: u128,
    pub provider_queue_ms: u128,
    pub page_readiness_ms: u128,
    pub input_ms: u128,
    pub send_ms: u128,
    pub ttft_ms: u128,
    pub generation_ms: u128,

    // Generation breakdown
    pub generation_breakdown: GenerationBreakdown,

    pub response_update_count: usize,
    pub response_len: usize,
    pub total_ms: u128,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationBreakdown {
    pub first_update_latency_ms: u128,
    pub active_growth_ms: u128,
    pub settle_wait_ms: u128,
    pub delivery_ms: u128,
}

impl Default for GenerationBreakdown {
    fn default() -> Self {
        Self { first_update_latency_ms: 0, active_growth_ms: 0, settle_wait_ms: 0, delivery_ms: 0 }
    }
}

impl PerformanceTrace {
    pub fn new(request_id: String, model: String) -> Self {
        Self {
            request_id,
            timestamp: chrono::Utc::now(),
            conversation_id: None,
            model,
            provider: String::new(),
            provider_attempts: vec![],
            parse_ms: 0,
            memory_ms: 0,
            context_ms: 0,
            routing_ms: 0,
            browser_acquisition_ms: 0,
            provider_queue_ms: 0,
            page_readiness_ms: 0,
            input_ms: 0,
            send_ms: 0,
            ttft_ms: 0,
            generation_ms: 0,
            generation_breakdown: GenerationBreakdown::default(),
            response_update_count: 0,
            response_len: 0,
            total_ms: 0,
            success: false,
            error: None,
        }
    }
}

pub struct TraceBuffer {
    inner: RwLock<Vec<PerformanceTrace>>,
    cap: usize,
}

impl TraceBuffer {
    pub fn new(cap: usize) -> Self { Self { inner: RwLock::new(Vec::new()), cap } }

    pub async fn push(&self, trace: PerformanceTrace) {
        // Structured log entry - mirrors Go's `performance trace` log, no prompts/responses/session data
        tracing::info!(
            target: "performance_trace",
            request_id = %trace.request_id,
            provider = %trace.provider,
            model = %trace.model,
            total_ms = %trace.total_ms,
            parse_ms = %trace.parse_ms,
            memory_ms = %trace.memory_ms,
            context_ms = %trace.context_ms,
            routing_ms = %trace.routing_ms,
            browser_acq_ms = %trace.browser_acquisition_ms,
            queue_ms = %trace.provider_queue_ms,
            readiness_ms = %trace.page_readiness_ms,
            input_ms = %trace.input_ms,
            send_ms = %trace.send_ms,
            ttft_ms = %trace.ttft_ms,
            gen_ms = %trace.generation_ms,
            gen_first_update = %trace.generation_breakdown.first_update_latency_ms,
            gen_growth = %trace.generation_breakdown.active_growth_ms,
            gen_settle = %trace.generation_breakdown.settle_wait_ms,
            gen_delivery = %trace.generation_breakdown.delivery_ms,
            update_count = %trace.response_update_count,
            success = %trace.success,
            "performance trace"
        );

        let mut g = self.inner.write().await;
        g.push(trace);
        if g.len() > self.cap { g.remove(0); }
    }

    pub async fn list(&self) -> Vec<PerformanceTrace> { self.inner.read().await.clone() }
    pub async fn latest(&self, n: usize) -> Vec<PerformanceTrace> {
        let g = self.inner.read().await;
        g.iter().rev().take(n).cloned().collect()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub browser_pool: BrowserPool,
    pub session_store: Arc<SessionStore>,
    pub memory: Arc<UnifiedMemory>,
    pub model_cache: Arc<ModelCache>,
    pub traces: Arc<TraceBuffer>,
    pub _supervisor: Arc<ObscuraSupervisor>,
}

