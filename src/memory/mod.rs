use crate::config::MemoryConfig;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
    pub tab_id: Option<String>, // Obscura/Chrome targetId reused via X-Conversation-Id
}

pub struct UnifiedMemory {
    cfg: MemoryConfig,
    conversations: DashMap<String, Conversation>,
}

impl UnifiedMemory {
    pub fn new(cfg: MemoryConfig) -> Self {
        Self { cfg, conversations: DashMap::new() }
    }

    /// Build prompt with sliding window, unified across providers
    pub fn build_context(&self, conversation_id: Option<&str>, new_messages: &[ChatMessage]) -> (String, Vec<ChatMessage>) {
        let mut full = vec![];
        if let Some(cid) = conversation_id {
            if let Some(conv) = self.conversations.get(cid) {
                full.extend(conv.messages.clone());
            }
        }
        full.extend(new_messages.iter().cloned());

        // Sliding window by token estimate
        let max_tokens = self.cfg.max_tokens;
        let mut tokens = 0;
        let mut windowed = Vec::new();
        for msg in full.iter().rev() {
            let t = (msg.content.len() as f32 / 4.0) as usize + 8;
            if tokens + t > max_tokens && !windowed.is_empty() { break; }
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
        // Optional: summarize if exceeds 2x max
    }

    pub fn get_tab(&self, cid: &str) -> Option<String> {
        self.conversations.get(cid).and_then(|c| c.tab_id.clone())
    }
}
