#[path = "memory/sqlite.rs"]
mod sqlite;

use rusqlite::{Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Mutex, MutexGuard},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Conversation {
    pub id: String,
    pub messages: Vec<ChatMessage>,
    pub provider: Option<String>,
    pub tab_id: Option<String>,
}

/// SQLite is authoritative: there is no mutable conversation cache to diverge
/// from disk. The mutex protects this connection; IMMEDIATE transactions protect
/// read/reconcile/write across independent connections and gateway processes.
pub struct UnifiedMemory {
    max_tokens: usize,
    connection: Mutex<Connection>,
}

impl UnifiedMemory {
    // Test-only helper is shared across test targets; some targets do not call it.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn open_path_for_test(
        path: &std::path::Path,
        legacy: Option<&std::path::Path>,
        max_tokens: usize,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            max_tokens,
            connection: Mutex::new(sqlite::open(Some(path), legacy)?),
        })
    }

    // Convenience constructor used by isolated unit tests.
    #[allow(dead_code)]
    pub fn new(max_tokens: usize) -> Self {
        Self {
            max_tokens,
            connection: Mutex::new(
                sqlite::open(None, None).expect("initializing in-memory conversation database"),
            ),
        }
    }

    /// Existing JSON path overrides keep working: `custom.json` imports into
    /// `custom.sqlite3`. Other extensions designate a database directly.
    pub fn from_env(max_tokens: usize) -> anyhow::Result<Self> {
        let configured = std::env::var_os("CHAT_GATEWAY_MEMORY_PATH").map(PathBuf::from);
        let (database, legacy) = match configured {
            Some(path) if path.extension().is_some_and(|ext| ext == "json") => {
                (path.with_extension("sqlite3"), path)
            }
            Some(path) => (path, PathBuf::from("data/conversations.json")),
            None => (
                PathBuf::from("data/conversations.sqlite3"),
                PathBuf::from("data/conversations.json"),
            ),
        };
        Ok(Self {
            max_tokens,
            connection: Mutex::new(sqlite::open(Some(&database), Some(&legacy))?),
        })
    }

    // Test-only helper is used by corruption-path tests in separate targets.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn drop_conversations_table_for_test(&self) -> anyhow::Result<()> {
        self.lock()?.execute_batch("DROP TABLE conversations")?;
        Ok(())
    }

    fn lock(&self) -> anyhow::Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|error| anyhow::anyhow!("conversation database lock poisoned: {error}"))
    }

    /// Read a conversation without converting database errors into missing history.
    fn conversation(&self, id: &str) -> anyhow::Result<Option<Conversation>> {
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let conversation = sqlite::load(&tx, id)?;
        tx.commit()
            .map_err(|error| anyhow::anyhow!("completing conversation read: {error}"))?;
        Ok(conversation)
    }

    pub fn build_context(
        &self,
        conversation_id: Option<&str>,
        new_messages: &[ChatMessage],
    ) -> anyhow::Result<(String, Vec<ChatMessage>)> {
        let mut full = match conversation_id {
            Some(id) => self
                .conversation(id)?
                .map(|conversation| conversation.messages)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        full.extend(messages_after_overlap(&full, new_messages));
        let mut tokens = 0;
        let mut windowed = Vec::new();
        for msg in full.iter().rev() {
            let t = (msg.content.len() as f32 / 4.0) as usize + 8;
            if tokens + t > self.max_tokens && !windowed.is_empty() {
                break;
            }
            tokens += t;
            windowed.push(msg.clone());
        }
        windowed.reverse();
        let prompt = windowed
            .iter()
            .map(|message| format!("{}: {}", message.role, message.content))
            .collect::<Vec<_>>()
            .join("\n");
        Ok((prompt, windowed))
    }

    pub fn store_turn(
        &self,
        conversation_id: String,
        user_msgs: Vec<ChatMessage>,
        assistant_msg: ChatMessage,
        provider: String,
        tab_id: String,
    ) -> anyhow::Result<()> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut conversation =
            sqlite::load(&tx, &conversation_id)?.unwrap_or_else(|| Conversation {
                id: conversation_id,
                messages: Vec::new(),
                provider: None,
                tab_id: None,
            });
        let completed: Vec<_> = user_msgs
            .iter()
            .cloned()
            .chain(std::iter::once(assistant_msg.clone()))
            .collect();
        let replay = !user_msgs.is_empty()
            && (conversation.messages.starts_with(&completed)
                || conversation.messages.ends_with(&completed));
        let unseen = messages_after_overlap(&conversation.messages, &user_msgs);
        // An identical completed turn replay must not append a second answer.
        let first = conversation.messages.len();
        if !replay {
            conversation.messages.extend(unseen);
            conversation.messages.push(assistant_msg);
        }
        conversation.provider = Some(provider);
        conversation.tab_id = Some(tab_id);
        sqlite::append(&tx, &conversation, first)?;
        tx.commit()
            .map_err(|error| anyhow::anyhow!("committing conversation turn: {error}"))
    }

    /// Export one complete conversation without exposing database internals.
    /// Callers must apply authorization before returning this record to clients.
    pub fn export_conversation(
        &self,
        conversation_id: &str,
    ) -> anyhow::Result<Option<Conversation>> {
        self.conversation(conversation_id)
    }

    /// List conversation metadata without loading message bodies.
    pub fn list_conversations(
        &self,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationSummary>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT c.id, c.provider, COUNT(m.position)
             FROM conversations c
             LEFT JOIN messages m ON m.conversation_id = c.id
             GROUP BY c.id, c.provider
             ORDER BY c.id
             LIMIT ?1 OFFSET ?2
        )?;
        let rows = statement.query_map(
            rusqlite::params![limit.min(100) as i64, offset.min(i64::MAX as usize) as i64],
            |row| {
                Ok(ConversationSummary {
                    id: row.get(0)?,
                    provider: row.get(1)?,
                    message_count: row.get::<_, i64>(2)?.max(0) as usize,
                })
            },
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn delete_conversation(&self, conversation_id: &str) -> anyhow::Result<bool> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = tx.execute("DELETE FROM conversations WHERE id = ?1", [conversation_id])?;
        tx.commit()?;
        Ok(deleted != 0)
    }

    // Legacy read helpers remain used by the benchmark and routing tests.
    #[allow(dead_code)]
    pub fn has_history(&self, cid: &str) -> anyhow::Result<bool> {
        Ok(self
            .conversation(cid)?
            .is_some_and(|conversation| !conversation.messages.is_empty()))
    }

    #[allow(dead_code)]
    pub fn get_tab(&self, cid: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .conversation(cid)?
            .and_then(|conversation| conversation.tab_id))
    }

    pub fn get_provider(&self, cid: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .conversation(cid)?
            .and_then(|conversation| conversation.provider))
    }
}

fn messages_after_overlap(stored: &[ChatMessage], incoming: &[ChatMessage]) -> Vec<ChatMessage> {
    if incoming.is_empty() {
        return Vec::new();
    }
    let common_prefix = stored
        .iter()
        .zip(incoming)
        .take_while(|(a, b)| a == b)
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
#[path = "memory/tests.rs"]
mod tests;

/// Metadata returned by the paged conversation listing endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationSummary {
    pub id: String,
    pub provider: Option<String>,
    pub message_count: usize,
}
