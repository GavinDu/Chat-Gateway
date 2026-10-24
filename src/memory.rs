use dashmap::DashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug)]
pub struct Conversation {
    pub id: String,
    pub messages: Vec<ChatMessage>,
    pub provider: Option<String>,
    pub tab_id: Option<String>, // page/tab affinity reused via X-Conversation-Id
}

/// Unified memory layer (v1 semantics): one conversation store shared
/// across all providers, sliding-window context by token estimate.
pub struct UnifiedMemory {
    max_tokens: usize,
    conversations: DashMap<String, Conversation>,
}

impl UnifiedMemory {
    pub fn new(max_tokens: usize) -> Self {
        Self { max_tokens, conversations: DashMap::new() }
    }

    /// Build prompt with sliding window, unified across providers.
    /// Returns (prompt, windowed messages).
    pub fn build_context(&self, conversation_id: Option<&str>, new_messages: &[ChatMessage]) -> (String, Vec<ChatMessage>) {
        let mut full = vec![];
        if let Some(cid) = conversation_id {
            if let Some(conv) = self.conversations.get(cid) {
                full.extend(conv.messages.clone());
            }
        }
        // Clients often resend the full transcript on each OpenAI-style
        // request. Reconcile it with the canonical transcript before
        // constructing context so old messages are not duplicated.
        let unseen = messages_after_overlap(&full, new_messages);
        full.extend(unseen);

        // Sliding window by token estimate
        let mut tokens = 0;
        let mut windowed = Vec::new();
        for msg in full.iter().rev() {
            let t = (msg.content.len() as f32 / 4.0) as usize + 8;
            if tokens + t > self.max_tokens && !windowed.is_empty() { break; }
            tokens += t;
            windowed.push(msg.clone());
        }
        windowed.reverse();
        let prompt = windowed.iter().map(|m| format!("{}: {}", m.role, m.content)).collect::<Vec<_>>().join("\n");
        (prompt, windowed)
    }

    pub fn store_turn(&self, conversation_id: String, user_msgs: Vec<ChatMessage>, assistant_msg: ChatMessage, provider: String, tab_id: String) {
        let mut entry = self.conversations.entry(conversation_id.clone()).or_insert_with(|| Conversation {
            id: conversation_id.clone(),
            messages: vec![],
            provider: None,
            tab_id: None,
        });
        // The caller may provide a context window containing messages
        // already stored for this conversation. Append only the unseen
        // suffix (or the non-overlapping tail of a sliding window).
        let unseen = messages_after_overlap(&entry.messages, &user_msgs);
        entry.messages.extend(unseen);
        entry.messages.push(assistant_msg);
        entry.provider = Some(provider);
        entry.tab_id = Some(tab_id);
    }

    /// True when the conversation already has stored turns. Used only
    /// to scope model selection (Go's new-conversation-only rule):
    /// selection runs only when there is NO prior history. This is a
    /// memory lookup, not a tab map — there is deliberately no
    /// per-conversation tab reuse in this restoration (Rust v1: fresh
    /// page per attempt, tab_id unread).
    pub fn has_history(&self, cid: &str) -> bool {
        self.conversations.get(cid).map(|c| !c.messages.is_empty()).unwrap_or(false)
    }

    pub fn get_tab(&self, cid: &str) -> Option<String> {
        self.conversations.get(cid).and_then(|c| c.tab_id.clone())
    }

    pub fn get_provider(&self, cid: &str) -> Option<String> {
        self.conversations.get(cid).and_then(|c| c.provider.clone())
    }
}


/// Remove a transcript prefix that is already represented in the stored
/// conversation. Supports both full-history requests and requests that
/// contain only a recent sliding window. Message equality is role+content.
fn messages_after_overlap(stored: &[ChatMessage], incoming: &[ChatMessage]) -> Vec<ChatMessage> {
    if incoming.is_empty() {
        return Vec::new();
    }
    if stored.is_empty() {
        return incoming.to_vec();
    }

    // Full transcript: find the longest incoming prefix that matches the
    // stored transcript prefix, then append only the remaining tail.
    let common_prefix = stored.iter().zip(incoming.iter())
        .take_while(|(a, b)| a.role == b.role && a.content == b.content)
        .count();
    if common_prefix == stored.len() && incoming.len() >= stored.len() {
        return incoming[common_prefix..].to_vec();
    }

    // Sliding-window transcript: find the largest stored suffix matching
    // the incoming prefix and append only the part after that overlap.
    let max_overlap = stored.len().min(incoming.len());
    for overlap in (1..=max_overlap).rev() {
        let matches = stored[stored.len() - overlap..].iter()
            .zip(incoming[..overlap].iter())
            .all(|(a, b)| a.role == b.role && a.content == b.content);
        if matches {
            return incoming[overlap..].to_vec();
        }
    }

    // No overlap means the request is a new turn or a deliberately
    // separate transcript. Do not discard it.
    incoming.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(role: &str, content: &str) -> ChatMessage {
        ChatMessage { role: role.to_string(), content: content.to_string() }
    }

    #[test]
    fn full_transcript_only_appends_new_suffix() {
        let stored = vec![m("user", "hello"), m("assistant", "hi")];
        let incoming = vec![m("user", "hello"), m("assistant", "hi"), m("user", "next")];
        let result = messages_after_overlap(&stored, &incoming);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "next");
    }

    #[test]
    fn sliding_window_only_appends_new_suffix() {
        let stored = vec![m("user", "one"), m("assistant", "two"), m("user", "three")];
        let incoming = vec![m("assistant", "two"), m("user", "three"), m("user", "four")];
        let result = messages_after_overlap(&stored, &incoming);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "four");
    }

    #[test]
    fn unrelated_new_turn_is_not_discarded() {
        let stored = vec![m("user", "old")];
        let incoming = vec![m("user", "new")];
        let result = messages_after_overlap(&stored, &incoming);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "new");
    }
}
