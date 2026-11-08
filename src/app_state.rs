use std::{collections::HashMap, sync::Arc};

use tokio::sync::RwLock;

use crate::{
    browser::pool::ObscuraPool, chat_service::ChatService, config::AppConfig,
    memory::UnifiedMemory, provider_guard::ProviderGuard, provider_state::ProviderSessionRegistry,
    relay::RelayState, router::ProviderRouter,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AuthKeyContext {
    /// Stable configured key name; never the raw bearer secret.
    pub(crate) key_id: String,
    pub(crate) provider_allowlist: Option<Vec<String>>,
}

pub(crate) struct AppState {
    pub(crate) cfg: Arc<AppConfig>,
    pub(crate) pool: Arc<ObscuraPool>,
    pub(crate) chat_service: ChatService,
    pub(crate) memory: Arc<UnifiedMemory>,
    pub(crate) router: ProviderRouter,
    pub(crate) models_cache: Arc<RwLock<HashMap<String, Vec<String>>>>,
    pub(crate) relay: Arc<RelayState>,
    /// Per-gateway provider pacing and challenge circuit-breaker state.
    pub(crate) provider_guard: ProviderGuard,
    /// Typed provider authentication and availability lifecycle.
    pub(crate) provider_sessions: ProviderSessionRegistry,
    pub(crate) metrics: crate::metrics::MetricsRegistry,
    /// Independent local-browser login tickets, revocations and shutdown.
    pub(crate) embedded_login: Arc<crate::embedded_login::EmbeddedLoginRuntime>,
}

/// A read-only eligibility probe must not consume a degraded provider's
/// one allowed half-open retry. A real dispatch claims that retry atomically.
#[derive(Clone, Copy)]
pub(crate) enum ProviderEligibilityMode {
    ReadOnly,
    ClaimRetry,
}

impl AppState {
    pub(crate) fn provider_eligible(&self, provider: &str, mode: ProviderEligibilityMode) -> bool {
        // A send may use an injected backend even when the native browser
        // worker is down. Only read-only readiness/model probes require the
        // native worker to be alive; backend admission is checked by send().
        if (matches!(mode, ProviderEligibilityMode::ReadOnly) && !self.pool.worker_alive())
            || !self
                .cfg
                .provider(provider)
                .is_some_and(|config| config.enabled)
            || self
                .provider_guard
                .provider_disabled_reason(provider)
                .is_some()
        {
            return false;
        }
        match mode {
            ProviderEligibilityMode::ReadOnly => self.provider_sessions.is_available(provider),
            ProviderEligibilityMode::ClaimRetry => self.provider_sessions.is_routable(provider),
        }
    }
}
