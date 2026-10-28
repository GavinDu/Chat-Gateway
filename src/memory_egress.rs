//! Fail-closed policy for gateway-injected data sent to provider adapters.
//!
//! Conversation transcripts and user-supplied current-turn content are distinct
//! from retrieved personal long-term memory. Long-term memory is denied by
//! default; linking a provider is not consent to disclose it.
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderDataClass {
    CurrentTurn,
    ConversationContext,
    PersonalLongTermMemory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersonalMemoryItem {
    pub id: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EgressError {
    UnknownProvider,
    PersonalMemoryDestinationDenied,
}

#[derive(Debug, Clone)]
pub(crate) struct MemoryEgressPolicy {
    allowed_personal_memory_providers: HashSet<String>,
}

impl MemoryEgressPolicy {
    /// An empty/missing allowlist means no provider may receive personal memory.
    /// Invalid provider names make policy construction fail rather than silently
    /// broadening the set of destinations.
    pub(crate) fn from_env() -> anyhow::Result<Self> {
        let configured = std::env::var("CHAT_GATEWAY_MEMORY_EGRESS_ALLOWLIST").unwrap_or_default();
        Self::from_allowlist(&configured)
    }

    /// Sorted provider IDs for the authenticated policy-inspection endpoint.
    pub(crate) fn allowed_personal_memory_providers(&self) -> Vec<String> {
        let mut providers = self
            .allowed_personal_memory_providers
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        providers.sort();
        providers
    }

    /// Parse an explicit comma-separated allowlist. Keeping parsing pure makes
    /// policy validation deterministic and testable without process-global env.
    pub(crate) fn from_allowlist(configured: &str) -> anyhow::Result<Self> {
        let mut allowed_personal_memory_providers = HashSet::new();
        for entry in configured
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let provider = entry.to_ascii_lowercase();
            if crate::providers::Provider::from_name(&provider).is_none() {
                anyhow::bail!("unknown provider in memory egress allowlist");
            }
            allowed_personal_memory_providers.insert(provider);
        }
        Ok(Self {
            allowed_personal_memory_providers,
        })
    }

    /// Return the effective explicit allowlist for safe operator inspection.
    pub(crate) fn allowed_provider_ids(&self) -> Vec<String> {
        let mut providers = self
            .allowed_personal_memory_providers
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        providers.sort();
        providers
    }

    pub(crate) fn authorize(
        &self,
        provider: &str,
        data_class: ProviderDataClass,
    ) -> Result<(), EgressError> {
        let provider = provider.to_ascii_lowercase();
        if crate::providers::Provider::from_name(&provider).is_none() {
            return Err(EgressError::UnknownProvider);
        }
        match data_class {
            ProviderDataClass::CurrentTurn | ProviderDataClass::ConversationContext => Ok(()),
            ProviderDataClass::PersonalLongTermMemory => {
                if self.allowed_personal_memory_providers.contains(&provider) {
                    Ok(())
                } else {
                    Err(EgressError::PersonalMemoryDestinationDenied)
                }
            }
        }
    }

    /// Append only policy-authorized personal memory to the final provider prompt.
    /// Keeping filtering and rendering in one function prevents callers from
    /// accidentally appending the original unfiltered collection.
    pub(crate) fn append_personal_memory_to_prompt(
        &self,
        provider: &str,
        prompt: &mut String,
        items: &[PersonalMemoryItem],
    ) -> Result<(), EgressError> {
        for item in self.filter_personal_memory(provider, items)? {
            prompt.push_str("\nPERSONAL_MEMORY_ITEM ");
            prompt.push_str(&item.id);
            prompt.push_str(": ");
            prompt.push_str(&item.content);
        }
        Ok(())
    }

    /// Append only policy-authorized personal memory to the outbound prompt.
    /// This is the single prompt-rendering boundary used by the send path.
    pub(crate) fn append_personal_memory_to_prompt(
        &self,
        provider: &str,
        prompt: &mut String,
        items: &[PersonalMemoryItem],
    ) -> Result<(), EgressError> {
        for item in self.filter_personal_memory(provider, items)? {
            prompt.push_str("\nPERSONAL_MEMORY_ITEM ");
            prompt.push_str(&item.id);
            prompt.push_str(": ");
            prompt.push_str(&item.content);
        }
        Ok(())
    }

    /// Filter personal memory at the provider boundary. Denials log only stable
    /// item IDs and a reason; the sensitive memory value is never logged.
    pub(crate) fn filter_personal_memory<'a>(
        &self,
        provider: &str,
        items: &'a [PersonalMemoryItem],
    ) -> Result<Vec<&'a PersonalMemoryItem>, EgressError> {
        self.authorize(provider, ProviderDataClass::CurrentTurn)?;
        let mut allowed = Vec::with_capacity(items.len());
        for item in items {
            match self.authorize(provider, ProviderDataClass::PersonalLongTermMemory) {
                Ok(()) => allowed.push(item),
                Err(EgressError::PersonalMemoryDestinationDenied) => {
                    tracing::warn!(
                        provider,
                        item_id = %item.id,
                        reason = "personal_memory_destination_not_allowed",
                        "personal memory item withheld from provider"
                    );
                }
                Err(error) => return Err(error),
            }
        }
        Ok(allowed)
    }
}

#[cfg(test)]
mod tests {
    use super::{EgressError, MemoryEgressPolicy, PersonalMemoryItem, ProviderDataClass};
    use std::collections::HashSet;

    fn policy(allowed: &[&str]) -> MemoryEgressPolicy {
        MemoryEgressPolicy {
            allowed_personal_memory_providers: allowed
                .iter()
                .map(|p| (*p).to_owned())
                .collect::<HashSet<_>>(),
        }
    }

    #[test]
    fn allowlist_parser_defaults_to_deny_and_normalizes_provider_names() {
        let empty = MemoryEgressPolicy::from_allowlist(" , ").unwrap();
        assert_eq!(
            empty.authorize("chatgpt", ProviderDataClass::PersonalLongTermMemory),
            Err(EgressError::PersonalMemoryDestinationDenied)
        );

        let allowed = MemoryEgressPolicy::from_allowlist(" ChatGPT, claude,chatgpt ").unwrap();
        assert_eq!(
            allowed.allowed_personal_memory_providers(),
            vec!["chatgpt".to_owned(), "claude".to_owned()]
        );
        assert_eq!(
            allowed.authorize("chatgpt", ProviderDataClass::PersonalLongTermMemory),
            Ok(())
        );
        assert_eq!(
            allowed.authorize("claude", ProviderDataClass::PersonalLongTermMemory),
            Ok(())
        );
    }

    #[test]
    fn allowlist_parser_rejects_unknown_provider_without_echoing_input() {
        let error = MemoryEgressPolicy::from_allowlist("chatgpt,secret-provider-name")
            .unwrap_err()
            .to_string();
        assert_eq!(error, "unknown provider in memory egress allowlist");
        assert!(!error.contains("secret-provider-name"));
    }

    #[test]
    fn personal_memory_is_denied_by_default_but_current_turn_and_transcript_are_distinct() {
        let policy = policy(&[]);
        assert_eq!(
            policy.authorize("chatgpt", ProviderDataClass::PersonalLongTermMemory),
            Err(EgressError::PersonalMemoryDestinationDenied)
        );
        assert_eq!(
            policy.authorize("chatgpt", ProviderDataClass::CurrentTurn),
            Ok(())
        );
        assert_eq!(
            policy.authorize("chatgpt", ProviderDataClass::ConversationContext),
            Ok(())
        );
    }

    #[test]
    fn explicitly_allowed_provider_receives_only_personal_memory_items_in_scope() {
        let policy = policy(&["chatgpt"]);
        let items = vec![
            PersonalMemoryItem {
                id: "mem-1".into(),
                content: "permitted fact".into(),
            },
            PersonalMemoryItem {
                id: "mem-2".into(),
                content: "another permitted fact".into(),
            },
        ];
        let filtered = policy.filter_personal_memory("chatgpt", &items).unwrap();
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].id, "mem-1");
        assert!(policy
            .filter_personal_memory("claude", &items)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn denied_memory_sentinel_never_reaches_the_rendered_provider_prompt() {
        let policy = policy(&[]);
        let items = vec![PersonalMemoryItem {
            id: "private-item-17".into(),
            content: "PRIVATE_MEMORY_SENTINEL_7f1d".into(),
        }];
        let mut prompt = "user: what should I remember?".to_owned();

        policy
            .append_personal_memory_to_prompt("chatgpt", &mut prompt, &items)
            .unwrap();

        assert!(!prompt.contains("PRIVATE_MEMORY_SENTINEL_7f1d"));
        assert!(!prompt.contains("private-item-17"));
    }

    #[test]
    fn explicitly_allowed_memory_is_appended_to_the_rendered_provider_prompt() {
        let policy = policy(&["chatgpt"]);
        let items = vec![PersonalMemoryItem {
            id: "allowed-item-1".into(),
            content: "allowed memory fact".into(),
        }];
        let mut prompt = "user: question".to_owned();

        policy
            .append_personal_memory_to_prompt("chatgpt", &mut prompt, &items)
            .unwrap();

        assert!(prompt.contains("PERSONAL_MEMORY_ITEM allowed-item-1: allowed memory fact"));
    }

    #[test]
    fn denied_personal_memory_sentinel_never_enters_outbound_prompt() {
        let policy = policy(&[]);
        let items = vec![PersonalMemoryItem {
            id: "private-item-17".into(),
            content: "SENTINEL_DO_NOT_SEND_9f73".into(),
        }];
        let mut outbound_prompt = "user: summarize my request".to_owned();

        policy
            .append_personal_memory_to_prompt("chatgpt", &mut outbound_prompt, &items)
            .unwrap();

        assert!(outbound_prompt.contains("summarize my request"));
        assert!(!outbound_prompt.contains("SENTINEL_DO_NOT_SEND_9f73"));
        assert!(!outbound_prompt.contains("private-item-17"));
    }

    #[test]
    fn allowed_personal_memory_is_appended_to_outbound_prompt() {
        let policy = policy(&["chatgpt"]);
        let items = vec![PersonalMemoryItem {
            id: "allowed-item-1".into(),
            content: "allowed fact".into(),
        }];
        let mut outbound_prompt = "user: question".to_owned();

        policy
            .append_personal_memory_to_prompt("chatgpt", &mut outbound_prompt, &items)
            .unwrap();

        assert!(outbound_prompt.contains("allowed-item-1"));
        assert!(outbound_prompt.contains("allowed fact"));
    }

    #[test]
    fn unknown_provider_fails_closed_without_exposing_memory_content() {
        let policy = policy(&[]);
        assert_eq!(
            policy.filter_personal_memory(
                "unregistered-provider",
                &[PersonalMemoryItem {
                    id: "mem-secret".into(),
                    content: "sensitive sentinel".into()
                }]
            ),
            Err(EgressError::UnknownProvider)
        );
    }
}
