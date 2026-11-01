#[path = "memory/sqlite.rs"]
mod sqlite;

use crate::context::estimate_message_tokens;
use crate::context::messages_after_overlap;
use crate::context::ContextBuilder;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// A user-controlled, cross-conversation memory item. Items are never
/// retrieved for prompts unless the owner has explicitly enabled memory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LongTermMemoryItem {
    pub id: String,
    pub memory_key: String,
    pub value: String,
    pub source_conversation_id: Option<String>,
    pub confidence: f64,
    pub created_at: i64,
    pub updated_at: i64,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Conversation {
    pub id: String,
    pub messages: Vec<ChatMessage>,
    pub provider: Option<String>,
    #[serde(skip_serializing)]
    pub tab_id: Option<String>,
}

/// Stable fingerprint for the canonical, ordered transcript.
pub fn transcript_marker(messages: &[ChatMessage]) -> String {
    let encoded = serde_json::to_vec(messages).unwrap_or_default();
    format!("{:x}", Sha256::digest(encoded))
}

/// SQLite is authoritative: there is no mutable conversation cache to diverge
/// from disk. The mutex protects this connection; IMMEDIATE transactions protect
/// read/reconcile/write across independent connections and gateway processes.
pub struct UnifiedMemory {
    max_tokens: usize,
    connection: Mutex<Connection>,
}

struct TurnWrite<'a> {
    owner_key_id: &'a str,
    conversation_id: String,
    user_msgs: Vec<ChatMessage>,
    assistant_msg: ChatMessage,
    provider: String,
    tab_id: Option<String>,
    compute_marker: bool,
}

/// Async contract for bounded conversation history access and atomic turn persistence.
/// Implementations keep synchronous SQLite work off Tokio request workers.
#[async_trait::async_trait]
pub trait ConversationStore: Send + Sync {
    /// Read the canonical transcript and rolling summary in one SQLite snapshot,
    /// then build provider context on Tokio's blocking pool.
    async fn build_context_with_transcript(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: Option<String>,
        new_messages: Vec<ChatMessage>,
        token_budget: usize,
    ) -> anyhow::Result<(String, Vec<ChatMessage>, bool, Vec<ChatMessage>)>;

    async fn persist_turn(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: String,
        user_messages: Vec<ChatMessage>,
        assistant_message: ChatMessage,
        provider: String,
        tab_id: Option<String>,
    ) -> anyhow::Result<String>;

    async fn list_conversations_async(
        self: Arc<Self>,
        owner_key_id: String,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationSummary>>;

    async fn export_conversation_async(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: String,
    ) -> anyhow::Result<Option<Conversation>>;

    async fn delete_conversation_async(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: String,
    ) -> anyhow::Result<bool>;

    async fn get_provider_thread_async(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: String,
        provider: String,
    ) -> anyhow::Result<Option<ProviderThread>>;

    async fn upsert_provider_thread_async(
        self: Arc<Self>,
        owner_key_id: String,
        thread: ProviderThread,
    ) -> anyhow::Result<()>;
}

#[async_trait::async_trait]
impl ConversationStore for UnifiedMemory {
    async fn build_context_with_transcript(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: Option<String>,
        new_messages: Vec<ChatMessage>,
        token_budget: usize,
    ) -> anyhow::Result<(String, Vec<ChatMessage>, bool, Vec<ChatMessage>)> {
        self.build_context_for_owner_with_transcript_budget_blocking(
            owner_key_id,
            conversation_id,
            new_messages,
            token_budget,
        )
        .await
    }

    async fn persist_turn(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: String,
        user_messages: Vec<ChatMessage>,
        assistant_message: ChatMessage,
        provider: String,
        tab_id: Option<String>,
    ) -> anyhow::Result<String> {
        self.store_turn_for_owner_with_tab_id_marker_blocking(
            owner_key_id,
            conversation_id,
            user_messages,
            assistant_message,
            provider,
            tab_id,
        )
        .await
    }

    async fn list_conversations_async(
        self: Arc<Self>,
        owner_key_id: String,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationSummary>> {
        tokio::task::spawn_blocking(move || {
            self.list_conversations_for_owner(&owner_key_id, limit, offset)
        })
        .await
        .map_err(|error| anyhow::anyhow!("conversation list task failed: {error}"))?
    }

    async fn export_conversation_async(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: String,
    ) -> anyhow::Result<Option<Conversation>> {
        tokio::task::spawn_blocking(move || {
            self.export_conversation_for_owner(&owner_key_id, &conversation_id)
        })
        .await
        .map_err(|error| anyhow::anyhow!("conversation export task failed: {error}"))?
    }

    async fn delete_conversation_async(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: String,
    ) -> anyhow::Result<bool> {
        tokio::task::spawn_blocking(move || {
            self.delete_conversation_for_owner(&owner_key_id, &conversation_id)
        })
        .await
        .map_err(|error| anyhow::anyhow!("conversation deletion task failed: {error}"))?
    }

    async fn get_provider_thread_async(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: String,
        provider: String,
    ) -> anyhow::Result<Option<ProviderThread>> {
        tokio::task::spawn_blocking(move || {
            self.get_provider_thread_for_owner(&owner_key_id, &conversation_id, &provider)
        })
        .await
        .map_err(|error| anyhow::anyhow!("provider thread lookup task failed: {error}"))?
    }

    async fn upsert_provider_thread_async(
        self: Arc<Self>,
        owner_key_id: String,
        thread: ProviderThread,
    ) -> anyhow::Result<()> {
        tokio::task::spawn_blocking(move || {
            self.upsert_provider_thread_for_owner(&owner_key_id, &thread)
        })
        .await
        .map_err(|error| anyhow::anyhow!("provider thread persistence task failed: {error}"))?
    }
}

fn is_sqlite_lock_contention(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
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
    /// Read a transcript on Tokio's blocking pool to avoid blocking an async worker.
    #[allow(dead_code)]
    #[allow(dead_code)] // Compatibility API retained for tests and downstream adapters.
    pub async fn conversation_messages_for_owner_blocking(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: String,
    ) -> anyhow::Result<Vec<ChatMessage>> {
        tokio::task::spawn_blocking(move || {
            self.conversation_messages_for_owner(&owner_key_id, &conversation_id)
        })
        .await
        .map_err(|error| anyhow::anyhow!("conversation transcript task failed: {error}"))?
    }

    /// Read a bounded recent history window without loading the full transcript.
    /// The owner check and row selection share a transaction.
    #[allow(dead_code)] // Used by ConversationStore when bounded-history call sites migrate.
    pub fn conversation_history_for_owner_with_limit(
        &self,
        owner_key_id: &str,
        conversation_id: &str,
        max_messages: usize,
    ) -> anyhow::Result<Vec<ChatMessage>> {
        if max_messages == 0 {
            return Ok(Vec::new());
        }
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        let owner: Option<String> = tx
            .query_row(
                "SELECT owner_key_id FROM conversations WHERE id = ?1",
                [conversation_id],
                |row| row.get(0),
            )
            .optional()?;
        match owner {
            Some(owner) if owner == owner_key_id => {}
            Some(_) => anyhow::bail!("conversation belongs to another API key"),
            None => return Ok(Vec::new()),
        }
        let limit = max_messages.min(i64::MAX as usize) as i64;
        let mut statement = tx.prepare(
            "SELECT role, content FROM messages WHERE conversation_id = ?1 ORDER BY position DESC LIMIT ?2",
        )?;
        let mut messages = statement
            .query_map(rusqlite::params![conversation_id, limit], |row| {
                Ok(ChatMessage {
                    role: row.get(0)?,
                    content: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        messages.reverse();
        drop(statement);
        tx.commit()?;
        Ok(messages)
    }

    #[allow(dead_code)]
    #[allow(dead_code)] // Compatibility API retained for tests and downstream adapters.
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

    /// Build context and report persisted transcript presence from the same scoped read.
    /// Build context and report persisted transcript presence from the same scoped read.
    pub fn build_context_for_owner_with_history(
        &self,
        owner_key_id: &str,
        conversation_id: Option<&str>,
        new_messages: &[ChatMessage],
    ) -> anyhow::Result<(String, Vec<ChatMessage>, bool)> {
        let (prompt, context, has_history, _history) = self
            .build_context_for_owner_with_transcript(owner_key_id, conversation_id, new_messages)?;
        Ok((prompt, context, has_history))
    }

    /// Build the rendered prompt and return the canonical stored transcript from
    /// the same SQLite snapshot, so provider retries do not reload all messages.
    pub fn build_context_for_owner_with_transcript(
        &self,
        owner_key_id: &str,
        conversation_id: Option<&str>,
        new_messages: &[ChatMessage],
    ) -> anyhow::Result<(String, Vec<ChatMessage>, bool, Vec<ChatMessage>)> {
        self.build_context_for_owner_with_transcript_budget(
            owner_key_id,
            conversation_id,
            new_messages,
            self.max_tokens,
        )
    }

    pub fn build_context_for_owner_with_transcript_budget(
        &self,
        owner_key_id: &str,
        conversation_id: Option<&str>,
        new_messages: &[ChatMessage],
        token_budget: usize,
    ) -> anyhow::Result<(String, Vec<ChatMessage>, bool, Vec<ChatMessage>)> {
        let (history, saved_summary) = match conversation_id {
            Some(id) => {
                // Read ownership, transcript, and summary in one SQLite snapshot.
                // Separate reads could otherwise combine history from one commit
                // with a summary written by a concurrent request.
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
                let summary = if conversation.is_some() {
                    tx.query_row(
                        "SELECT summary, covered_through_position
                         FROM conversation_summaries WHERE conversation_id = ?1",
                        [id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?.max(0) as usize,
                            ))
                        },
                    )
                    .optional()?
                } else {
                    None
                };
                let history = conversation
                    .map(|conversation| conversation.messages)
                    .unwrap_or_default();
                tx.commit().map_err(|error| {
                    anyhow::anyhow!("completing context snapshot read: {error}")
                })?;
                (history, summary)
            }
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
            token_budget.saturating_sub(summary_tokens)
        } else {
            token_budget
        };
        let (mut prompt, mut context) =
            ContextBuilder::build(&history, new_messages, message_budget);
        if let Some(summary) = summary_message {
            context.insert(0, summary);
            prompt = crate::context::render_prompt(&context);
        }
        Ok((prompt, context, has_history, history))
    }

    pub async fn build_context_for_owner_with_transcript_budget_blocking(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: Option<String>,
        new_messages: Vec<ChatMessage>,
        token_budget: usize,
    ) -> anyhow::Result<(String, Vec<ChatMessage>, bool, Vec<ChatMessage>)> {
        tokio::task::spawn_blocking(move || {
            self.build_context_for_owner_with_transcript_budget(
                &owner_key_id,
                conversation_id.as_deref(),
                &new_messages,
                token_budget,
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("conversation context task failed: {error}"))?
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

    /// Persist a completed turn on Tokio's blocking pool so SQLite mutex waits,
    /// transactions, and disk I/O never block an async runtime worker.
    #[allow(dead_code)]
    #[allow(dead_code)] // Compatibility wrapper for callers that do not need the committed marker.
    pub async fn store_turn_for_owner_with_tab_id_blocking(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: String,
        user_msgs: Vec<ChatMessage>,
        assistant_msg: ChatMessage,
        provider: String,
        tab_id: Option<String>,
    ) -> anyhow::Result<()> {
        self.store_turn_for_owner_with_tab_id_marker_blocking(
            owner_key_id,
            conversation_id,
            user_msgs,
            assistant_msg,
            provider,
            tab_id,
        )
        .await
        .map(|_| ())
    }

    pub async fn store_turn_for_owner_with_tab_id_marker_blocking(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: String,
        user_msgs: Vec<ChatMessage>,
        assistant_msg: ChatMessage,
        provider: String,
        tab_id: Option<String>,
    ) -> anyhow::Result<String> {
        tokio::task::spawn_blocking(move || {
            self.store_turn_for_owner_with_tab_id_and_marker(
                &owner_key_id,
                conversation_id,
                user_msgs,
                assistant_msg,
                provider,
                tab_id,
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("conversation persistence task failed: {error}"))?
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
        self.store_turn_for_owner_with_tab_id_internal(TurnWrite {
            owner_key_id,
            conversation_id,
            user_msgs,
            assistant_msg,
            provider,
            tab_id,
            compute_marker: false,
        })
        .map(|_| ())
    }

    pub fn store_turn_for_owner_with_tab_id_and_marker(
        &self,
        owner_key_id: &str,
        conversation_id: String,
        user_msgs: Vec<ChatMessage>,
        assistant_msg: ChatMessage,
        provider: String,
        tab_id: Option<String>,
    ) -> anyhow::Result<String> {
        self.store_turn_for_owner_with_tab_id_internal(TurnWrite {
            owner_key_id,
            conversation_id,
            user_msgs,
            assistant_msg,
            provider,
            tab_id,
            compute_marker: true,
        })?
        .ok_or_else(|| anyhow::anyhow!("transcript marker was not computed"))
    }

    fn store_turn_for_owner_with_tab_id_internal(
        &self,
        write: TurnWrite<'_>,
    ) -> anyhow::Result<Option<String>> {
        let TurnWrite {
            owner_key_id,
            conversation_id,
            user_msgs,
            assistant_msg,
            provider,
            tab_id,
            compute_marker,
        } = write;
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
        let marker = compute_marker.then(|| transcript_marker(&conversation.messages));
        tx.commit()
            .map_err(|error| anyhow::anyhow!("committing conversation turn: {error}"))?;
        Ok(marker)
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

    /// Export a conversation without blocking a Tokio runtime worker.
    #[allow(dead_code)] // Retained compatibility wrapper while async-store callers migrate.
    pub async fn export_conversation_for_owner_blocking(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: String,
    ) -> anyhow::Result<Option<Conversation>> {
        tokio::task::spawn_blocking(move || {
            self.export_conversation_for_owner(&owner_key_id, &conversation_id)
        })
        .await
        .map_err(|error| anyhow::anyhow!("conversation export task failed: {error}"))?
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

    /// List conversation metadata on Tokio's blocking pool. SQLite lock waits and
    /// row decoding must not occupy an async runtime worker.
    #[allow(dead_code)] // Retained compatibility wrapper while async-store callers migrate.
    pub async fn list_conversations_for_owner_blocking(
        self: Arc<Self>,
        owner_key_id: String,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationSummary>> {
        tokio::task::spawn_blocking(move || {
            self.list_conversations_for_owner(&owner_key_id, limit, offset)
        })
        .await
        .map_err(|error| anyhow::anyhow!("conversation list task failed: {error}"))?
    }

    /// Flush SQLite WAL pages on Tokio's blocking pool after HTTP requests drain.
    pub async fn checkpoint_blocking(self: Arc<Self>) -> anyhow::Result<()> {
        tokio::task::spawn_blocking(move || self.checkpoint())
            .await
            .map_err(|error| anyhow::anyhow!("SQLite checkpoint task failed: {error}"))?
    }

    /// Flush SQLite WAL pages after HTTP requests have drained.
    ///
    /// Checkpointing is maintenance, not a commit boundary: committed WAL
    /// transactions are already durable. A transient SQLITE_BUSY/SQLITE_LOCKED
    /// during shutdown must therefore defer compaction rather than turn an
    /// otherwise clean process exit into a failure.
    pub fn checkpoint(&self) -> anyhow::Result<()> {
        let connection = self.lock()?;
        let mut checkpointed = false;
        let mut last_contention = None;

        for attempt in 0..5 {
            match connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            }) {
                Ok((0, _, _)) => {
                    checkpointed = true;
                    break;
                }
                Ok((busy, _, _)) => {
                    last_contention =
                        Some(format!("checkpoint reported {busy} busy connection(s)"));
                }
                Err(error) if is_sqlite_lock_contention(&error) => {
                    last_contention = Some(error.to_string());
                }
                Err(error) => {
                    return Err(anyhow::anyhow!("SQLite WAL checkpoint failed: {error}"));
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(100 * (attempt + 1)));
        }

        if !checkpointed {
            tracing::warn!(
                reason = last_contention.as_deref().unwrap_or("checkpoint remained busy"),
                "deferring SQLite WAL checkpoint during shutdown; committed data remains in the WAL"
            );
            return Ok(());
        }

        if let Err(error) = connection.execute_batch("PRAGMA optimize;") {
            if is_sqlite_lock_contention(&error) {
                tracing::warn!(%error, "deferring SQLite optimize during shutdown");
                return Ok(());
            }
            return Err(anyhow::anyhow!(
                "SQLite optimize during shutdown failed: {error}"
            ));
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn delete_conversation(&self, conversation_id: &str) -> anyhow::Result<bool> {
        self.delete_conversation_for_owner("local", conversation_id)
    }

    /// Delete a conversation without blocking a Tokio runtime worker.
    #[allow(dead_code)] // Retained compatibility wrapper while async-store callers migrate.
    pub async fn delete_conversation_for_owner_blocking(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: String,
    ) -> anyhow::Result<bool> {
        tokio::task::spawn_blocking(move || {
            self.delete_conversation_for_owner(&owner_key_id, &conversation_id)
        })
        .await
        .map_err(|error| anyhow::anyhow!("conversation deletion task failed: {error}"))?
    }

    pub fn delete_conversation_for_owner(
        &self,
        owner_key_id: &str,
        conversation_id: &str,
    ) -> anyhow::Result<bool> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Long-term memory is independently user-owned and survives source
        // conversation deletion. Clear only its provenance pointer atomically,
        // so exports never advertise a source conversation that no longer exists.
        tx.execute(
            "UPDATE memory_items SET source_conversation_id = NULL
             WHERE source_conversation_id = ?1 AND owner_key_id = ?2",
            rusqlite::params![conversation_id, owner_key_id],
        )?;
        let deleted = tx.execute(
            "DELETE FROM conversations WHERE id = ?1 AND owner_key_id = ?2",
            rusqlite::params![conversation_id, owner_key_id],
        )?;
        tx.commit()?;
        Ok(deleted != 0)
    }

    pub fn purge_expired_conversations(&self, cutoff_unix_seconds: i64) -> anyhow::Result<usize> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Retention removes transcript provenance, not independently managed
        // memory items. Null the source reference in the same transaction.
        tx.execute(
            "UPDATE memory_items SET source_conversation_id = NULL
             WHERE source_conversation_id IN (
                 SELECT id FROM conversations
                 WHERE last_activity_at > 0 AND last_activity_at < ?1
             )",
            [cutoff_unix_seconds],
        )?;
        let deleted = tx.execute(
            "DELETE FROM conversations WHERE last_activity_at > 0 AND last_activity_at < ?1",
            [cutoff_unix_seconds],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    /// Purge expired conversations on Tokio's blocking pool.
    pub async fn purge_expired_conversations_blocking(
        self: Arc<Self>,
        cutoff_unix_seconds: i64,
    ) -> anyhow::Result<usize> {
        tokio::task::spawn_blocking(move || self.purge_expired_conversations(cutoff_unix_seconds))
            .await
            .map_err(|error| anyhow::anyhow!("conversation retention task failed: {error}"))?
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

    /// Look up provider-thread metadata on Tokio's blocking pool.
    #[allow(dead_code)] // Retained compatibility wrapper while async-store callers migrate.
    pub async fn get_provider_thread_for_owner_blocking(
        self: Arc<Self>,
        owner_key_id: String,
        conversation_id: String,
        provider: String,
    ) -> anyhow::Result<Option<ProviderThread>> {
        tokio::task::spawn_blocking(move || {
            self.get_provider_thread_for_owner(&owner_key_id, &conversation_id, &provider)
        })
        .await
        .map_err(|error| anyhow::anyhow!("provider thread lookup task failed: {error}"))?
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

    /// Persist provider-thread metadata on Tokio's blocking pool.
    #[allow(dead_code)] // Retained compatibility wrapper while async-store callers migrate.
    pub async fn upsert_provider_thread_for_owner_blocking(
        self: Arc<Self>,
        owner_key_id: String,
        thread: ProviderThread,
    ) -> anyhow::Result<()> {
        tokio::task::spawn_blocking(move || {
            self.upsert_provider_thread_for_owner(&owner_key_id, &thread)
        })
        .await
        .map_err(|error| anyhow::anyhow!("provider thread persistence task failed: {error}"))?
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

    /// Long-term memory is opt-in. Missing settings deliberately mean disabled.
    pub fn long_term_memory_enabled(&self, owner_key_id: &str) -> anyhow::Result<bool> {
        let connection = self.lock()?;
        let enabled = connection
            .query_row(
                "SELECT enabled FROM memory_settings WHERE owner_key_id=?1",
                [owner_key_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false);
        Ok(enabled)
    }

    pub fn set_long_term_memory_enabled(
        &self,
        owner_key_id: &str,
        enabled: bool,
    ) -> anyhow::Result<()> {
        if owner_key_id.trim().is_empty() {
            anyhow::bail!("owner key is required");
        }
        let now = unix_timestamp();
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO memory_settings(owner_key_id, enabled, updated_at) VALUES(?1, ?2, ?3)
             ON CONFLICT(owner_key_id) DO UPDATE SET enabled=excluded.enabled, updated_at=excluded.updated_at",
            rusqlite::params![owner_key_id, enabled, now],
        )?;
        Ok(())
    }

    /// Save or replace one owner-scoped memory key. Writes are rejected while
    /// disabled, and source conversation provenance is checked against ownership.
    pub fn save_long_term_memory_item(
        &self,
        owner_key_id: &str,
        memory_key: &str,
        value: &str,
        source_conversation_id: Option<&str>,
        confidence: f64,
        expires_at: Option<i64>,
    ) -> anyhow::Result<LongTermMemoryItem> {
        if owner_key_id.trim().is_empty()
            || memory_key.trim().is_empty()
            || memory_key.chars().count() > 128
            || value.trim().is_empty()
            || value.chars().count() > 16_384
            || !confidence.is_finite()
            || !(0.0..=1.0).contains(&confidence)
        {
            anyhow::bail!("invalid long-term memory item");
        }
        let now = unix_timestamp();
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let enabled: bool = tx
            .query_row(
                "SELECT enabled FROM memory_settings WHERE owner_key_id=?1",
                [owner_key_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if !enabled {
            anyhow::bail!("long-term memory is disabled");
        }
        if let Some(source_id) = source_conversation_id {
            let source_owner: Option<String> = tx
                .query_row(
                    "SELECT owner_key_id FROM conversations WHERE id=?1",
                    [source_id],
                    |row| row.get(0),
                )
                .optional()?;
            if source_owner.as_deref() != Some(owner_key_id) {
                anyhow::bail!("source conversation is not owned by this API key");
            }
        }
        tx.execute(
            "INSERT INTO memory_items(
                id, owner_key_id, memory_key, value, source_conversation_id,
                confidence, created_at, updated_at, expires_at
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)
             ON CONFLICT(owner_key_id, memory_key) DO UPDATE SET
                value=excluded.value,
                source_conversation_id=excluded.source_conversation_id,
                confidence=excluded.confidence,
                updated_at=excluded.updated_at,
                expires_at=excluded.expires_at",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                owner_key_id,
                memory_key.trim(),
                value.trim(),
                source_conversation_id,
                confidence,
                now,
                expires_at,
            ],
        )?;
        let item = tx.query_row(
            "SELECT id, memory_key, value, source_conversation_id, confidence,
                    created_at, updated_at, expires_at
             FROM memory_items WHERE owner_key_id=?1 AND memory_key=?2",
            rusqlite::params![owner_key_id, memory_key.trim()],
            memory_item_from_row,
        )?;
        tx.commit()?;
        Ok(item)
    }

    /// List every stored item for inspection/export, including expired items.
    /// This deliberately remains available when retrieval is disabled.
    pub fn list_long_term_memory_items(
        &self,
        owner_key_id: &str,
    ) -> anyhow::Result<Vec<LongTermMemoryItem>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, memory_key, value, source_conversation_id, confidence,
                    created_at, updated_at, expires_at
             FROM memory_items WHERE owner_key_id=?1 ORDER BY updated_at DESC, id",
        )?;
        let rows = statement.query_map([owner_key_id], memory_item_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Retrieval is empty unless explicitly enabled and always excludes expired items.

    /// Read the opt-in flag and exported items from one SQLite snapshot.
    pub fn long_term_memory_snapshot(
        &self,
        owner_key_id: &str,
    ) -> anyhow::Result<(bool, Vec<LongTermMemoryItem>)> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let enabled = tx
            .query_row(
                "SELECT enabled FROM memory_settings WHERE owner_key_id=?1",
                [owner_key_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false);
        let mut statement = tx.prepare(
            "SELECT id, memory_key, value, source_conversation_id, confidence,
                    created_at, updated_at, expires_at
             FROM memory_items WHERE owner_key_id=?1 ORDER BY updated_at DESC, id",
        )?;
        let rows = statement.query_map([owner_key_id], memory_item_from_row)?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        tx.commit()?;
        Ok((enabled, items))
    }

    /// Retrieval is empty unless explicitly enabled and always excludes expired items.
    pub fn retrieve_long_term_memory_items(
        &self,
        owner_key_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<LongTermMemoryItem>> {
        // Keep the opt-in check and item read in one transaction. Separate
        // statements without a transaction allow a concurrent disable to race
        // between the setting check and retrieval, disclosing one last batch.
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let enabled: bool = tx
            .query_row(
                "SELECT enabled FROM memory_settings WHERE owner_key_id=?1",
                [owner_key_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if !enabled {
            tx.commit()?;
            return Ok(Vec::new());
        }

        // A caller-supplied zero is an explicit request for no items, not the
        // minimum non-zero page size. Keep positive limits bounded to avoid
        // unbounded context growth.
        if limit == 0 {
            tx.commit()?;
            return Ok(Vec::new());
        }
        let limit = limit.min(20) as i64;
        let now = unix_timestamp();
        let mut statement = tx.prepare(
            "SELECT id, memory_key, value, source_conversation_id, confidence,
                    created_at, updated_at, expires_at
             FROM memory_items
             WHERE owner_key_id=?1 AND (expires_at IS NULL OR expires_at > ?2)
             ORDER BY confidence DESC, updated_at DESC, id
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            rusqlite::params![owner_key_id, now, limit],
            memory_item_from_row,
        )?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        tx.commit()?;
        Ok(items)
    }

    pub fn delete_long_term_memory_item(
        &self,
        owner_key_id: &str,
        item_id: &str,
    ) -> anyhow::Result<bool> {
        let connection = self.lock()?;
        Ok(connection.execute(
            "DELETE FROM memory_items WHERE owner_key_id=?1 AND id=?2",
            rusqlite::params![owner_key_id, item_id],
        )? == 1)
    }

    /// Delete all long-term memory items belonging to one owner. This does not
    /// affect the owner's opt-in setting or any conversation transcript.
    pub fn delete_all_long_term_memory_items(&self, owner_key_id: &str) -> anyhow::Result<usize> {
        if owner_key_id.trim().is_empty() {
            anyhow::bail!("owner key is required");
        }
        let connection = self.lock()?;
        Ok(connection.execute(
            "DELETE FROM memory_items WHERE owner_key_id=?1",
            [owner_key_id],
        )?)
    }
}

fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

fn memory_item_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LongTermMemoryItem> {
    Ok(LongTermMemoryItem {
        id: row.get(0)?,
        memory_key: row.get(1)?,
        value: row.get(2)?,
        source_conversation_id: row.get(3)?,
        confidence: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        expires_at: row.get(7)?,
    })
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


/// Async operations for opt-in cross-conversation memory controls and items.
#[async_trait::async_trait]
pub trait LongTermMemoryStore: Send + Sync {
    async fn long_term_memory_enabled_async(
        self: Arc<Self>,
        owner_key_id: String,
    ) -> anyhow::Result<bool>;

    async fn set_long_term_memory_enabled_async(
        self: Arc<Self>,
        owner_key_id: String,
        enabled: bool,
    ) -> anyhow::Result<()>;

    async fn long_term_memory_snapshot_async(
        self: Arc<Self>,
        owner_key_id: String,
    ) -> anyhow::Result<(bool, Vec<LongTermMemoryItem>)>;

    async fn save_long_term_memory_item_async(
        self: Arc<Self>,
        owner_key_id: String,
        memory_key: String,
        value: String,
        source_conversation_id: Option<String>,
        confidence: f64,
        expires_at: Option<i64>,
    ) -> anyhow::Result<LongTermMemoryItem>;

    async fn delete_long_term_memory_item_async(
        self: Arc<Self>,
        owner_key_id: String,
        item_id: String,
    ) -> anyhow::Result<bool>;

    async fn delete_all_long_term_memory_items_async(
        self: Arc<Self>,
        owner_key_id: String,
    ) -> anyhow::Result<usize>;

    async fn retrieve_long_term_memory_items_async(
        self: Arc<Self>,
        owner_key_id: String,
        limit: usize,
    ) -> anyhow::Result<Vec<LongTermMemoryItem>>;
}

#[async_trait::async_trait]
impl LongTermMemoryStore for UnifiedMemory {
    async fn long_term_memory_enabled_async(
        self: Arc<Self>,
        owner_key_id: String,
    ) -> anyhow::Result<bool> {
        tokio::task::spawn_blocking(move || self.long_term_memory_enabled(&owner_key_id))
            .await
            .map_err(|error| anyhow::anyhow!("memory settings read task failed: {error}"))?
    }

    async fn set_long_term_memory_enabled_async(
        self: Arc<Self>,
        owner_key_id: String,
        enabled: bool,
    ) -> anyhow::Result<()> {
        tokio::task::spawn_blocking(move || {
            self.set_long_term_memory_enabled(&owner_key_id, enabled)
        })
        .await
        .map_err(|error| anyhow::anyhow!("memory settings write task failed: {error}"))?
    }

    async fn long_term_memory_snapshot_async(
        self: Arc<Self>,
        owner_key_id: String,
    ) -> anyhow::Result<(bool, Vec<LongTermMemoryItem>)> {
        tokio::task::spawn_blocking(move || self.long_term_memory_snapshot(&owner_key_id))
            .await
            .map_err(|error| anyhow::anyhow!("memory snapshot task failed: {error}"))?
    }

    async fn save_long_term_memory_item_async(
        self: Arc<Self>,
        owner_key_id: String,
        memory_key: String,
        value: String,
        source_conversation_id: Option<String>,
        confidence: f64,
        expires_at: Option<i64>,
    ) -> anyhow::Result<LongTermMemoryItem> {
        tokio::task::spawn_blocking(move || {
            self.save_long_term_memory_item(
                &owner_key_id,
                &memory_key,
                &value,
                source_conversation_id.as_deref(),
                confidence,
                expires_at,
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("memory item save task failed: {error}"))?
    }

    async fn delete_long_term_memory_item_async(
        self: Arc<Self>,
        owner_key_id: String,
        item_id: String,
    ) -> anyhow::Result<bool> {
        tokio::task::spawn_blocking(move || {
            self.delete_long_term_memory_item(&owner_key_id, &item_id)
        })
        .await
        .map_err(|error| anyhow::anyhow!("memory item deletion task failed: {error}"))?
    }

    async fn delete_all_long_term_memory_items_async(
        self: Arc<Self>,
        owner_key_id: String,
    ) -> anyhow::Result<usize> {
        tokio::task::spawn_blocking(move || self.delete_all_long_term_memory_items(&owner_key_id))
            .await
            .map_err(|error| anyhow::anyhow!("memory bulk deletion task failed: {error}"))?
    }

    async fn retrieve_long_term_memory_items_async(
        self: Arc<Self>,
        owner_key_id: String,
        limit: usize,
    ) -> anyhow::Result<Vec<LongTermMemoryItem>> {
        tokio::task::spawn_blocking(move || {
            self.retrieve_long_term_memory_items(&owner_key_id, limit)
        })
        .await
        .map_err(|error| anyhow::anyhow!("memory retrieval task failed: {error}"))?
    }
}

#[cfg(test)]

#[cfg(test)]
mod async_long_term_memory_store_tests {
    use super::{ConversationStore, LongTermMemoryStore, UnifiedMemory};
    use std::sync::Arc;

    #[tokio::test]
    async fn async_store_methods_preserve_opt_in_and_owner_isolation() {
        let store = Arc::new(UnifiedMemory::new(8000));
        assert!(!store.clone().long_term_memory_enabled_async("owner-a".into()).await.unwrap());

        store.clone().set_long_term_memory_enabled_async("owner-a".into(), true).await.unwrap();
        store.clone().save_long_term_memory_item_async(
            "owner-a".into(), "timezone".into(), "America/New_York".into(),
            None, 0.9, None,
        ).await.unwrap();

        let (enabled, items) = store.clone().long_term_memory_snapshot_async("owner-a".into()).await.unwrap();
        assert!(enabled);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].value, "America/New_York");

        let (other_enabled, other_items) = store.clone().long_term_memory_snapshot_async("owner-b".into()).await.unwrap();
        assert!(!other_enabled);
        assert!(other_items.is_empty());
        assert!(store.clone().retrieve_long_term_memory_items_async("owner-b".into(), 20).await.unwrap().is_empty());

        let id = items[0].id.clone();
        assert!(!store.clone().delete_long_term_memory_item_async("owner-b".into(), id.clone()).await.unwrap());
        assert!(store.clone().delete_long_term_memory_item_async("owner-a".into(), id).await.unwrap());

        store.clone().save_long_term_memory_item_async(
            "owner-a".into(), "timezone".into(), "Europe/London".into(),
            None, 0.8, None,
        ).await.unwrap();
        assert_eq!(store.clone().delete_all_long_term_memory_items_async("owner-a".into()).await.unwrap(), 1);
        assert!(store.clone().long_term_memory_snapshot_async("owner-a".into()).await.unwrap().1.is_empty());
    }
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

#[cfg(test)]
mod retrieval_ranking_tests {
    use super::{sqlite, UnifiedMemory};
    use std::sync::Mutex;

    fn memory() -> UnifiedMemory {
        UnifiedMemory {
            max_tokens: 8000,
            connection: Mutex::new(
                sqlite::open(None, None).expect("in-memory SQLite should initialize"),
            ),
        }
    }

    #[test]
    fn long_term_memory_retrieval_uses_stable_confidence_time_and_id_ordering() {
        let memory = memory();
        memory
            .set_long_term_memory_enabled("ranking-owner", true)
            .unwrap();
        let low = memory
            .save_long_term_memory_item("ranking-owner", "low-confidence", "low", None, 0.1, None)
            .unwrap();
        let high_old = memory
            .save_long_term_memory_item("ranking-owner", "high-old", "older", None, 0.9, None)
            .unwrap();
        let high_new = memory
            .save_long_term_memory_item("ranking-owner", "high-new", "newer A", None, 0.9, None)
            .unwrap();
        let high_tie = memory
            .save_long_term_memory_item("ranking-owner", "high-tie", "newer B", None, 0.9, None)
            .unwrap();

        {
            let connection = memory.lock().unwrap();
            for (key, timestamp) in [
                ("low-confidence", 300_i64),
                ("high-old", 100_i64),
                ("high-new", 200_i64),
                ("high-tie", 200_i64),
            ] {
                connection
                    .execute(
                        "UPDATE memory_items SET updated_at=?1 WHERE owner_key_id=?2 AND memory_key=?3",
                        rusqlite::params![timestamp, "ranking-owner", key],
                    )
                    .unwrap();
            }
        }

        let retrieved = memory
            .retrieve_long_term_memory_items("ranking-owner", 20)
            .unwrap();
        let mut tied = [
            (high_new.id.clone(), high_new.memory_key.clone()),
            (high_tie.id.clone(), high_tie.memory_key.clone()),
        ];
        tied.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(retrieved.len(), 4);
        assert_eq!(retrieved[0].memory_key, tied[0].1);
        assert_eq!(retrieved[1].memory_key, tied[1].1);
        assert_eq!(retrieved[2].id, high_old.id);
        assert_eq!(retrieved[3].id, low.id);
    }

    #[test]
    fn long_term_memory_disabled_expired_and_deleted_items_never_retrieve() {
        let memory = memory();

        assert!(
            memory
                .save_long_term_memory_item(
                    "memory-owner",
                    "preference",
                    "must not persist while disabled",
                    None,
                    0.9,
                    None,
                )
                .is_err(),
            "saving personal memory must require explicit opt-in"
        );
        assert!(memory
            .retrieve_long_term_memory_items("memory-owner", 20)
            .unwrap()
            .is_empty());

        memory
            .set_long_term_memory_enabled("memory-owner", true)
            .unwrap();
        for source_id in ["source-old", "source-current"] {
            memory
                .store_turn_for_owner_with_tab_id(
                    "memory-owner",
                    source_id.to_owned(),
                    vec![super::ChatMessage {
                        role: "user".to_owned(),
                        content: "source prompt".to_owned(),
                    }],
                    super::ChatMessage {
                        role: "assistant".to_owned(),
                        content: "source answer".to_owned(),
                    },
                    "chatgpt".to_owned(),
                    None,
                )
                .unwrap();
        }

        let expired_at = super::unix_timestamp().saturating_sub(1);
        let expired = memory
            .save_long_term_memory_item(
                "memory-owner",
                "expired",
                "stale value",
                Some("source-old"),
                1.0,
                Some(expired_at),
            )
            .unwrap();
        let active = memory
            .save_long_term_memory_item(
                "memory-owner",
                "active",
                "current value",
                Some("source-current"),
                0.8,
                None,
            )
            .unwrap();

        let retrieved = memory
            .retrieve_long_term_memory_items("memory-owner", 20)
            .unwrap();
        assert_eq!(retrieved.len(), 1);
        assert_eq!(retrieved[0].id, active.id);
        assert_ne!(retrieved[0].id, expired.id);

        assert!(memory
            .delete_long_term_memory_item("memory-owner", &active.id)
            .unwrap());
        assert!(memory
            .retrieve_long_term_memory_items("memory-owner", 20)
            .unwrap()
            .is_empty());
        assert!(!memory
            .delete_long_term_memory_item("another-owner", &expired.id)
            .unwrap());
    }
}

#[cfg(test)]
mod provider_thread_map_isolation_tests {
    use super::{ChatMessage, ProviderThread, UnifiedMemory};

    fn persist_conversation(memory: &UnifiedMemory, id: &str, owner: &str) {
        memory
            .store_turn_for_owner_with_tab_id(
                owner,
                id.to_owned(),
                vec![ChatMessage {
                    role: "user".to_owned(),
                    content: format!("question for {id}"),
                }],
                ChatMessage {
                    role: "assistant".to_owned(),
                    content: format!("answer for {id}"),
                },
                "chatgpt".to_owned(),
                None,
            )
            .expect("conversation turn should persist");
    }

    fn thread(conversation_id: &str, provider: &str, url: &str, marker: &str) -> ProviderThread {
        ProviderThread {
            conversation_id: conversation_id.to_owned(),
            provider: provider.to_owned(),
            thread_url: url.to_owned(),
            synced_through_message_id: Some(marker.to_owned()),
            model: Some("test-model".to_owned()),
            updated_at: 1,
        }
    }

    #[test]
    fn interleaved_conversations_and_provider_switch_keep_independent_thread_markers() {
        let memory = UnifiedMemory::new(8000);
        persist_conversation(&memory, "conversation-a", "owner-a");
        persist_conversation(&memory, "conversation-b", "owner-a");

        memory
            .upsert_provider_thread_for_owner(
                "owner-a",
                &thread(
                    "conversation-a",
                    "chatgpt",
                    "https://chatgpt.com/c/a",
                    "marker-a1",
                ),
            )
            .unwrap();
        memory
            .upsert_provider_thread_for_owner(
                "owner-a",
                &thread(
                    "conversation-b",
                    "chatgpt",
                    "https://chatgpt.com/c/b",
                    "marker-b1",
                ),
            )
            .unwrap();
        memory
            .upsert_provider_thread_for_owner(
                "owner-a",
                &thread(
                    "conversation-a",
                    "claude",
                    "https://claude.ai/chat/a",
                    "marker-a-claude",
                ),
            )
            .unwrap();

        memory
            .upsert_provider_thread_for_owner(
                "owner-a",
                &thread(
                    "conversation-a",
                    "chatgpt",
                    "https://chatgpt.com/c/a",
                    "marker-a2",
                ),
            )
            .unwrap();

        assert_eq!(
            memory
                .get_provider_thread_for_owner("owner-a", "conversation-a", "chatgpt")
                .unwrap()
                .unwrap()
                .synced_through_message_id
                .as_deref(),
            Some("marker-a2")
        );
        assert_eq!(
            memory
                .get_provider_thread_for_owner("owner-a", "conversation-b", "chatgpt")
                .unwrap()
                .unwrap()
                .synced_through_message_id
                .as_deref(),
            Some("marker-b1")
        );
        assert_eq!(
            memory
                .get_provider_thread_for_owner("owner-a", "conversation-a", "claude")
                .unwrap()
                .unwrap()
                .synced_through_message_id
                .as_deref(),
            Some("marker-a-claude")
        );
        assert!(memory
            .get_provider_thread_for_owner("owner-b", "conversation-a", "chatgpt")
            .unwrap()
            .is_none());
    }
}
