use crate::config::AppConfig;
use crate::memory::{ConversationStore, UnifiedMemory};
use std::sync::Arc;

/// Provider priority + failover routing. Explicit model names must resolve;
/// only "auto" opts into routing across all enabled providers.
#[derive(Clone)]
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
    #[allow(dead_code)]
    pub fn resolve(
        &self,
        model: &str,
        conversation_id: Option<&str>,
        memory: &UnifiedMemory,
    ) -> anyhow::Result<Vec<String>> {
        self.resolve_for_owner("local", model, conversation_id, memory)
    }

    pub fn resolve_for_owner(
        &self,
        owner_key_id: &str,
        model: &str,
        conversation_id: Option<&str>,
        memory: &UnifiedMemory,
    ) -> anyhow::Result<Vec<String>> {
        let candidates = self.resolve_without_affinity(model);
        self.with_affinity(owner_key_id, candidates, conversation_id, memory)
    }

    /// Resolve providers using the async conversation-store contract so
    /// affinity metadata reads never block a Tokio request worker.
    pub async fn resolve_for_owner_async<T>(
        &self,
        owner_key_id: &str,
        model: &str,
        conversation_id: Option<&str>,
        memory: std::sync::Arc<T>,
    ) -> anyhow::Result<Vec<String>>
    where
        T: ConversationStore + 'static,
    {
        let candidates = self.resolve_without_affinity(model);
        self.with_affinity_async(owner_key_id, candidates, conversation_id, memory)
            .await
    }

    fn resolve_without_affinity(&self, model: &str) -> Vec<String> {
        let model_lc = model.trim().to_lowercase();
        if model_lc == "auto" {
            return self.enabled_in_priority_order();
        }
        let (pinned_provider, requested_model) = match model_lc.split_once('/') {
            Some((provider, requested)) if !provider.is_empty() && !requested.is_empty() => {
                (Some(provider), requested)
            }
            _ => (None, model_lc.as_str()),
        };
        self.cfg
            .providers
            .iter()
            .filter(|provider| provider.enabled)
            .filter(|provider| {
                pinned_provider
                    .map(|name| provider.name.eq_ignore_ascii_case(name))
                    .unwrap_or(true)
            })
            .filter(|provider| {
                provider
                    .models
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(requested_model))
            })
            .map(|provider| provider.name.clone())
            .collect()
    }

    async fn with_affinity_async<T>(
        &self,
        owner_key_id: &str,
        mut candidates: Vec<String>,
        conversation_id: Option<&str>,
        memory: std::sync::Arc<T>,
    ) -> anyhow::Result<Vec<String>>
    where
        T: ConversationStore + 'static,
    {
        if let Some(conversation_id) = conversation_id {
            if let Some(previous_provider) = memory
                .get_provider_for_owner_async(owner_key_id.to_owned(), conversation_id.to_owned())
                .await?
            {
                if let Some(position) = candidates.iter().position(|candidate| candidate == &previous_provider) {
                    let first = candidates.remove(position);
                    candidates.insert(0, first);
                }
            }
        }
        Ok(candidates)
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
    use crate::memory::ConversationStore;
    use crate::config::{
        AppConfig, AuthConfig, MemoryConfig, ObscuraConfig, ProviderConfig, ServerConfig,
    };

    fn router() -> ProviderRouter {
        let cfg = AppConfig {
            server: ServerConfig::default(),
            memory: MemoryConfig::default(),
            obscura: ObscuraConfig::default(),
            auth: AuthConfig::default(),
            providers: vec![
                ProviderConfig {
                    name: "claude".into(),
                    enabled: true,
                    priority: 1,
                    timeout_seconds: 30,
                    queue_timeout_seconds: 30,
                    session_pool_size: 1,
                    min_request_interval_ms: 5_000,
                    daily_request_cap: 100,
                    models: vec!["sonnet".into()],
                    completion_idle_ms: 15_000,
                },
                ProviderConfig {
                    name: "chatgpt".into(),
                    enabled: true,
                    priority: 2,
                    timeout_seconds: 30,
                    queue_timeout_seconds: 30,
                    session_pool_size: 1,
                    min_request_interval_ms: 5_000,
                    daily_request_cap: 100,
                    models: vec!["gpt-4o".into()],
                    completion_idle_ms: 15_000,
                },
            ],
        };
        ProviderRouter::new(Arc::new(cfg))
    }

    #[test]
    fn unknown_model_does_not_fall_back_to_unrelated_provider() {
        let r = router();
        let memory = UnifiedMemory::new(1000);
        assert!(r
            .resolve("made-up-model", None, &memory)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn provider_can_be_pinned() {
        let r = router();
        let memory = UnifiedMemory::new(1000);
        assert_eq!(
            r.resolve("chatgpt/gpt-4o", None, &memory).unwrap(),
            vec!["chatgpt"]
        );
        assert!(r
            .resolve("claude/gpt-4o", None, &memory)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn async_resolution_uses_conversation_affinity_without_blocking_api() {
        use crate::memory::ChatMessage;
        let router = router();
        let memory = Arc::new(UnifiedMemory::new(1000));
        memory
            .clone()
            .persist_turn(
                "owner-a".into(),
                "conversation-123".into(),
                vec![ChatMessage { role: "user".into(), content: "hello".into() }],
                ChatMessage { role: "assistant".into(), content: "world".into() },
                "chatgpt".into(),
                None,
            )
            .await
            .unwrap();

        let candidates = router
            .resolve_for_owner_async(
                "owner-a",
                "auto",
                Some("conversation-123"),
                memory,
            )
            .await
            .unwrap();
        assert_eq!(candidates, vec!["chatgpt", "claude"]);
    }

    #[tokio::test]
    async fn async_resolution_keeps_owner_affinity_isolated() {
        let router = router();
        let memory = Arc::new(UnifiedMemory::new(1000));
        memory
            .clone()
            .persist_turn(
                "owner-a".into(),
                "conversation-123".into(),
                vec![crate::memory::ChatMessage { role: "user".into(), content: "hello".into() }],
                crate::memory::ChatMessage { role: "assistant".into(), content: "world".into() },
                "chatgpt".into(),
                None,
            )
            .await
            .unwrap();

        let owner_b_candidates = router
            .resolve_for_owner_async(
                "owner-b",
                "auto",
                Some("conversation-123"),
                memory,
            )
            .await
            .unwrap();
        assert_eq!(owner_b_candidates, vec!["claude", "chatgpt"]);
    }

    #[test]
    fn auto_uses_enabled_priority_order() {
        let r = router();
        let memory = UnifiedMemory::new(1000);
        assert_eq!(
            r.resolve("auto", None, &memory).unwrap(),
            vec!["claude", "chatgpt"]
        );
    }
}
