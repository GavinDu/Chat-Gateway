#[path = "memory/sqlite.rs"]
mod sqlite;

use crate::context::estimate_message_tokens;
use crate::context::messages_after_overlap;
use crate::context::ContextBuilder;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
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

    fn conversation_for_owner(
        &self,
        owner_key_id: &str,
        id: &str,
    ) -> anyhow::Result<Option<Conversation>> {
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let owner: Option<String> = tx
            .query_row(
                "SELECT owner_key_id FROM conversations WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        let conversation = match owner {
            Some(owner) if owner == owner_key_id => sqlite::load(&tx, id)?,
            Some(_) => anyhow::bail!("conversation belongs to another API key"),
            None => None,
        };
        tx.commit()
            .map_err(|error| anyhow::anyhow!("completing scoped conversation read: {error}"))?;
        Ok(conversation)
    }

    #[allow(dead_code)]
    pub fn build_context(
        &self,
        conversation_id: Option<&str>,
        new_messages: &[ChatMessage],
    ) -> anyhow::Result<(String, Vec<ChatMessage>)> {
        self.build_context_for_owner("local", conversation_id, new_messages)
    }

    /// Read the complete durable transcript for a conversation owned by this API key.
    /// Used to verify whether a provider-side thread still matches the committed gateway state.
    pub fn conversation_messages_for_owner(
        &self,
        owner_key_id: &str,
        conversation_id: &str,
    ) -> anyhow::Result<Vec<ChatMessage>> {
        Ok(self
            .conversation_for_owner(owner_key_id, conversation_id)?
            .map(|conversation| conversation.messages)
            .unwrap_or_default())
    }

    pub fn build_context_for_owner(
        &self,
        owner_key_id: &str,
        conversation_id: Option<&str>,
        new_messages: &[ChatMessage],
    ) -> anyhow::Result<(String, Vec<ChatMessage>)> {
        let (prompt, context, _) =
            self.build_context_for_owner_with_history(owner_key_id, conversation_id, new_messages)?;
        Ok((prompt, context))
    }

    /// Run context construction and its SQLite reads on Tokio's blocking pool.
    pub async fn build_context_for_owner_with_transcript_blocking(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: Option<String>,
        new_messages: Vec<ChatMessage>,
    ) -> anyhow::Result<(String, Vec<ChatMessage>, bool, Vec<ChatMessage>)> {
        tokio::task::spawn_blocking(move || {
            self.build_context_for_owner_with_transcript(
                &owner_key_id,
                conversation_id.as_deref(),
                &new_messages,
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("conversation context task failed: {error}"))?
    }

    /// Build context and report persisted transcript presence from the same scoped read.
    pub fn build_context_for_owner_with_history(
        &self,
        owner_key_id: &str,
        conversation_id: Option<&str>,
        new_messages: &[ChatMessage],
    ) -> anyhow::Result<(String, Vec<ChatMessage>, bool)> {
        let (prompt, context, has_history, _history) =
            self.build_context_for_owner_with_transcript(owner_key_id, conversation_id, new_messages)?;
        Ok((prompt, context, has_history))
    }

    /// Return the canonical stored transcript alongside the rendered context so
    /// callers that need both can reuse one SQLite snapshot for the request.
    pub fn build_context_for_owner_with_transcript(
        &self,
        owner_key_id: &str,
        conversation_id: Option<&str>,
        new_messages: &[ChatMessage],
    ) -> anyhow::Result<(String, Vec<ChatMessage>, bool, Vec<ChatMessage>)> {
        let (history, saved_summary) = match conversation_id {
            Some(id) => match self.conversation_for_owner(owner_key_id, id)? {
                Some(conversation) => (conversation.messages, self.rolling_summary(id)?),
                None => (Vec::new(), None),
            },
            None => (Vec::new(), None),
        };
        let has_history = !history.is_empty();
        let summary_message = saved_summary.map(|(summary, _covered)| ChatMessage {
            role: "system".to_owned(),
            content: format!("Conversation summary of earlier turns:\n{summary}"),
        });
        let summary_tokens = summary_message
            .as_ref()
            .map(|message| estimate_message_tokens(&message.content).saturating_add(8))
            .unwrap_or(0);
        let message_budget = if summary_message.is_some() {
            self.max_tokens.saturating_sub(summary_tokens)
        } else {
            self.max_tokens
        };
        let (mut prompt, mut context) =
            ContextBuilder::build(&history, new_messages, message_budget);
        if let Some(summary) = summary_message {
            context.insert(0, summary);
            prompt = crate::context::render_prompt(&context);
        }
        Ok((prompt, context, has_history, history))
    }

    fn rolling_summary(&self, conversation_id: &str) -> anyhow::Result<Option<(String, usize)>> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT summary, covered_through_position FROM conversation_summaries WHERE conversation_id=?1",
                [conversation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?.max(0) as usize,
                    ))
                },
            )
            .optional()
            .map_err(Into::into)
    }

    #[allow(dead_code)]
    pub fn store_turn(
        &self,
        conversation_id: String,
        user_msgs: Vec<ChatMessage>,
        assistant_msg: ChatMessage,
        provider: String,
        tab_id: String,
    ) -> anyhow::Result<()> {
        self.store_turn_for_owner(
            "local",
            conversation_id,
            user_msgs,
            assistant_msg,
            provider,
            tab_id,
        )
    }

    pub fn store_turn_for_owner(
        &self,
        owner_key_id: &str,
        conversation_id: String,
        user_msgs: Vec<ChatMessage>,
        assistant_msg: ChatMessage,
        provider: String,
        tab_id: String,
    ) -> anyhow::Result<()> {
        self.store_turn_for_owner_with_tab_id(
            owner_key_id,
            conversation_id,
            user_msgs,
            assistant_msg,
            provider,
            Some(tab_id),
        )
    }

    /// Persist a completed turn with optional real browser tab metadata.
    pub fn store_turn_for_owner_with_tab_id(
        &self,
        owner_key_id: &str,
        conversation_id: String,
        user_msgs: Vec<ChatMessage>,
        assistant_msg: ChatMessage,
        provider: String,
        tab_id: Option<String>,
    ) -> anyhow::Result<()> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing_owner: Option<String> = tx
            .query_row(
                "SELECT owner_key_id FROM conversations WHERE id = ?1",
                [&conversation_id],
                |row| row.get(0),
            )
            .optional()?;
        if existing_owner
            .as_deref()
            .is_some_and(|existing| existing != owner_key_id)
        {
            anyhow::bail!("conversation belongs to another API key");
        }
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
        let first = conversation.messages.len();
        if !replay {
            conversation.messages.extend(unseen);
            conversation.messages.push(assistant_msg);
        }
        conversation.provider = Some(provider);
        if let Some(tab_id) = tab_id {
            conversation.tab_id = Some(tab_id);
        } else if conversation
            .tab_id
            .as_deref()
            .is_some_and(|tab_id| tab_id.starts_with("tab-"))
        {
            // Remove legacy synthetic identifiers without erasing real tab metadata.
            conversation.tab_id = None;
        }
        sqlite::append_owned(&tx, &conversation, first, owner_key_id)?;
        // Maintain a bounded deterministic summary in the same transaction as the turn.
        const RECENT_MESSAGES_TO_KEEP: usize = 8;
        const SUMMARY_START_THRESHOLD: usize = 16;
        if conversation.messages.len() >= SUMMARY_START_THRESHOLD {
            let covered = conversation.messages.len() - RECENT_MESSAGES_TO_KEEP;
            let summary = extractive_rolling_summary(&conversation.messages[..covered], 1200);
            if !summary.is_empty() {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .min(i64::MAX as u64) as i64;
                tx.execute(
                    "INSERT INTO conversation_summaries(conversation_id,summary,covered_through_position,updated_at)
                     VALUES(?1,?2,?3,?4)
                     ON CONFLICT(conversation_id) DO UPDATE SET
                        summary=excluded.summary,
                        covered_through_position=excluded.covered_through_position,
                        updated_at=excluded.updated_at",
                    rusqlite::params![conversation.id, summary, covered as i64, now],
                )?;
            }
        }
        tx.commit()
            .map_err(|error| anyhow::anyhow!("committing conversation turn: {error}"))
    }

    /// Export one complete conversation without exposing database internals.
    /// Callers must apply authorization before returning this record to clients.
    /// Export one complete conversation for the legacy local tenant.
    #[allow(dead_code)]
    pub fn export_conversation(
        &self,
        conversation_id: &str,
    ) -> anyhow::Result<Option<Conversation>> {
        self.export_conversation_for_owner("local", conversation_id)
    }

    pub fn export_conversation_for_owner(
        &self,
        owner_key_id: &str,
        conversation_id: &str,
    ) -> anyhow::Result<Option<Conversation>> {
        self.conversation_for_owner(owner_key_id, conversation_id)
    }

    /// List conversation metadata for the legacy local tenant.
    #[allow(dead_code)]
    pub fn list_conversations(
        &self,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationSummary>> {
        self.list_conversations_for_owner("local", limit, offset)
    }

    pub fn list_conversations_for_owner(
        &self,
        owner_key_id: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationSummary>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT c.id, c.provider, COUNT(m.position)
             FROM conversations c
             LEFT JOIN messages m ON m.conversation_id = c.id
             WHERE c.owner_key_id = ?1
             GROUP BY c.id, c.provider
             ORDER BY c.id
             LIMIT ?2 OFFSET ?3",
        )?;
        let rows = statement.query_map(
            rusqlite::params![
                owner_key_id,
                limit.min(100) as i64,
                offset.min(i64::MAX as usize) as i64
            ],
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

    /// Flush SQLite WAL pages after HTTP requests have drained.
    pub fn checkpoint(&self) -> anyhow::Result<()> {
        let connection = self.lock()?;
        let (busy, _log_frames, _checkpointed_frames): (i64, i64, i64) =
            connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        if busy != 0 {
            anyhow::bail!(
                "SQLite WAL checkpoint could not complete because another connection is busy"
            );
        }
        connection.execute_batch("PRAGMA optimize;")?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn delete_conversation(&self, conversation_id: &str) -> anyhow::Result<bool> {
        self.delete_conversation_for_owner("local", conversation_id)
    }

    pub fn delete_conversation_for_owner(
        &self,
        owner_key_id: &str,
        conversation_id: &str,
    ) -> anyhow::Result<bool> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = tx.execute(
            "DELETE FROM conversations WHERE id = ?1 AND owner_key_id = ?2",
            rusqlite::params![conversation_id, owner_key_id],
        )?;
        tx.commit()?;
        Ok(deleted != 0)
    }

    /// Delete conversations whose activity timestamp is older than the cutoff.
    pub fn purge_expired_conversations(&self, cutoff_unix_seconds: i64) -> anyhow::Result<usize> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = tx.execute(
            "DELETE FROM conversations WHERE last_activity_at > 0 AND last_activity_at < ?1",
            [cutoff_unix_seconds],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    // Legacy read helpers remain scoped to the local tenant for tests and CLI tools.
    #[allow(dead_code)]
    pub fn has_history(&self, cid: &str) -> anyhow::Result<bool> {
        self.has_history_for_owner("local", cid)
    }

    pub fn has_history_for_owner(&self, owner_key_id: &str, cid: &str) -> anyhow::Result<bool> {
        // This is a routing hint, not a transcript read. Use EXISTS so the
        // query can stop at the first message instead of deserializing history.
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1
                    FROM conversations c
                    JOIN messages m ON m.conversation_id = c.id
                    WHERE c.id = ?1 AND c.owner_key_id = ?2
                )",
                rusqlite::params![cid, owner_key_id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    #[allow(dead_code)]
    pub fn get_tab(&self, cid: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .conversation(cid)?
            .and_then(|conversation| conversation.tab_id))
    }

    #[allow(dead_code)]
    pub fn get_provider(&self, cid: &str) -> anyhow::Result<Option<String>> {
        self.get_provider_for_owner("local", cid)
    }

    pub fn get_provider_for_owner(
        &self,
        owner_key_id: &str,
        cid: &str,
    ) -> anyhow::Result<Option<String>> {
        // Routing needs only conversation metadata. Avoid loading every message
        // just to recover provider affinity; context construction loads the
        // transcript once after the route is selected.
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT provider FROM conversations WHERE id = ?1 AND owner_key_id = ?2",
                rusqlite::params![cid, owner_key_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    #[allow(dead_code)]
    pub fn get_provider_thread(
        &self,
        conversation_id: &str,
        provider: &str,
    ) -> anyhow::Result<Option<ProviderThread>> {
        self.get_provider_thread_for_owner("local", conversation_id, provider)
    }

    pub fn get_provider_thread_for_owner(
        &self,
        owner_key_id: &str,
        conversation_id: &str,
        provider: &str,
    ) -> anyhow::Result<Option<ProviderThread>> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT pt.thread_url, pt.synced_through_message_id, pt.model, pt.updated_at
                 FROM provider_threads pt
                 JOIN conversations c ON c.id = pt.conversation_id
                 WHERE pt.conversation_id = ?1 AND pt.provider = ?2 AND c.owner_key_id = ?3",
                rusqlite::params![conversation_id, provider, owner_key_id],
                |row| {
                    Ok(ProviderThread {
                        conversation_id: conversation_id.to_owned(),
                        provider: provider.to_owned(),
                        thread_url: row.get(0)?,
                        synced_through_message_id: row.get(1)?,
                        model: row.get(2)?,
                        updated_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    #[allow(dead_code)]
    pub fn upsert_provider_thread(&self, thread: &ProviderThread) -> anyhow::Result<()> {
        self.upsert_provider_thread_for_owner("local", thread)
    }

    pub fn upsert_provider_thread_for_owner(
        &self,
        owner_key_id: &str,
        thread: &ProviderThread,
    ) -> anyhow::Result<()> {
        let connection = self.lock()?;
        let owner: Option<String> = connection
            .query_row(
                "SELECT owner_key_id FROM conversations WHERE id = ?1",
                [&thread.conversation_id],
                |row| row.get(0),
            )
            .optional()?;
        if owner.as_deref() != Some(owner_key_id) {
            anyhow::bail!("conversation is not owned by this API key");
        }
        connection.execute(
            "INSERT INTO provider_threads(
                conversation_id, provider, thread_url, synced_through_message_id, model, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(conversation_id, provider) DO UPDATE SET
                thread_url=excluded.thread_url,
                synced_through_message_id=excluded.synced_through_message_id,
                model=excluded.model,
                updated_at=excluded.updated_at",
            rusqlite::params![
                thread.conversation_id,
                thread.provider,
                thread.thread_url,
                thread.synced_through_message_id,
                thread.model,
                thread.updated_at,
            ],
        )?;
        Ok(())
    }
}

fn extractive_rolling_summary(messages: &[ChatMessage], max_chars: usize) -> String {
    let mut lines = Vec::new();
    let mut used = 0usize;
    for message in messages.iter().rev() {
        let normalized = message
            .content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if normalized.is_empty() {
            continue;
        }
        let excerpt = normalized.chars().take(180).collect::<String>();
        let line = format!("{}: {}", message.role, excerpt);
        let cost = line.chars().count() + 1;
        if used + cost > max_chars && !lines.is_empty() {
            break;
        }
        used += cost;
        lines.push(line);
    }
    lines.reverse();
    lines.join("\n")
}

#[cfg(test)]
#[path = "memory/tests.rs"]
mod tests;

/// Persisted provider-side thread identity and the last synchronized gateway message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderThread {
    pub conversation_id: String,
    pub provider: String,
    pub thread_url: String,
    pub synced_through_message_id: Option<String>,
    pub model: Option<String>,
    pub updated_at: i64,
}

/// Metadata returned by the paged conversation listing endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationSummary {
    pub id: String,
    pub provider: Option<String>,
    pub message_count: usize,
}
