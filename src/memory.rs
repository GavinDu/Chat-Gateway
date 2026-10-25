use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::{Path, PathBuf}, sync::Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    persistence_path: Option<PathBuf>,
    persistence_lock: Mutex<()>,
}

impl UnifiedMemory {
    pub fn new(max_tokens: usize) -> Self {
        Self::with_optional_persistence(max_tokens, None)
            .expect("in-memory conversation store initialization cannot fail")
    }

    /// Restore durable conversation memory from CHAT_GATEWAY_MEMORY_PATH,
    /// defaulting to data/conversations.json. Snapshot writes use an atomic
    /// rename so a process interruption cannot leave a partially written file.
    pub fn from_env(max_tokens: usize) -> anyhow::Result<Self> {
        let path = std::env::var_os("CHAT_GATEWAY_MEMORY_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("data/conversations.json"));
        Self::with_optional_persistence(max_tokens, Some(path))
    }

    fn with_optional_persistence(max_tokens: usize, persistence_path: Option<PathBuf>) -> anyhow::Result<Self> {
        let conversations = DashMap::new();
        if let Some(path) = persistence_path.as_deref() {
            // Fail closed: never start with an empty store after a restore
            // error, since a subsequent write could overwrite recoverable data.
            let loaded = load_conversations(path)
                .map_err(|error| anyhow::anyhow!("failed to restore conversation memory from {}: {error}", path.display()))?;
            for (id, conversation) in loaded {
                if id != conversation.id {
                    anyhow::bail!("conversation memory key/id mismatch for key '{id}' in {}", path.display());
                }
                conversations.insert(id, conversation);
            }
        }
        Ok(Self {
            max_tokens,
            conversations,
            persistence_path,
            persistence_lock: Mutex::new(()),
        })
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

    pub fn store_turn(&self, conversation_id: String, user_msgs: Vec<ChatMessage>, assistant_msg: ChatMessage, provider: String, tab_id: String) -> anyhow::Result<()> {
        let _guard = self.persistence_lock.lock()
            .map_err(|error| anyhow::anyhow!("conversation persistence lock poisoned: {error}"))?;

        // Keep a rollback copy so a failed durable write cannot leave an
        // apparently committed turn in memory while disk has the old state.
        let previous = self.conversations.get(&conversation_id).map(|entry| entry.value().clone());
        {
            let mut entry = self.conversations.entry(conversation_id.clone()).or_insert_with(|| Conversation {
                id: conversation_id.clone(),
                messages: vec![],
                provider: None,
                tab_id: None,
            });
            let unseen = messages_after_overlap(&entry.messages, &user_msgs);
            entry.messages.extend(unseen);
            entry.messages.push(assistant_msg);
            entry.provider = Some(provider);
            entry.tab_id = Some(tab_id);
        }

        if let Err(error) = self.persist_snapshot() {
            match previous {
                Some(conversation) => { self.conversations.insert(conversation_id, conversation); }
                None => { self.conversations.remove(&conversation_id); }
            }
            return Err(error.context("persisting conversation turn"));
        }
        Ok(())
    }

    fn persist_snapshot(&self) -> anyhow::Result<()> {
        let Some(path) = self.persistence_path.as_deref() else {
            return Ok(());
        };
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let snapshot: HashMap<String, Conversation> = self.conversations
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();
        let bytes = serde_json::to_vec(&snapshot)?;
        // Conversation snapshots contain private prompts and responses. Use a
        // unique temp file and owner-only permissions on Unix, then flush file
        // contents before atomically replacing the durable snapshot.
        let temp_path = path.with_extension(format!("tmp-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&temp_path)?;
        if let Err(error) = std::io::Write::write_all(&mut output, &bytes)
            .and_then(|()| output.sync_all())
        {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error.into());
        }
        drop(output);
        if let Err(error) = std::fs::rename(&temp_path, path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error.into());
        }
        // Best-effort directory sync makes the rename durable across a crash
        // on filesystems that support syncing directory entries.
        #[cfg(unix)]
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    }

    /// Delete one conversation and persist the deletion before reporting success.
    /// If the snapshot write fails, restore the removed entry in memory.
    pub fn delete_conversation(&self, conversation_id: &str) -> anyhow::Result<bool> {
        let _guard = self.persistence_lock.lock()
            .map_err(|error| anyhow::anyhow!("conversation persistence lock poisoned: {error}"))?;
        let Some((_, previous)) = self.conversations.remove(conversation_id) else {
            return Ok(false);
        };
        if let Err(error) = self.persist_snapshot() {
            self.conversations.insert(conversation_id.to_string(), previous);
            return Err(error.context("persisting conversation deletion"));
        }
        Ok(true)
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


fn load_conversations(path: &Path) -> anyhow::Result<HashMap<String, Conversation>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let bytes = std::fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
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
    fn failed_persistence_rolls_back_memory_mutation() {
        // A directory at the target path forces atomic rename to fail.
        let path = std::env::temp_dir().join(format!("chat-gateway-memory-target-{}", uuid::Uuid::new_v4()));
        let memory = UnifiedMemory::with_optional_persistence(8000, Some(path.clone())).unwrap();
        // Turn the intended snapshot file into a directory after startup so
        // restore succeeds but the subsequent atomic rename must fail.
        std::fs::create_dir_all(&path).unwrap();
        let result = memory.store_turn(
            "conversation-failed".to_string(),
            vec![m("user", "hello")],
            m("assistant", "hi"),
            "chatgpt".to_string(),
            "tab-1".to_string(),
        );
        assert!(result.is_err());
        assert!(!memory.has_history("conversation-failed"));
        let _ = std::fs::remove_dir_all(path);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_file_is_created_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("chat-gateway-memory-private-{}.json", uuid::Uuid::new_v4()));
        let memory = UnifiedMemory::with_optional_persistence(8000, Some(path.clone())).unwrap();
        memory.store_turn(
            "conversation-private".to_string(),
            vec![m("user", "private prompt")],
            m("assistant", "private answer"),
            "chatgpt".to_string(),
            "tab-private".to_string(),
        ).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn persistent_memory_restores_conversation_after_restart() {
        let path = std::env::temp_dir().join(format!("chat-gateway-memory-{}.json", uuid::Uuid::new_v4()));
        {
            let memory = UnifiedMemory::with_optional_persistence(8000, Some(path.clone())).unwrap();
            memory.store_turn(
                "conversation-1".to_string(),
                vec![m("user", "hello")],
                m("assistant", "hi"),
                "chatgpt".to_string(),
                "tab-1".to_string(),
            );
            assert!(path.exists());
        }
        {
            let memory = UnifiedMemory::with_optional_persistence(8000, Some(path.clone())).unwrap();
            assert!(memory.has_history("conversation-1"));
            assert_eq!(memory.get_provider("conversation-1").as_deref(), Some("chatgpt"));
            let (prompt, _) = memory.build_context(Some("conversation-1"), &[m("user", "next")]);
            assert!(prompt.contains("assistant: hi"));
            assert!(prompt.contains("user: next"));
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn deleting_conversation_persists_across_restart() {
        let path = std::env::temp_dir().join(format!("chat-gateway-memory-delete-{}.json", uuid::Uuid::new_v4()));
        {
            let memory = UnifiedMemory::with_optional_persistence(8000, Some(path.clone())).unwrap();
            memory.store_turn(
                "conversation-delete".to_string(),
                vec![m("user", "private prompt")],
                m("assistant", "private answer"),
                "chatgpt".to_string(),
                "tab-delete".to_string(),
            ).unwrap();
            assert!(memory.delete_conversation("conversation-delete").unwrap());
            assert!(!memory.has_history("conversation-delete"));
            assert!(!memory.delete_conversation("conversation-delete").unwrap());
        }
        let restored = UnifiedMemory::with_optional_persistence(8000, Some(path.clone())).unwrap();
        assert!(!restored.has_history("conversation-delete"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn corrupt_snapshot_fails_restore_instead_of_starting_empty() {
        let path = std::env::temp_dir().join(format!("chat-gateway-memory-corrupt-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"{ definitely not json").unwrap();
        let result = UnifiedMemory::with_optional_persistence(8000, Some(path.clone()));
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{ definitely not json");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_snapshot_with_mismatched_conversation_identity() {
        let path = std::env::temp_dir().join(format!("chat-gateway-memory-mismatch-{}.json", uuid::Uuid::new_v4()));
        let data = serde_json::json!({
            "key-1": { "id": "different-id", "messages": [], "provider": null, "tab_id": null }
        });
        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        assert!(UnifiedMemory::with_optional_persistence(8000, Some(path.clone())).is_err());
        let _ = std::fs::remove_file(path);
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
