use crate::config::AppConfig;
use crate::memory::UnifiedMemory;
use std::sync::Arc;

/// Provider priority + failover routing. Explicit model names must resolve;
/// only "auto" opts into routing across all enabled providers.
pub struct ProviderRouter {
    cfg: Arc<AppConfig>,
}

impl ProviderRouter {
    pub fn new(cfg: Arc<AppConfig>) -> Self {
        Self { cfg }
    }

    /// Resolve an explicit model to providers that advertise that exact model.
    /// A provider can be pinned with "provider/model". Unknown model names
    /// return an empty list instead of silently routing to an unrelated model.
    pub fn resolve(
        &self,
        model: &str,
        conversation_id: Option<&str>,
        memory: &UnifiedMemory,
    ) -> Vec<String> {
        let model_lc = model.trim().to_lowercase();
        if model_lc == "auto" {
            return self.with_affinity(self.enabled_in_priority_order(), conversation_id, memory);
        }

        let (pinned_provider, requested_model) = match model_lc.split_once('/') {
            Some((provider, requested)) if !provider.is_empty() && !requested.is_empty() => {
                (Some(provider), requested)
            }
            _ => (None, model_lc.as_str()),
        };

        let candidates: Vec<String> = self
            .cfg
            .providers
            .iter()
            .filter(|p| p.enabled)
            .filter(|p| {
                pinned_provider
                    .map(|name| p.name.eq_ignore_ascii_case(name))
                    .unwrap_or(true)
            })
            .filter(|p| {
                p.models
                    .iter()
                    .any(|m| m.eq_ignore_ascii_case(requested_model))
            })
            .map(|p| p.name.clone())
            .collect();

        self.with_affinity(candidates, conversation_id, memory)
    }

    fn with_affinity(
        &self,
        mut candidates: Vec<String>,
        conversation_id: Option<&str>,
        memory: &UnifiedMemory,
    ) -> Vec<String> {
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
        self.cfg
            .providers
            .iter()
            .filter(|p| p.enabled)
            .map(|p| p.name.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, MemoryConfig, ObscuraConfig, ProviderConfig, ServerConfig};

    fn router() -> ProviderRouter {
        let cfg = AppConfig {
            server: ServerConfig::default(),
            memory: MemoryConfig::default(),
            obscura: ObscuraConfig::default(),
            providers: vec![
                ProviderConfig {
                    name: "claude".into(),
                    enabled: true,
                    priority: 1,
                    timeout_seconds: 30,
                    session_pool_size: 1,
                    models: vec!["sonnet".into()],
                },
                ProviderConfig {
                    name: "chatgpt".into(),
                    enabled: true,
                    priority: 2,
                    timeout_seconds: 30,
                    session_pool_size: 1,
                    models: vec!["gpt-4o".into()],
                },
            ],
        };
        ProviderRouter::new(Arc::new(cfg))
    }

    #[test]
    fn unknown_model_does_not_fall_back_to_unrelated_provider() {
        let r = router();
        let memory = UnifiedMemory::new(1000);
        assert!(r.resolve("made-up-model", None, &memory).is_empty());
    }

    #[test]
    fn provider_can_be_pinned() {
        let r = router();
        let memory = UnifiedMemory::new(1000);
        assert_eq!(r.resolve("chatgpt/gpt-4o", None, &memory), vec!["chatgpt"]);
        assert!(r.resolve("claude/gpt-4o", None, &memory).is_empty());
    }

    #[test]
    fn auto_uses_enabled_priority_order() {
        let r = router();
        let memory = UnifiedMemory::new(1000);
        assert_eq!(r.resolve("auto", None, &memory), vec!["claude", "chatgpt"]);
    }
}
