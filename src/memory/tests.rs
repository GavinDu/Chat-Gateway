// Helpers are shared by several test targets; not every target exercises each fixture helper.
#![allow(dead_code)]

use super::*;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
    thread,
};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("gateway-sqlite-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn db(&self) -> PathBuf {
        self.0.join("conversations.sqlite3")
    }

    fn json(&self) -> PathBuf {
        self.0.join("conversations.json")
    }

    fn open(&self) -> UnifiedMemory {
        open(&self.db(), Some(&self.json())).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn open(path: &Path, legacy: Option<&Path>) -> anyhow::Result<UnifiedMemory> {
    Ok(UnifiedMemory {
        max_tokens: 8000,
        connection: Mutex::new(sqlite::open(Some(path), legacy)?),
    })
}

fn m(role: &str, content: &str) -> ChatMessage {
    ChatMessage {
        role: role.into(),
        content: content.into(),
    }
}

fn store(memory: &UnifiedMemory, id: &str, text: &str) {
    memory
        .store_turn(
            id.into(),
            vec![m("user", text)],
            m("assistant", &format!("answer {text}")),
            "chatgpt".into(),
            format!("tab-{text}"),
        )
        .unwrap();
}

fn snapshot(fixture: &Fixture) -> Vec<u8> {
    let conversation = Conversation {
        id: "legacy".into(),
        messages: vec![m("user", "hello"), m("assistant", "hi")],
        provider: Some("qwen".into()),
        tab_id: Some("legacy-tab".into()),
    };
    let bytes = serde_json::to_vec(&HashMap::from([("legacy", conversation)])).unwrap();
    fs::write(fixture.json(), &bytes).unwrap();
    bytes
}

#[test]
fn json_import_restores_order_and_affinity_and_preserves_source() {
    let fixture = Fixture::new();
    let bytes = snapshot(&fixture);
    {
        let memory = fixture.open();
        assert_eq!(
            memory.get_provider("legacy").unwrap().as_deref(),
            Some("qwen")
        );
        assert_eq!(
            memory.get_tab("legacy").unwrap().as_deref(),
            Some("legacy-tab")
        );
        assert_eq!(
            memory.conversation("legacy").unwrap().unwrap().messages,
            vec![m("user", "hello"), m("assistant", "hi")]
        );
    }
    assert_eq!(fs::read(fixture.json()).unwrap(), bytes);
    assert!(fixture.open().has_history("legacy").unwrap());
}

#[test]
fn failed_import_rolls_back_schema_and_can_be_retried_without_source_loss() {
    let fixture = Fixture::new();
    let invalid = serde_json::json!({
        "valid": {"id":"valid","messages":[],"provider":null,"tab_id":null},
        "wrong": {"id":"different","messages":[],"provider":null,"tab_id":null}
    });
    let bytes = serde_json::to_vec(&invalid).unwrap();
    fs::write(fixture.json(), &bytes).unwrap();
    assert!(open(&fixture.db(), Some(&fixture.json())).is_err());
    assert_eq!(fs::read(fixture.json()).unwrap(), bytes);
    let connection = Connection::open(fixture.db()).unwrap();
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    let tables: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((version, tables), (0, 0));
    drop(connection);
    snapshot(&fixture);
    assert!(fixture.open().has_history("legacy").unwrap());
}

#[test]
fn corrupt_json_fails_closed_and_is_not_replaced() {
    let fixture = Fixture::new();
    fs::write(fixture.json(), b"{ broken").unwrap();
    let error = open(&fixture.db(), Some(&fixture.json())).err().unwrap();
    assert!(format!("{error:#}").contains("legacy snapshot"));
    assert_eq!(fs::read(fixture.json()).unwrap(), b"{ broken");
}

#[test]
fn completed_import_never_replays_json_or_resurrects_deleted_conversations() {
    let fixture = Fixture::new();
    snapshot(&fixture);
    {
        let memory = fixture.open();
        assert!(memory.delete_conversation("legacy").unwrap());
    }
    // Even a subsequently corrupt legacy file must not block a migrated store.
    fs::write(fixture.json(), b"corrupted old backup").unwrap();
    assert!(!fixture.open().has_history("legacy").unwrap());
}

#[test]
fn restart_recovers_messages_provider_and_tab() {
    let fixture = Fixture::new();
    {
        let memory = fixture.open();
        store(&memory, "one", "first");
        store(&memory, "one", "second");
    }
    let memory = fixture.open();
    assert_eq!(
        memory.get_provider("one").unwrap().as_deref(),
        Some("chatgpt")
    );
    assert_eq!(
        memory.get_tab("one").unwrap().as_deref(),
        Some("tab-second")
    );
    assert_eq!(
        memory.conversation("one").unwrap().unwrap().messages,
        vec![
            m("user", "first"),
            m("assistant", "answer first"),
            m("user", "second"),
            m("assistant", "answer second"),
        ]
    );
}

#[test]
fn deletion_cascades_and_survives_restart() {
    let fixture = Fixture::new();
    {
        let memory = fixture.open();
        store(&memory, "delete", "secret");
        store(&memory, "keep", "other");
        assert!(memory.delete_conversation("delete").unwrap());
        assert!(!memory.delete_conversation("delete").unwrap());
        assert_eq!(memory.get_tab("delete").unwrap(), None);
    }
    let memory = fixture.open();
    assert!(!memory.has_history("delete").unwrap());
    assert!(memory.has_history("keep").unwrap());
    let count: i64 = memory
        .lock()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM messages WHERE conversation_id='delete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn injected_insert_failure_rolls_back_partial_messages_and_affinity() {
    let fixture = Fixture::new();
    let memory = fixture.open();
    store(&memory, "existing", "original");
    let previous = memory.conversation("existing").unwrap().unwrap();
    memory
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_answer BEFORE INSERT ON messages
         WHEN NEW.content='fail' BEGIN SELECT RAISE(ABORT,'injected write failure'); END;",
        )
        .unwrap();
    for id in ["existing", "new"] {
        assert!(memory
            .store_turn(
                id.into(),
                vec![m("user", "partial")],
                m("assistant", "fail"),
                "qwen".into(),
                "changed-tab".into(),
            )
            .is_err());
    }
    assert_eq!(memory.conversation("existing").unwrap().unwrap(), previous);
    assert!(!memory.has_history("new").unwrap());
    drop(memory);
    let restored = fixture.open();
    assert_eq!(
        restored.conversation("existing").unwrap().unwrap(),
        previous
    );
    assert!(!restored.has_history("new").unwrap());
}

#[test]
fn injected_delete_failure_preserves_conversation() {
    let fixture = Fixture::new();
    let memory = fixture.open();
    store(&memory, "keep", "original");
    memory
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_delete BEFORE DELETE ON messages
         BEGIN SELECT RAISE(ABORT,'injected delete failure'); END;",
        )
        .unwrap();
    assert!(memory.delete_conversation("keep").is_err());
    assert!(memory.has_history("keep").unwrap());
    drop(memory);
    assert!(fixture.open().has_history("keep").unwrap());
}

#[test]
fn separate_connections_observe_commits_instead_of_stale_caches() {
    let fixture = Fixture::new();
    let first = fixture.open();
    let second = fixture.open();
    store(&first, "one", "hello");
    assert!(second.has_history("one").unwrap());
    assert!(second.delete_conversation("one").unwrap());
    assert!(!first.has_history("one").unwrap());
}

fn concurrent_writes(same_conversation: bool) {
    let fixture = Fixture::new();
    // Separate connections are essential: a shared Rust mutex alone would hide
    // stale read/overwrite bugs across store instances.
    let stores: Vec<_> = (0..8).map(|_| fixture.open()).collect();
    let barrier = Arc::new(Barrier::new(stores.len()));
    thread::scope(|scope| {
        for (i, memory) in stores.into_iter().enumerate() {
            let barrier = barrier.clone();
            scope.spawn(move || {
                barrier.wait();
                let id = if same_conversation {
                    "shared".into()
                } else {
                    format!("conv-{i}")
                };
                store(&memory, &id, &format!("turn-{i}"));
            });
        }
    });
    let memory = fixture.open();
    if same_conversation {
        let messages = memory.conversation("shared").unwrap().unwrap().messages;
        assert_eq!(messages.len(), 16);
        let mut users = std::collections::HashSet::new();
        let (pairs, remainder) = messages.as_chunks::<2>();
        assert!(remainder.is_empty());
        for pair in pairs {
            assert_eq!(pair[0].role, "user");
            assert_eq!(
                pair[1],
                m("assistant", &format!("answer {}", pair[0].content))
            );
            assert!(users.insert(pair[0].content.clone()));
        }
    } else {
        for i in 0..8 {
            assert_eq!(
                memory
                    .conversation(&format!("conv-{i}"))
                    .unwrap()
                    .unwrap()
                    .messages
                    .len(),
                2
            );
        }
    }
}

#[test]
fn concurrent_different_conversations_do_not_overwrite() {
    concurrent_writes(false);
}

#[test]
fn concurrent_same_conversation_preserves_ordered_atomic_turns() {
    concurrent_writes(true);
}

#[test]
fn concurrent_identical_transcript_replays_are_idempotent() {
    let fixture = Fixture::new();
    let stores: Vec<_> = (0..8).map(|_| fixture.open()).collect();
    let barrier = Arc::new(Barrier::new(8));
    thread::scope(|scope| {
        for memory in stores {
            let barrier = barrier.clone();
            scope.spawn(move || {
                barrier.wait();
                store(&memory, "shared", "same");
            });
        }
    });
    assert_eq!(
        fixture
            .open()
            .conversation("shared")
            .unwrap()
            .unwrap()
            .messages
            .len(),
        2
    );
}

#[test]
fn concurrent_branched_full_transcripts_do_not_duplicate_shared_history() {
    let fixture = Fixture::new();
    store(&fixture.open(), "shared", "base");
    let stores: Vec<_> = (0..8).map(|_| fixture.open()).collect();
    let barrier = Arc::new(Barrier::new(8));
    thread::scope(|scope| {
        for (i, memory) in stores.into_iter().enumerate() {
            let barrier = barrier.clone();
            scope.spawn(move || {
                barrier.wait();
                memory
                    .store_turn(
                        "shared".into(),
                        vec![
                            m("user", "base"),
                            m("assistant", "answer base"),
                            m("user", &format!("branch-{i}")),
                        ],
                        m("assistant", &format!("answer branch-{i}")),
                        "qwen".into(),
                        format!("tab-{i}"),
                    )
                    .unwrap();
            });
        }
    });
    let messages = fixture
        .open()
        .conversation("shared")
        .unwrap()
        .unwrap()
        .messages;
    assert_eq!(messages.len(), 18);
    assert_eq!(messages.iter().filter(|m| m.content == "base").count(), 1);
}

#[test]
fn repeated_prompt_with_a_different_answer_is_a_new_turn() {
    let memory = UnifiedMemory::new(8000);
    store(&memory, "one", "repeat");
    memory
        .store_turn(
            "one".into(),
            vec![m("user", "repeat")],
            m("assistant", "different"),
            "qwen".into(),
            "new-tab".into(),
        )
        .unwrap();
    assert_eq!(
        memory.conversation("one").unwrap().unwrap().messages.len(),
        4
    );
    assert_eq!(memory.get_tab("one").unwrap().as_deref(), Some("new-tab"));
}

#[test]
fn schema_v5_migrates_to_v6_with_long_term_memory_disabled_by_default() {
    let fixture = Fixture::new();
    {
        let memory = fixture.open();
        memory
            .lock()
            .unwrap()
            .execute_batch(
                "DROP INDEX memory_items_owner_updated_idx;
                 DROP INDEX memory_items_owner_key_idx;
                 DROP TABLE memory_items;
                 DROP TABLE memory_settings;
                 PRAGMA user_version=5;",
            )
            .unwrap();
    }
    let memory = fixture.open();
    let version: i64 = memory
        .lock()
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 6);
    assert!(!memory.long_term_memory_enabled("owner-a").unwrap());
    let item_count: i64 = memory
        .lock()
        .unwrap()
        .query_row("SELECT count(*) FROM memory_items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(item_count, 0);
}

#[test]
fn long_term_memory_is_opt_in_owner_scoped_and_deletable() {
    let memory = UnifiedMemory::new(8000);
    assert!(!memory.long_term_memory_enabled("owner-a").unwrap());
    assert!(memory
        .save_long_term_memory_item("owner-a", "preference", "concise answers", None, 0.9, None)
        .is_err());
    assert!(memory
        .retrieve_long_term_memory_items("owner-a", 5)
        .unwrap()
        .is_empty());

    memory.set_long_term_memory_enabled("owner-a", true).unwrap();
    let item = memory
        .save_long_term_memory_item("owner-a", "preference", "concise answers", None, 0.9, None)
        .unwrap();
    assert_eq!(item.memory_key, "preference");
    assert_eq!(memory.retrieve_long_term_memory_items("owner-b", 5).unwrap().len(), 0);
    assert_eq!(memory.retrieve_long_term_memory_items("owner-a", 5).unwrap(), vec![item.clone()]);

    assert!(!memory.delete_long_term_memory_item("owner-b", &item.id).unwrap());
    assert!(memory.delete_long_term_memory_item("owner-a", &item.id).unwrap());
    assert!(memory.list_long_term_memory_items("owner-a").unwrap().is_empty());
}

#[test]
fn long_term_memory_provenance_cannot_reference_another_owners_conversation() {
    let memory = UnifiedMemory::new(8000);
    memory.set_long_term_memory_enabled("owner-a", true).unwrap();
    memory.set_long_term_memory_enabled("owner-b", true).unwrap();
    memory
        .store_turn_for_owner(
            "owner-a",
            "source-conversation".into(),
            vec![m("user", "private source")],
            m("assistant", "private response"),
            "chatgpt".into(),
            "tab".into(),
        )
        .unwrap();

    assert!(memory
        .save_long_term_memory_item(
            "owner-b",
            "stolen-provenance",
            "not allowed",
            Some("source-conversation"),
            0.9,
            None,
        )
        .is_err());
    assert!(memory
        .list_long_term_memory_items("owner-b")
        .unwrap()
        .is_empty());
}

#[test]
fn long_term_memory_disable_stops_retrieval_without_erasing_inspectable_items() {
    let memory = UnifiedMemory::new(8000);
    memory.set_long_term_memory_enabled("owner-a", true).unwrap();
    memory
        .save_long_term_memory_item("owner-a", "timezone", "America/New_York", None, 1.0, None)
        .unwrap();

    memory.set_long_term_memory_enabled("owner-a", false).unwrap();
    assert!(memory.retrieve_long_term_memory_items("owner-a", 5).unwrap().is_empty());
    assert_eq!(memory.list_long_term_memory_items("owner-a").unwrap().len(), 1);
}

#[test]
fn long_term_memory_rejects_invalid_confidence_and_expired_items() {
    let memory = UnifiedMemory::new(8000);
    memory.set_long_term_memory_enabled("owner-a", true).unwrap();
    assert!(memory
        .save_long_term_memory_item("owner-a", "invalid", "value", None, f64::NAN, None)
        .is_err());
    memory
        .save_long_term_memory_item(
            "owner-a",
            "expired",
            "stale fact",
            None,
            1.0,
            Some(1),
        )
        .unwrap();
    assert!(memory.retrieve_long_term_memory_items("owner-a", 5).unwrap().is_empty());
    assert_eq!(memory.list_long_term_memory_items("owner-a").unwrap().len(), 1);
}

#[test]
fn full_and_sliding_transcripts_append_only_new_suffix() {
    let memory = UnifiedMemory::new(8000);
    store(&memory, "one", "first");
    memory
        .store_turn(
            "one".into(),
            vec![
                m("user", "first"),
                m("assistant", "answer first"),
                m("user", "second"),
            ],
            m("assistant", "answer second"),
            "chatgpt".into(),
            "tab".into(),
        )
        .unwrap();
    memory
        .store_turn(
            "one".into(),
            vec![m("assistant", "answer second"), m("user", "third")],
            m("assistant", "answer third"),
            "chatgpt".into(),
            "tab".into(),
        )
        .unwrap();
    assert_eq!(
        memory.conversation("one").unwrap().unwrap().messages.len(),
        6
    );
    let (prompt, messages) = memory
        .build_context(Some("one"), &[m("user", "fourth")])
        .unwrap();
    assert_eq!(messages.len(), 7);
    assert!(prompt.ends_with("fourth\n</conversation_message>\n"));
}

#[test]
fn context_token_window_and_unrelated_turn_are_preserved() {
    let mut memory = UnifiedMemory::new(8000);
    store(&memory, "one", "old");
    let (_, messages) = memory
        .build_context(Some("one"), &[m("user", "unrelated")])
        .unwrap();
    assert_eq!(messages.len(), 3);
    memory.max_tokens = 1;
    let (_, messages) = memory
        .build_context(Some("one"), &[m("user", "last")])
        .unwrap();
    assert_eq!(messages, vec![m("user", "last")]);
}

#[test]
fn corrupt_database_fails_closed_without_replacing_bytes() {
    let fixture = Fixture::new();
    let bytes = b"not a SQLite database";
    fs::write(fixture.db(), bytes).unwrap();
    let error = open(&fixture.db(), None).err().unwrap();
    assert!(format!("{error:#}").contains("failed validation/migration"));
    assert_eq!(fs::read(fixture.db()).unwrap(), bytes);
}

#[test]
fn unsupported_schema_fails_closed_without_importing_json() {
    let fixture = Fixture::new();
    snapshot(&fixture);
    {
        let memory = fixture.open();
        memory
            .lock()
            .unwrap()
            .execute_batch("PRAGMA user_version=999;")
            .unwrap();
    }
    let bytes = fs::read(fixture.db()).unwrap();
    let error = open(&fixture.db(), Some(&fixture.json())).err().unwrap();
    assert!(format!("{error:#}").contains("unsupported conversation schema"));
    assert_eq!(fs::read(fixture.db()).unwrap(), bytes);
}

#[test]
fn unversioned_nonempty_database_is_not_overwritten() {
    let fixture = Fixture::new();
    Connection::open(fixture.db())
        .unwrap()
        .execute_batch("CREATE TABLE unrelated(secret TEXT);")
        .unwrap();
    let error = open(&fixture.db(), None).err().unwrap();
    assert!(format!("{error:#}").contains("unversioned/nonempty"));
}

#[test]
fn context_read_errors_are_returned_instead_of_panicking_or_dropping_history() {
    let fixture = Fixture::new();
    let memory = fixture.open();
    store(&memory, "read-failure", "durable turn");

    // Simulate a post-startup schema/storage failure through a second connection.
    // The request path must return an error rather than panic or send empty history.
    Connection::open(fixture.db())
        .unwrap()
        .execute_batch("DROP TABLE messages;")
        .unwrap();

    let result = memory.build_context(Some("read-failure"), &[m("user", "next turn")]);
    assert!(result.is_err());
    // Provider affinity is metadata-only and should remain readable even when
    // the transcript table is unavailable; history/context reads must fail closed.
    assert!(memory.get_provider("read-failure").is_ok());
    assert!(memory.has_history("read-failure").is_err());
}

#[test]
fn supported_version_with_missing_schema_fails_closed() {
    let fixture = Fixture::new();
    {
        let memory = fixture.open();
        memory
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE messages;")
            .unwrap();
    }
    assert!(open(&fixture.db(), None).is_err());
}

#[test]
fn supported_version_without_required_constraints_fails_closed() {
    let fixture = Fixture::new();
    {
        let memory = fixture.open();
        memory.lock().unwrap().execute_batch(
            "DROP TABLE messages;
             CREATE TABLE messages(conversation_id TEXT, position INTEGER, role TEXT, content TEXT);"
        ).unwrap();
    }
    let error = open(&fixture.db(), None).err().unwrap();
    assert!(format!("{error:#}").contains("invalid schema definition"));
}

#[test]
fn orphan_messages_fail_startup() {
    let fixture = Fixture::new();
    drop(fixture.open());
    let connection = Connection::open(fixture.db()).unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys=OFF;
         INSERT INTO messages VALUES('missing',0,'user','orphan');",
        )
        .unwrap();
    drop(connection);
    let error = open(&fixture.db(), None).err().unwrap();
    assert!(format!("{error:#}").contains("invalid foreign keys"));
}

#[test]
fn invalid_message_order_fails_startup() {
    let fixture = Fixture::new();
    {
        let memory = fixture.open();
        store(&memory, "one", "hello");
        memory
            .lock()
            .unwrap()
            .execute("UPDATE messages SET position=8 WHERE position=1", [])
            .unwrap();
    }
    let error = open(&fixture.db(), None).err().unwrap();
    assert!(format!("{error:#}").contains("invalid message ordering"));
}

#[cfg(unix)]
#[test]
fn database_and_transaction_journal_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let memory = fixture.open();
    assert_eq!(
        fs::metadata(fixture.db()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let mut connection = memory.lock().unwrap();
    let tx = connection.transaction().unwrap();
    tx.execute("INSERT INTO conversations(id) VALUES('private')", [])
        .unwrap();
    let wal = PathBuf::from(format!("{}-wal", fixture.db().display()));
    assert_eq!(
        fs::metadata(&wal).unwrap().permissions().mode() & 0o777,
        0o600
    );
    tx.rollback().unwrap();
}

#[cfg(unix)]
#[test]
fn existing_database_permissions_are_tightened_and_symlinks_rejected() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let fixture = Fixture::new();
    drop(fixture.open());
    fs::set_permissions(fixture.db(), fs::Permissions::from_mode(0o644)).unwrap();
    drop(fixture.open());
    assert_eq!(
        fs::metadata(fixture.db()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let link = fixture.0.join("linked.sqlite3");
    symlink(fixture.db(), &link).unwrap();
    assert!(open(&link, None).is_err());
}

#[test]
fn conversation_export_returns_complete_durable_record_and_missing_is_none() {
    let fixture = Fixture::new();
    let memory = fixture.open();
    store(&memory, "export-me", "export payload");

    let exported = memory.export_conversation("export-me").unwrap().unwrap();
    assert_eq!(exported.id, "export-me");
    assert_eq!(exported.provider.as_deref(), Some("chatgpt"));
    assert_eq!(exported.tab_id.as_deref(), Some("tab-export payload"));
    assert_eq!(
        exported.messages,
        vec![
            m("user", "export payload"),
            m("assistant", "answer export payload"),
        ]
    );
    assert_eq!(memory.export_conversation("missing-export").unwrap(), None);
}

#[test]
fn provider_thread_schema_supports_multiple_providers_and_cascades_on_delete() {
    let fixture = Fixture::new();
    let memory = fixture.open();
    store(&memory, "thread-map", "hello");

    {
        let connection = memory.lock().unwrap();
        let upsert = "INSERT INTO provider_threads(
                conversation_id, provider, thread_url, synced_through_message_id, model, updated_at
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(conversation_id, provider) DO UPDATE SET
                thread_url=excluded.thread_url,
                synced_through_message_id=excluded.synced_through_message_id,
                model=excluded.model,
                updated_at=excluded.updated_at";
        connection
            .execute(
                upsert,
                rusqlite::params![
                    "thread-map",
                    "chatgpt",
                    "https://chatgpt.com/c/thread-one",
                    "message-1",
                    "gpt-4o",
                    100,
                ],
            )
            .unwrap();
        connection
            .execute(
                upsert,
                rusqlite::params![
                    "thread-map",
                    "chatgpt",
                    "https://chatgpt.com/c/thread-two",
                    "message-2",
                    "gpt-4o",
                    200,
                ],
            )
            .unwrap();
        connection
            .execute(
                upsert,
                rusqlite::params![
                    "thread-map",
                    "claude",
                    "https://claude.ai/chat/thread-claude",
                    "message-3",
                    "claude-sonnet",
                    150,
                ],
            )
            .unwrap();

        let chatgpt_url: String = connection
            .query_row(
                "SELECT thread_url FROM provider_threads WHERE conversation_id=?1 AND provider=?2",
                ["thread-map", "chatgpt"],
                |row| row.get(0),
            )
            .unwrap();
        let claude_url: String = connection
            .query_row(
                "SELECT thread_url FROM provider_threads WHERE conversation_id=?1 AND provider=?2",
                ["thread-map", "claude"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(chatgpt_url, "https://chatgpt.com/c/thread-two");
        assert_eq!(claude_url, "https://claude.ai/chat/thread-claude");
    }

    assert!(memory.delete_conversation("thread-map").unwrap());
    let connection = memory.lock().unwrap();
    let remaining: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM provider_threads WHERE conversation_id=?1",
            ["thread-map"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0);
}

#[test]
fn schema_v1_database_migrates_provider_thread_map_without_losing_conversations() {
    let fixture = Fixture::new();
    {
        let connection = Connection::open(fixture.db()).unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys=ON;
                 CREATE TABLE conversations (
                    id TEXT PRIMARY KEY NOT NULL,
                    provider TEXT,
                    tab_id TEXT
                 );
                 CREATE TABLE messages (
                    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                    position INTEGER NOT NULL CHECK(position >= 0),
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    PRIMARY KEY(conversation_id, position)
                 );
                 CREATE TABLE metadata (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
                 INSERT INTO conversations(id,provider,tab_id) VALUES('existing','chatgpt','tab-existing');
                 INSERT INTO messages(conversation_id,position,role,content) VALUES('existing',0,'user','keep me');
                 INSERT INTO metadata(key,value) VALUES('legacy_import_complete','1');
                 PRAGMA application_id=1128748877;
                 PRAGMA user_version=1;",
            )
            .unwrap();
    }

    let memory = open(&fixture.db(), None).unwrap();
    assert_eq!(
        memory.conversation("existing").unwrap().unwrap().messages,
        vec![m("user", "keep me")]
    );
    let connection = memory.lock().unwrap();
    let mapped_threads: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM provider_threads WHERE conversation_id=?1",
            ["existing"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(mapped_threads, 0);
    drop(connection);
    let version: i64 = memory
        .lock()
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 6);
    let activity: i64 = memory
        .lock()
        .unwrap()
        .query_row(
            "SELECT last_activity_at FROM conversations WHERE id=?1",
            ["existing"],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        activity > 0,
        "migration should start retention from migration time"
    );
    let owner: String = memory
        .lock()
        .unwrap()
        .query_row(
            "SELECT owner_key_id FROM conversations WHERE id='existing'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        owner, "local",
        "legacy rows must remain in the local tenant"
    );
    assert_eq!(memory.purge_expired_conversations(1).unwrap(), 0);
}

#[test]
fn conversation_listing_is_paged_and_reports_message_counts() {
    let fixture = Fixture::new();
    let memory = fixture.open();
    store(&memory, "list-alpha", "alpha");
    store(&memory, "list-bravo", "bravo");
    store(&memory, "list-charlie", "charlie");

    let first = memory.list_conversations(2, 0).unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].id, "list-alpha");
    assert_eq!(first[0].provider.as_deref(), Some("chatgpt"));
    assert_eq!(first[0].message_count, 2);

    let second = memory.list_conversations(2, 2).unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].id, "list-charlie");
    assert!(memory.list_conversations(0, 0).unwrap().is_empty());
}

#[test]
fn sqlite_checkpoint_is_safe_after_durable_turns_and_can_repeat() {
    let fixture = Fixture::new();
    let memory = fixture.open();
    memory
        .store_turn(
            "shutdown-checkpoint".into(),
            vec![m("user", "persist before shutdown")],
            m("assistant", "persisted"),
            "chatgpt".into(),
            "tab".into(),
        )
        .unwrap();

    memory.checkpoint().unwrap();
    memory.checkpoint().unwrap();

    let conversation = memory
        .export_conversation("shutdown-checkpoint")
        .unwrap()
        .unwrap();
    assert_eq!(
        conversation.messages,
        vec![
            m("user", "persist before shutdown"),
            m("assistant", "persisted")
        ],
    );
}

#[test]
fn provider_thread_mapping_is_provider_scoped_and_survives_restart() {
    let fixture = Fixture::new();
    {
        let memory = fixture.open();
        store(&memory, "thread-api", "seed");
        memory
            .upsert_provider_thread(&ProviderThread {
                conversation_id: "thread-api".into(),
                provider: "chatgpt".into(),
                thread_url: "https://chatgpt.com/c/thread-a".into(),
                synced_through_message_id: Some("message-3".into()),
                model: Some("gpt-4o".into()),
                updated_at: 100,
            })
            .unwrap();
        memory
            .upsert_provider_thread(&ProviderThread {
                conversation_id: "thread-api".into(),
                provider: "claude".into(),
                thread_url: "https://claude.ai/chat/thread-b".into(),
                synced_through_message_id: None,
                model: Some("claude-sonnet".into()),
                updated_at: 101,
            })
            .unwrap();
        let updated = ProviderThread {
            conversation_id: "thread-api".into(),
            provider: "chatgpt".into(),
            thread_url: "https://chatgpt.com/c/thread-a".into(),
            synced_through_message_id: Some("message-4".into()),
            model: Some("gpt-4o".into()),
            updated_at: 102,
        };
        memory.upsert_provider_thread(&updated).unwrap();
        assert_eq!(
            memory.get_provider_thread("thread-api", "chatgpt").unwrap(),
            Some(updated)
        );
        assert!(memory
            .get_provider_thread("thread-api", "gemini")
            .unwrap()
            .is_none());
    }

    let reopened = fixture.open();
    assert_eq!(
        reopened
            .get_provider_thread("thread-api", "claude")
            .unwrap()
            .unwrap()
            .thread_url,
        "https://claude.ai/chat/thread-b"
    );
}

#[test]
fn retention_purge_removes_expired_conversations_and_cascades_threads() {
    let fixture = Fixture::new();
    let memory = fixture.open();
    store(&memory, "expired-conversation", "expired");

    {
        let connection = memory.lock().unwrap();
        connection
            .execute(
                "INSERT INTO provider_threads(conversation_id,provider,thread_url,updated_at)
                 VALUES(?1,?2,?3,?4)",
                rusqlite::params![
                    "expired-conversation",
                    "chatgpt",
                    "https://example.test/thread",
                    1_i64
                ],
            )
            .unwrap();
    }

    assert_eq!(memory.purge_expired_conversations(i64::MAX).unwrap(), 1);
    assert!(memory
        .export_conversation("expired-conversation")
        .unwrap()
        .is_none());
    let connection = memory.lock().unwrap();
    let thread_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM provider_threads", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(thread_count, 0);
}

#[test]
fn conversations_and_provider_threads_are_isolated_by_api_key_owner() {
    let fixture = Fixture::new();
    let memory = fixture.open();
    memory
        .store_turn_for_owner(
            "alpha",
            "tenant-chat-1".into(),
            vec![m("user", "private prompt")],
            m("assistant", "private answer"),
            "chatgpt".into(),
            "tab-tenant".into(),
        )
        .unwrap();
    memory
        .upsert_provider_thread_for_owner(
            "alpha",
            &ProviderThread {
                conversation_id: "tenant-chat-1".into(),
                provider: "chatgpt".into(),
                thread_url: "https://chatgpt.com/c/private-thread".into(),
                synced_through_message_id: Some("message-1".into()),
                model: Some("gpt-4o".into()),
                updated_at: 100,
            },
        )
        .unwrap();

    assert!(memory
        .build_context_for_owner("bravo", Some("tenant-chat-1"), &[])
        .is_err());
    assert!(memory
        .has_history_for_owner("alpha", "tenant-chat-1")
        .unwrap());
    assert!(!memory
        .has_history_for_owner("bravo", "tenant-chat-1")
        .unwrap());
    assert!(memory
        .export_conversation_for_owner("bravo", "tenant-chat-1")
        .is_err());
    assert!(memory
        .list_conversations_for_owner("bravo", 100, 0)
        .unwrap()
        .is_empty());
    assert_eq!(
        memory
            .get_provider_for_owner("alpha", "tenant-chat-1")
            .unwrap()
            .as_deref(),
        Some("chatgpt")
    );
    assert!(memory
        .get_provider_for_owner("bravo", "tenant-chat-1")
        .unwrap()
        .is_none());
    assert!(!memory
        .delete_conversation_for_owner("bravo", "tenant-chat-1")
        .unwrap());
    assert!(memory
        .get_provider_thread_for_owner("bravo", "tenant-chat-1", "chatgpt")
        .unwrap()
        .is_none());
    assert!(memory
        .store_turn_for_owner(
            "bravo",
            "tenant-chat-1".into(),
            vec![m("user", "attempted takeover")],
            m("assistant", "must not persist"),
            "claude".into(),
            "tab-other".into(),
        )
        .is_err());
    assert_eq!(
        memory
            .export_conversation_for_owner("alpha", "tenant-chat-1")
            .unwrap()
            .unwrap()
            .messages
            .last()
            .unwrap()
            .content,
        "private answer"
    );
    assert!(memory
        .delete_conversation_for_owner("alpha", "tenant-chat-1")
        .unwrap());
}

#[test]
fn schema_v3_migration_assigns_existing_conversations_to_local_owner() {
    let fixture = Fixture::new();
    {
        let connection = rusqlite::Connection::open(fixture.db()).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE conversations (
                    id TEXT PRIMARY KEY NOT NULL,
                    provider TEXT,
                    tab_id TEXT,
                    last_activity_at INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE messages (
                    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                    position INTEGER NOT NULL CHECK(position >= 0),
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    PRIMARY KEY(conversation_id, position)
                 );
                 CREATE TABLE metadata (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
                 CREATE TABLE provider_threads (
                    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                    provider TEXT NOT NULL,
                    thread_url TEXT NOT NULL,
                    synced_through_message_id TEXT,
                    model TEXT,
                    updated_at INTEGER NOT NULL,
                    PRIMARY KEY(conversation_id, provider)
                 );
                 INSERT INTO conversations(id,provider,tab_id,last_activity_at)
                    VALUES('legacy-v3','chatgpt','tab-v3',123);
                 INSERT INTO messages(conversation_id,position,role,content)
                    VALUES('legacy-v3',0,'user','preserve this turn');
                 INSERT INTO metadata(key,value) VALUES('legacy_import_complete','1');
                 PRAGMA application_id=1128748877;
                 PRAGMA user_version=3;",
            )
            .unwrap();
    }

    let memory = open(&fixture.db(), None).unwrap();
    let owner: String = memory
        .lock()
        .unwrap()
        .query_row(
            "SELECT owner_key_id FROM conversations WHERE id='legacy-v3'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(owner, "local");
    assert_eq!(
        memory
            .export_conversation_for_owner("local", "legacy-v3")
            .unwrap()
            .unwrap()
            .messages[0]
            .content,
        "preserve this turn"
    );
    let version: i64 = memory
        .lock()
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 6);
}

#[test]
fn context_budget_estimate_handles_multibyte_text_conservatively() {
    let ascii = "hello world";
    let cjk = "你好世界你好世界";
    let emoji = "🙂🙂🙂🙂";
    assert!(
        crate::context::estimate_message_tokens(cjk)
            >= crate::context::estimate_message_tokens(ascii)
    );
    assert!(crate::context::estimate_message_tokens(emoji) > 0);

    let fixture = Fixture::new();
    let mut memory = fixture.open();
    memory.max_tokens = 12;
    memory
        .store_turn(
            "unicode-budget".into(),
            vec![m("user", "你好世界你好世界你好世界你好世界")],
            m("assistant", "🙂🙂🙂🙂🙂🙂🙂🙂"),
            "chatgpt".into(),
            "tab".into(),
        )
        .unwrap();
    let (_, context) = memory.build_context(Some("unicode-budget"), &[]).unwrap();
    assert!(!context.is_empty());
    assert!(
        context
            .iter()
            .map(|msg| crate::context::estimate_message_tokens(&msg.content) + 8)
            .sum::<usize>()
            <= 12
            || context.len() == 1
    );
}

#[test]
fn rolling_summary_persists_and_context_keeps_recent_messages() {
    let fixture = Fixture::new();
    let memory = fixture.open();

    for index in 0..12 {
        memory
            .store_turn(
                "rolling-summary".into(),
                vec![m(
                    "user",
                    &format!("user turn {index}: details that should remain in durable history"),
                )],
                m(
                    "assistant",
                    &format!("assistant response {index}: durable response details"),
                ),
                "chatgpt".into(),
                "tab".into(),
            )
            .unwrap();
    }

    let (prompt, context) = memory.build_context(Some("rolling-summary"), &[]).unwrap();
    assert!(
        prompt.contains("Conversation summary of earlier turns:"),
        "long conversations should include the persisted rolling summary"
    );
    assert!(
        context
            .iter()
            .any(|message| message.content.contains("user turn 11")),
        "the latest user turn must remain verbatim"
    );
    assert!(
        context
            .iter()
            .any(|message| message.content.contains("assistant response 11")),
        "the latest assistant response must remain verbatim"
    );

    drop(memory);
    let reopened = fixture.open();
    let (reopened_prompt, _) = reopened
        .build_context(Some("rolling-summary"), &[])
        .unwrap();
    assert!(
        reopened_prompt.contains("Conversation summary of earlier turns:"),
        "summary metadata should survive database restart"
    );
}

#[test]
fn schema_v4_migration_adds_summary_table_without_losing_tenant_data() {
    let fixture = Fixture::new();
    let connection = Connection::open(fixture.db()).unwrap();
    connection
        .execute_batch(
            "PRAGMA application_id=0x4347574d;
             PRAGMA user_version=4;
             CREATE TABLE conversations (
                 id TEXT PRIMARY KEY NOT NULL,
                 provider TEXT,
                 tab_id TEXT,
                 last_activity_at INTEGER NOT NULL DEFAULT 0,
                 owner_key_id TEXT NOT NULL DEFAULT 'local'
             );
             CREATE TABLE messages (
                 conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                 position INTEGER NOT NULL CHECK(position >= 0),
                 role TEXT NOT NULL,
                 content TEXT NOT NULL,
                 PRIMARY KEY(conversation_id, position)
             );
             CREATE TABLE metadata (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
             CREATE TABLE provider_threads (
                 conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                 provider TEXT NOT NULL,
                 thread_url TEXT NOT NULL,
                 synced_through_message_id TEXT,
                 model TEXT,
                 updated_at INTEGER NOT NULL,
                 PRIMARY KEY(conversation_id, provider)
             );
             CREATE INDEX conversations_owner_key_id_idx ON conversations(owner_key_id, id);
             INSERT INTO metadata(key,value) VALUES('legacy_import_complete','1');
             INSERT INTO conversations(id,provider,tab_id,last_activity_at,owner_key_id)
                 VALUES('v4-tenant','chatgpt','tab-v4',123,'tenant-a');
             INSERT INTO messages(conversation_id,position,role,content)
                 VALUES('v4-tenant',0,'user','preserve this v4 message');",
        )
        .unwrap();
    drop(connection);

    let memory = open(&fixture.db(), None).unwrap();
    let version: i64 = memory
        .lock()
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 6);
    let owner: String = memory
        .lock()
        .unwrap()
        .query_row(
            "SELECT owner_key_id FROM conversations WHERE id='v4-tenant'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(owner, "tenant-a");
    let summary_table: i64 = memory
        .lock()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='conversation_summaries'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(summary_table, 1);
    assert_eq!(
        memory
            .export_conversation_for_owner("tenant-a", "v4-tenant")
            .unwrap()
            .unwrap()
            .messages[0]
            .content,
        "preserve this v4 message"
    );
}

#[test]
fn context_snapshot_returns_stored_transcript_for_provider_retry_reuse() {
    let fixture = Fixture::new();
    let memory = fixture.open();
    let stored = vec![
        m("user", "previous question"),
        m("assistant", "previous answer"),
    ];
    memory
        .store_turn_for_owner_with_tab_id(
            "key:transcript",
            "transcript-conversation".to_owned(),
            vec![stored[0].clone()],
            stored[1].clone(),
            "chatgpt".to_owned(),
            None,
        )
        .unwrap();

    let (prompt, _windowed, has_history, transcript) = memory
        .build_context_for_owner_with_transcript(
            "key:transcript",
            Some("transcript-conversation"),
            &[m("user", "next question")],
        )
        .unwrap();

    assert!(has_history);
    assert!(prompt.contains("previous question"));
    assert_eq!(transcript, stored);
}

#[tokio::test]
async fn async_turn_persistence_runs_and_commits_transactionally() {
    let fixture = Fixture::new();
    let memory = Arc::new(fixture.open());
    memory
        .clone()
        .store_turn_for_owner_with_tab_id_blocking(
            "key:async".to_owned(),
            "async-persisted-conversation".to_owned(),
            vec![m("user", "hello from async persistence")],
            m("assistant", "persisted response"),
            "chatgpt".to_owned(),
            None,
        )
        .await
        .unwrap();

    let conversation = memory
        .conversation_for_owner("key:async", "async-persisted-conversation")
        .unwrap()
        .unwrap();
    assert_eq!(
        conversation.messages,
        vec![
            m("user", "hello from async persistence"),
            m("assistant", "persisted response")
        ]
    );
    assert_eq!(conversation.tab_id, None);
}

#[test]
fn completed_turn_without_real_tab_does_not_persist_synthetic_tab_id() {
    let fixture = Fixture::new();
    let memory = fixture.open();
    memory
        .store_turn_for_owner_with_tab_id(
            "key:test",
            "conversation-without-tab".into(),
            vec![m("user", "hello")],
            m("assistant", "hi"),
            "chatgpt".into(),
            None,
        )
        .unwrap();

    let conversation = memory
        .conversation_for_owner("key:test", "conversation-without-tab")
        .unwrap()
        .unwrap();
    assert_eq!(conversation.tab_id, None);
}

#[test]
fn file_backed_database_uses_wal_and_configured_busy_timeout() {
    use super::sqlite::open;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after UNIX epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "chat-gateway-sqlite-settings-{}-{nonce}.db",
        std::process::id()
    ));
    let connection = open(Some(&path), None).expect("database should initialize");

    let journal_mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal mode should be readable");
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");

    let busy_timeout_ms: i64 = connection
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .expect("busy timeout should be readable");
    assert_eq!(busy_timeout_ms, 30_000);

    drop(connection);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
}
