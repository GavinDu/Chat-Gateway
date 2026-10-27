use crate::memory::ChatMessage;

/// Pure context planning helpers. This module intentionally has no database,
/// provider, or async-runtime dependency so policy can be tested independently.
pub(crate) struct ContextBuilder;

impl ContextBuilder {
    /// Reconcile a durable history with an incoming OpenAI-style transcript,
    /// then retain the newest messages that fit the configured token budget.
    pub(crate) fn build(
        history: &[ChatMessage],
        incoming: &[ChatMessage],
        budget: usize,
    ) -> (String, Vec<ChatMessage>) {
        let mut full = history.to_vec();
        full.extend(messages_after_overlap(&full, incoming));

        let mut tokens = 0usize;
        let mut windowed = Vec::new();
        for message in full.iter().rev() {
            let cost = estimate_message_tokens(&message.content).saturating_add(8);
            if tokens.saturating_add(cost) > budget && !windowed.is_empty() {
                break;
            }
            tokens = tokens.saturating_add(cost);
            windowed.push(message.clone());
        }
        windowed.reverse();
        let prompt = render_prompt(&windowed);
        (prompt, windowed)
    }
}

pub(crate) fn render_prompt(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .map(|message| format!("{}: {}", message.role, message.content))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Conservative, dependency-free estimate for browser-backed providers without
/// authoritative tokenizer usage. Count multibyte characters separately.
pub(crate) fn estimate_message_tokens(content: &str) -> usize {
    let mut ascii = 0usize;
    let mut non_ascii = 0usize;
    for character in content.chars() {
        if character.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii.div_ceil(4) + non_ascii
}

pub(crate) fn messages_after_overlap(
    stored: &[ChatMessage],
    incoming: &[ChatMessage],
) -> Vec<ChatMessage> {
    if incoming.is_empty() {
        return Vec::new();
    }
    let common_prefix = stored
        .iter()
        .zip(incoming)
        .take_while(|(stored, incoming)| stored == incoming)
        .count();
    // A complete shared prefix can precede independently committed concurrent
    // turns. Reconcile it even when the incoming branch is no longer a suffix.
    // A single matching user message alone may instead be an intentional repeat.
    if common_prefix == stored.len() || common_prefix >= 2 {
        return incoming[common_prefix..].to_vec();
    }
    for overlap in (1..=stored.len().min(incoming.len())).rev() {
        if stored[stored.len() - overlap..] == incoming[..overlap] {
            return incoming[overlap..].to_vec();
        }
    }
    incoming.to_vec()
}

#[cfg(test)]
#[allow(unused_imports)] // The standalone benchmark includes this module with test cfg enabled.
mod tests {
    #[allow(unused_imports)]
    use super::{estimate_message_tokens, messages_after_overlap, ContextBuilder};
    use crate::memory::ChatMessage;

    fn m(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn pure_builder_reconciles_transcript_and_keeps_recent_context() {
        let history = vec![m("user", "first"), m("assistant", "answer first")];
        let incoming = vec![
            m("user", "first"),
            m("assistant", "answer first"),
            m("user", "second"),
        ];
        let (prompt, messages) = ContextBuilder::build(&history, &incoming, 1000);
        assert_eq!(messages.len(), 3);
        assert!(prompt.ends_with("user: second"));
    }

    #[test]
    fn pure_builder_bounds_context_to_recent_messages() {
        let history = vec![
            m("user", "an old message that should fall outside the budget"),
            m(
                "assistant",
                "an old answer that should fall outside the budget",
            ),
            m("user", "latest"),
        ];
        let (_, messages) = ContextBuilder::build(&history, &[], 10);
        assert!(messages.iter().any(|message| message.content == "latest"));
        assert!(messages.len() < history.len());
    }

    #[test]
    fn overlap_handles_repeated_prompts_and_unicode_estimates() {
        let stored = vec![m("user", "repeat"), m("assistant", "first answer")];
        let incoming = vec![m("user", "repeat")];
        assert_eq!(messages_after_overlap(&stored, &incoming), incoming);
        assert!(estimate_message_tokens("你好🙂") >= 3);
    }
}
