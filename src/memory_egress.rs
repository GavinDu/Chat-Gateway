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
