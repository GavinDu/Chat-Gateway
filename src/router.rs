use std::sync::Arc;
use crate::config::AppConfig;
use crate::memory::UnifiedMemory;

/// Provider priority + failover routing (v1 semantics): `auto` tries all
/// enabled providers in priority order; a model alias resolves to its
/// provider(s) in priority order; no match falls back to all enabled.
/// A conversation with provider affinity tries its last provider first.
pub struct ProviderRouter {
    cfg: Arc<AppConfig>,
}

impl ProviderRouter {
    pub fn new(cfg: Arc<AppConfig>) -> Self { Self { cfg } }

    /// Resolve model -> provider candidates ordered by priority + conversation affinity
    pub fn resolve(&self, model: &str, conversation_id: Option<&str>, memory: &UnifiedMemory) -> Vec<String> {
        let model_lc = model.to_lowercase();
        if model_lc == "auto" {
            return self.enabled_in_priority_order();
        }

        let mut candidates: Vec<String> = self.cfg.providers.iter()
            .filter(|p| p.enabled && p.models.iter().any(|m| {
                let ml = m.to_lowercase();
                ml == model_lc || model_lc.contains(&ml)
            }))
            .map(|p| p.name.clone())
            .collect();
        if candidates.is_empty() {
            candidates = self.enabled_in_priority_order();
        }

        // Conversation affinity: keep a conversation on its provider when eligible
        if let Some(cid) = conversation_id {
            if let Some(prev) = memory.get_provider(cid) {
                if let Some(pos) = candidates.iter().position(|c| c == &prev) {
                    let first = candidates.remove(pos);
                    candidates.insert(0, first);
                }
            }
        }
        candidates
    }

    fn enabled_in_priority_order(&self) -> Vec<String> {
        // cfg.providers is sorted by priority at load time
        self.cfg.providers.iter().filter(|p| p.enabled).map(|p| p.name.clone()).collect()
    }
}
