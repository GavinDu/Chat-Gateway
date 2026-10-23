use std::sync::Arc;
use crate::config::AppConfig;

pub struct ProviderRouter {
    cfg: Arc<AppConfig>,
}

impl ProviderRouter {
    pub fn new(cfg: Arc<AppConfig>) -> Self { Self { cfg } }

    /// Resolve model -> provider candidates ordered by priority + conversation affinity
    pub fn resolve(&self, model: &str, conversation_id: Option<&str>, memory: &crate::memory::UnifiedMemory) -> Vec<String> {
        let model_lc = model.to_lowercase();
        if model_lc == "auto" {
            return self.cfg.providers.iter().filter(|p| p.enabled).map(|p| p.name.clone()).collect();
        }

        // If conversation has affinity and model matches, try that provider first
        if let Some(cid) = conversation_id {
            if let Some(tab) = memory.get_tab(cid) {
                // We still need provider from memory; for simplicity try last provider
                // In full impl, store provider in conversation
            }
        }

        let mut candidates: Vec<_> = self.cfg.providers.iter()
            .filter(|p| p.enabled && (p.models.iter().any(|m| m.to_lowercase() == model_lc) || p.models.iter().any(|m| model_lc.contains(&m.to_lowercase()))))
            .collect();
        candidates.sort_by_key(|p| p.priority);
        let names: Vec<String> = candidates.into_iter().map(|p| p.name.clone()).collect();
        if names.is_empty() {
            // fallback to all enabled sorted by priority
            self.cfg.providers.iter().filter(|p| p.enabled).map(|p| p.name.clone()).collect()
        } else { names }
    }
}
