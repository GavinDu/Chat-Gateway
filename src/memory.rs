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
        full.extend(new_messages.iter().cloned());

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
        entry.messages.extend(user_msgs);
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
