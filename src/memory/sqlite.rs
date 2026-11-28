use super::{ChatMessage, Conversation};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::{collections::HashMap, fs, path::Path, time::Duration};

const VERSION: i64 = 12;
const APPLICATION_ID: i64 = 0x4347574d; // CGWM
const PROVIDER_THREADS_SCHEMA: &str = "
    CREATE TABLE provider_threads (
        conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        provider TEXT NOT NULL,
        thread_url TEXT NOT NULL,
        synced_through_message_id TEXT,
        model TEXT,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY(conversation_id, provider)
    );
";
const ROLLING_SUMMARIES_SCHEMA: &str = "
    CREATE TABLE conversation_summaries (
        conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        summary TEXT NOT NULL,
        covered_through_position INTEGER NOT NULL CHECK(covered_through_position >= 0),
        updated_at INTEGER NOT NULL
    );
";
const MEMORY_SCHEMA: &str = "
    CREATE TABLE memory_settings (
        owner_key_id TEXT PRIMARY KEY NOT NULL,
        enabled INTEGER NOT NULL CHECK(enabled IN (0, 1)),
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE memory_items (
        id TEXT PRIMARY KEY NOT NULL,
        owner_key_id TEXT NOT NULL,
        memory_key TEXT NOT NULL,
        value TEXT NOT NULL,
        source_conversation_id TEXT,
        confidence REAL NOT NULL CHECK(confidence >= 0.0 AND confidence <= 1.0),
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        expires_at INTEGER,
        CHECK(length(trim(memory_key)) > 0),
        CHECK(length(trim(value)) > 0),
        UNIQUE(owner_key_id, memory_key)
    );
    CREATE INDEX memory_items_owner_updated_idx ON memory_items(owner_key_id, updated_at DESC);
    CREATE INDEX memory_items_owner_key_idx ON memory_items(owner_key_id, memory_key);
";
// Private monotonic counters. Trigger updates are part of the originating
// statement; overflow fails the integer CHECK rather than wrapping.
const MEMORY_REVISION_SCHEMA: &str = "
    ALTER TABLE memory_settings ADD COLUMN consent_epoch INTEGER NOT NULL DEFAULT 0
        CHECK(typeof(consent_epoch) = 'integer' AND consent_epoch >= 0);
    ALTER TABLE memory_items ADD COLUMN revision INTEGER NOT NULL DEFAULT 0
        CHECK(typeof(revision) = 'integer' AND revision >= 0);
    CREATE TRIGGER memory_consent_on_insert AFTER INSERT ON memory_settings
    WHEN NEW.enabled = 1
    BEGIN
        UPDATE memory_settings SET consent_epoch = 1 WHERE owner_key_id = NEW.owner_key_id;
    END;
    CREATE TRIGGER memory_consent_on_update AFTER UPDATE OF enabled ON memory_settings
    WHEN NEW.enabled != OLD.enabled
    BEGIN
        UPDATE memory_settings SET consent_epoch = OLD.consent_epoch + 1
        WHERE owner_key_id = NEW.owner_key_id;
    END;
    CREATE TRIGGER memory_revision_on_update AFTER UPDATE OF
        id, owner_key_id, memory_key, value, source_conversation_id, confidence,
        created_at, updated_at, expires_at ON memory_items
    BEGIN
        UPDATE memory_items SET revision = OLD.revision + 1 WHERE id = NEW.id;
    END;
";
// Private durable identity, never serialized as part of a conversation.
const IDENTITY_SCHEMA: &str = "
    CREATE TABLE conversation_identities (
        conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        incarnation TEXT NOT NULL UNIQUE DEFAULT (lower(hex(randomblob(16))))
            CHECK(length(incarnation) = 32 AND incarnation NOT GLOB '*[^0-9a-f]*'),
        generation INTEGER NOT NULL DEFAULT 0
            CHECK(typeof(generation) = 'integer' AND generation >= 0)
    );
    CREATE TRIGGER conversation_identity_on_insert AFTER INSERT ON conversations
    BEGIN
        INSERT INTO conversation_identities(conversation_id) VALUES(NEW.id);
    END;
";
// Bounded, expiring leases for absent explicit IDs. Creation invalidates all
// leases for that exact ID, including a different owner's competing lease;
// deletion does not affect leases for unrelated IDs.
const ADMISSION_SCHEMA: &str = "
    CREATE TABLE conversation_admissions (
        owner_key_id TEXT NOT NULL,
        conversation_id TEXT NOT NULL,
        token TEXT NOT NULL UNIQUE DEFAULT (lower(hex(randomblob(16))))
            CHECK(length(token) = 32 AND token NOT GLOB '*[^0-9a-f]*'),
        expires_at INTEGER NOT NULL CHECK(typeof(expires_at) = 'integer'),
        PRIMARY KEY(owner_key_id, conversation_id)
    );
    CREATE INDEX conversation_admissions_id_idx ON conversation_admissions(conversation_id);
    CREATE INDEX conversation_admissions_expiry_idx ON conversation_admissions(expires_at);
    CREATE TRIGGER conversation_admission_on_insert AFTER INSERT ON conversations
    BEGIN
        DELETE FROM conversation_admissions WHERE conversation_id = NEW.id;
    END;
";
const SCHEMA: &str = "
    CREATE TABLE conversations (
        id TEXT PRIMARY KEY NOT NULL,
        provider TEXT,
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
    CREATE TABLE conversation_summaries (
        conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        summary TEXT NOT NULL,
        covered_through_position INTEGER NOT NULL CHECK(covered_through_position >= 0),
        updated_at INTEGER NOT NULL
    );

    CREATE TABLE memory_settings (
        owner_key_id TEXT PRIMARY KEY NOT NULL,
        enabled INTEGER NOT NULL CHECK(enabled IN (0, 1)),
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE memory_items (
        id TEXT PRIMARY KEY NOT NULL,
        owner_key_id TEXT NOT NULL,
        memory_key TEXT NOT NULL,
        value TEXT NOT NULL,
        source_conversation_id TEXT,
        confidence REAL NOT NULL CHECK(confidence >= 0.0 AND confidence <= 1.0),
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        expires_at INTEGER,
        CHECK(length(trim(memory_key)) > 0),
        CHECK(length(trim(value)) > 0),
        UNIQUE(owner_key_id, memory_key)
    );
    CREATE INDEX memory_items_owner_updated_idx ON memory_items(owner_key_id, updated_at DESC);
    CREATE INDEX memory_items_owner_key_idx ON memory_items(owner_key_id, memory_key);
";

pub(super) fn open(path: Option<&Path>, legacy: Option<&Path>) -> Result<Connection> {
    let file_backed = path.is_some();
    let mut connection = match path {
        Some(path) => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            fs::create_dir_all(parent)?;
            let mut options = fs::OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
                // Reject symlinks rather than changing permissions on their targets.
                if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
                    bail!(
                        "conversation database must not be a symlink: {}",
                        path.display()
                    );
                }
            }
            let file = options
                .open(path)
                .with_context(|| format!("opening conversation database {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
            drop(file);
            Connection::open(path)
                .with_context(|| format!("opening conversation database {}", path.display()))?
        }
        None => Connection::open_in_memory()?,
    };
    initialize(&mut connection, legacy).with_context(|| {
        format!(
            "conversation database {} failed validation/migration; preserve it and restore a backup",
            path.map(|path| path.display().to_string())
                .unwrap_or_else(|| ":memory:".into())
        )
    })?;
    // Validate existing database bytes before changing journal mode. In particular,
    // malformed files must fail closed with the migration context and stay intact.
    if file_backed {
        let journal_mode: String =
            connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            bail!("expected file-backed conversation database to use WAL mode, got {journal_mode}");
        }
    }
    Ok(connection)
}

fn initialize(connection: &mut Connection, legacy: Option<&Path>) -> Result<()> {
    connection.busy_timeout(Duration::from_secs(30))?;
    connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")?;
    // integrity_check can return multiple rows when it finds multiple
    // problems; inspecting only the first row could accidentally accept a
    // damaged database if the first result happened to be "ok".
    let integrity = connection
        .prepare("PRAGMA integrity_check")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if integrity.as_slice() != ["ok"] {
        bail!("SQLite integrity check failed: {}", integrity.join("; "));
    }
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let needs_synthetic_tab_cleanup = (1..=7).contains(&version);
    let app: i64 = tx.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    if version == 0 {
        let tables: i64 = tx.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if tables != 0 || app != 0 {
            bail!("unversioned/nonempty SQLite database; refusing to overwrite");
        }
        tx.execute_batch(SCHEMA)?;
        tx.execute_batch(IDENTITY_SCHEMA)?;
        tx.execute_batch(ADMISSION_SCHEMA)?;
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        tx.pragma_update(None, "user_version", VERSION)?;
    } else if version == 1 && app == APPLICATION_ID {
        // Migrate provider-thread mappings, retention timestamps, and tenant ownership.
        tx.execute_batch(PROVIDER_THREADS_SCHEMA)?;
        tx.execute_batch(
            "ALTER TABLE conversations ADD COLUMN last_activity_at INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE conversations ADD COLUMN owner_key_id TEXT NOT NULL DEFAULT 'local';
             UPDATE conversations SET last_activity_at = unixepoch() WHERE last_activity_at = 0;",
        )?;
        tx.execute_batch(ROLLING_SUMMARIES_SCHEMA)?;
        tx.execute_batch(MEMORY_SCHEMA)?;
        tx.pragma_update(None, "user_version", VERSION)?;
    } else if version == 2 && app == APPLICATION_ID {
        tx.execute_batch(
            "ALTER TABLE conversations ADD COLUMN last_activity_at INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE conversations ADD COLUMN owner_key_id TEXT NOT NULL DEFAULT 'local';
             UPDATE conversations SET last_activity_at = unixepoch() WHERE last_activity_at = 0;",
        )?;
        tx.execute_batch(ROLLING_SUMMARIES_SCHEMA)?;
        tx.execute_batch(MEMORY_SCHEMA)?;
        tx.pragma_update(None, "user_version", VERSION)?;
    } else if version == 3 && app == APPLICATION_ID {
        tx.execute_batch(
            "ALTER TABLE conversations ADD COLUMN owner_key_id TEXT NOT NULL DEFAULT 'local';",
        )?;
        tx.execute_batch(ROLLING_SUMMARIES_SCHEMA)?;
        tx.execute_batch(MEMORY_SCHEMA)?;
        tx.pragma_update(None, "user_version", VERSION)?;
    } else if version == 4 && app == APPLICATION_ID {
        tx.execute_batch(ROLLING_SUMMARIES_SCHEMA)?;
        tx.execute_batch(MEMORY_SCHEMA)?;
        tx.pragma_update(None, "user_version", VERSION)?;
    } else if version == 5 && app == APPLICATION_ID {
        tx.execute_batch(MEMORY_SCHEMA)?;
        tx.pragma_update(None, "user_version", VERSION)?;
    } else if version == 6 && app == APPLICATION_ID {
        tx.pragma_update(None, "user_version", VERSION)?;
    } else if version == 7 && app == APPLICATION_ID {
        // The v8 column removal is performed after legacy synthetic values are cleaned.
    } else if version == 8 && app == APPLICATION_ID {
        // Version 9 invalidates legacy markers without changing table definitions.
    } else if version == 9 && app == APPLICATION_ID {
        // Version 10 introduces durable conversation identities.
    } else if version == 10 && app == APPLICATION_ID {
        // Version 11 introduces bounded absent-ID admissions.
    } else if version == 11 && app == APPLICATION_ID {
        // Version 12 adds private memory consent and item revision counters.
    } else if version != VERSION || app != APPLICATION_ID {
        bail!("unsupported conversation schema version {version} (expected {VERSION}) or application id {app}");
    }
    // Remove historical synthetic tab IDs only when migrating an existing schema.
    // Running this on every startup would erase live conversation affinity written
    // after the migration had already completed.
    if needs_synthetic_tab_cleanup {
        tx.execute(
            "UPDATE conversations SET tab_id = NULL WHERE substr(tab_id, 1, 4) = 'tab-'",
            [],
        )?;
    }
    // Remove the obsolete affinity column only after legacy synthetic values
    // have been cleared. Versions 1–7 all contain this column; a new
    // database is created without it by SCHEMA.
    if needs_synthetic_tab_cleanup {
        tx.execute_batch("ALTER TABLE conversations DROP COLUMN tab_id;")?;
        // Version 7 does not run one of the earlier migration branches that
        // already advances user_version. Commit the schema and version together.
        tx.pragma_update(None, "user_version", VERSION)?;
    }
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS conversations_owner_key_id_idx ON conversations(owner_key_id, id);",
    )?;
    if (1..10).contains(&version) {
        tx.execute_batch(IDENTITY_SCHEMA)?;
        tx.execute(
            "INSERT INTO conversation_identities(conversation_id)
             SELECT id FROM conversations",
            [],
        )?;
    }
    if (1..11).contains(&version) {
        tx.execute_batch(ADMISSION_SCHEMA)?;
    }
    if (0..12).contains(&version) {
        tx.execute_batch(MEMORY_REVISION_SCHEMA)?;
    }
    validate_schema(&tx)?;
    tx.execute(
        "DELETE FROM conversation_admissions WHERE expires_at <= unixepoch()",
        [],
    )?;
    if (1..9).contains(&version) {
        // Older releases accepted client-supplied sync claims. There is no
        // reliable provenance for existing markers, so preserve the mappings
        // but require a fresh replay before any future Delta.
        tx.execute("UPDATE provider_threads SET synced_through_message_id = NULL WHERE synced_through_message_id IS NOT NULL", [])?;
    }
    if (1..VERSION).contains(&version) {
        tx.pragma_update(None, "user_version", VERSION)?;
    }
    let imported: Option<String> = tx
        .query_row(
            "SELECT value FROM metadata WHERE key='legacy_import_complete'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if imported.as_deref().is_some_and(|value| value != "1") {
        bail!("invalid legacy import completion marker");
    }
    if imported.is_none() {
        let count: i64 =
            tx.query_row("SELECT count(*) FROM conversations", [], |row| row.get(0))?;
        if count != 0 {
            bail!("missing import marker in populated conversation database");
        }
        if let Some(path) = legacy {
            match fs::read(path) {
                Ok(bytes) => {
                    let snapshot: HashMap<String, Conversation> = serde_json::from_slice(&bytes)
                        .with_context(|| format!("importing legacy snapshot {}", path.display()))?;
                    for (id, mut conversation) in snapshot {
                        if id != conversation.id {
                            bail!("legacy conversation key/id mismatch for '{id}'");
                        }
                        // Apply the same rule as the schema-v7 migration to JSON
                        // imports into a brand-new database. There is no prior
                        // schema version to trigger the migration cleanup here.
                        if conversation
                            .tab_id
                            .as_deref()
                            .is_some_and(|tab_id| tab_id.starts_with("tab-"))
                        {
                            conversation.tab_id = None;
                        }
                        append(&tx, &conversation, 0)?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("reading legacy conversation snapshot"),
            }
        }
        tx.execute(
            "INSERT INTO metadata(key,value) VALUES('legacy_import_complete','1')",
            [],
        )?;
    }
    let missing_identity = tx
        .prepare(
            "SELECT 1 FROM conversations c LEFT JOIN conversation_identities i
             ON i.conversation_id = c.id WHERE i.conversation_id IS NULL LIMIT 1",
        )?
        .exists([])?;
    if missing_identity {
        bail!("conversation database has missing durable identities");
    }
    let foreign_key_failure = tx.prepare("PRAGMA foreign_key_check")?.exists([])?;
    if foreign_key_failure {
        bail!("conversation database has invalid foreign keys");
    }
    let ids = tx
        .prepare("SELECT id FROM conversations")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        load(&tx, &id)?;
    }
    tx.commit()?;
    Ok(())
}

fn validate_schema(connection: &Connection) -> Result<()> {
    // The current schema is owned by this application. Fail closed if its definitions
    // were changed, including constraints that SELECT alone cannot validate.
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(SCHEMA)?;
    expected.execute_batch(IDENTITY_SCHEMA)?;
    expected.execute_batch(ADMISSION_SCHEMA)?;
    expected.execute_batch(MEMORY_REVISION_SCHEMA)?;
    for table in [
        "conversations",
        "messages",
        "metadata",
        "provider_threads",
        "conversation_summaries",
        "memory_settings",
        "memory_items",
        "conversation_identities",
        "conversation_admissions",
    ] {
        let sql = |database: &Connection| -> Result<String> {
            Ok(database.query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get::<_, String>(0),
            )?)
        };
        let normalize = |sql: String| sql.split_whitespace().collect::<String>().to_lowercase();
        if normalize(sql(connection)?) != normalize(sql(&expected)?) {
            bail!("invalid schema definition for table '{table}'; expected version {VERSION}");
        }
    }
    for trigger in [
        "conversation_identity_on_insert",
        "conversation_admission_on_insert",
        "memory_consent_on_insert",
        "memory_consent_on_update",
        "memory_revision_on_update",
    ] {
        let trigger_sql = |database: &Connection| -> Result<String> {
            Ok(database.query_row(
                "SELECT sql FROM sqlite_master WHERE type='trigger' AND name=?1",
                [trigger],
                |row| row.get(0),
            )?)
        };
        let normalize = |sql: String| sql.split_whitespace().collect::<String>().to_lowercase();
        if normalize(trigger_sql(connection)?) != normalize(trigger_sql(&expected)?) {
            bail!("invalid schema trigger '{trigger}'");
        }
    }
    Ok(())
}

pub(super) fn load(connection: &Connection, id: &str) -> Result<Option<Conversation>> {
    let provider = connection
        .query_row(
            "SELECT provider FROM conversations WHERE id=?1",
            [id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    let Some(provider) = provider else {
        return Ok(None);
    };
    let mut statement = connection.prepare(
        "SELECT position, role, content FROM messages WHERE conversation_id=?1 ORDER BY position",
    )?;
    let rows = statement.query_map([id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            ChatMessage {
                role: row.get(1)?,
                content: row.get(2)?,
            },
        ))
    })?;
    let mut messages = Vec::new();
    for row in rows {
        let (position, message) = row?;
        if position != messages.len() as i64 {
            bail!("invalid message ordering in conversation '{id}' at position {position}");
        }
        messages.push(message);
    }
    Ok(Some(Conversation {
        id: id.to_owned(),
        messages,
        provider,
        // Compatibility field only; browser tab identifiers are no longer persisted.
        tab_id: None,
    }))
}

pub(super) fn append(
    connection: &Connection,
    conversation: &Conversation,
    first: usize,
) -> Result<()> {
    append_owned(connection, conversation, first, "local")
}

pub(super) fn append_owned(
    connection: &Connection,
    conversation: &Conversation,
    first: usize,
    owner_key_id: &str,
) -> Result<()> {
    let existing_owner: Option<String> = connection
        .query_row(
            "SELECT owner_key_id FROM conversations WHERE id = ?1",
            [&conversation.id],
            |row| row.get(0),
        )
        .optional()?;
    if existing_owner
        .as_deref()
        .is_some_and(|existing| existing != owner_key_id)
    {
        bail!("conversation belongs to another API key");
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64;
    connection.execute(
        "INSERT INTO conversations(id,provider,last_activity_at,owner_key_id)
         VALUES(?1,?2,?3,?4)
         ON CONFLICT(id) DO UPDATE SET provider=excluded.provider,
             last_activity_at=excluded.last_activity_at",
        params![conversation.id, conversation.provider, now, owner_key_id],
    )?;
    let mut insert = connection.prepare(
        "INSERT INTO messages(conversation_id,position,role,content) VALUES(?1,?2,?3,?4)",
    )?;
    for (position, message) in conversation.messages.iter().enumerate().skip(first) {
        insert.execute(params![
            conversation.id,
            position as i64,
            message.role,
            message.content
        ])?;
    }
    let updated = connection.execute(
        "UPDATE conversation_identities SET generation = generation + 1
         WHERE conversation_id = ?1",
        [&conversation.id],
    )?;
    if updated != 1 {
        bail!("conversation durable identity missing during append");
    }
    Ok(())
}

#[cfg(test)]
#[allow(dead_code)] // This module is also compiled through the standalone SQLite benchmark.
mod concurrency_configuration_tests {
    #[test]
    fn file_backed_database_enforces_expected_concurrency_pragmas() -> super::Result<()> {
        let path = std::env::temp_dir().join(format!(
            "chat-gateway-sqlite-pragmas-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let connection = super::open(Some(&path), None)?;

        let journal_mode: String =
            connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        let busy_timeout_ms: i64 =
            connection.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
        let foreign_keys: i64 =
            connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        let synchronous: i64 = connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
        let schema_version: i64 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let application_id: i64 =
            connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;

        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        assert_eq!(busy_timeout_ms, 30_000);
        assert_eq!(foreign_keys, 1);
        assert_eq!(synchronous, 2, "durable writes require synchronous=FULL");
        assert_eq!(schema_version, super::VERSION);
        assert_eq!(application_id, super::APPLICATION_ID);

        drop(connection);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
        Ok(())
    }

    #[test]
    fn independent_connections_can_open_the_same_wal_database() -> super::Result<()> {
        let path = std::env::temp_dir().join(format!(
            "chat-gateway-sqlite-concurrent-open-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let first = super::open(Some(&path), None)?;
        let second = super::open(Some(&path), None)?;

        let first_mode: String = first.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        let second_mode: String = second.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        assert_eq!(first_mode.to_ascii_lowercase(), "wal");
        assert_eq!(second_mode.to_ascii_lowercase(), "wal");

        // SQLite connection-local settings must be applied to every connection,
        // not just the connection that performed the initial schema setup.
        for connection in [&first, &second] {
            let busy_timeout_ms: i64 =
                connection.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
            let foreign_keys: i64 =
                connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
            let synchronous: i64 =
                connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
            assert_eq!(busy_timeout_ms, 30_000);
            assert_eq!(foreign_keys, 1);
            assert_eq!(synchronous, 2, "each connection must use synchronous=FULL");
        }

        drop(second);
        drop(first);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
        Ok(())
    }

    #[test]
    fn concurrent_writers_serialize_with_busy_timeout_without_losing_rows() -> super::Result<()> {
        use std::sync::mpsc;
        use std::time::Duration;

        let path = std::env::temp_dir().join(format!(
            "chat-gateway-sqlite-concurrent-writers-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let mut first = super::open(Some(&path), None)?;
        let mut second = super::open(Some(&path), None)?;
        let first_tx = first.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        first_tx.execute(
            "INSERT INTO conversations(id,provider,last_activity_at,owner_key_id)
             VALUES ('writer-first', NULL, 0, 'local')",
            [],
        )?;

        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let writer_path = path.clone();
        let second_writer = std::thread::spawn(move || -> super::Result<()> {
            started_tx.send(()).expect("test receiver stays connected");
            let transaction =
                second.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            transaction.execute(
                "INSERT INTO conversations(id,provider,last_activity_at,owner_key_id)
                 VALUES ('writer-second', NULL, 0, 'local')",
                [],
            )?;
            transaction.commit()?;
            finished_tx.send(()).expect("test receiver stays connected");
            Ok(())
        });

        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second writer should start");
        // The first transaction intentionally holds SQLite's writer lock. A
        // correctly configured busy timeout keeps the second writer blocked
        // until the first transaction releases that lock.
        if finished_rx.recv_timeout(Duration::from_millis(100)).is_ok() {
            anyhow::bail!("second writer finished while the first writer held the lock");
        }
        first_tx.commit()?;
        second_writer
            .join()
            .expect("second writer thread must not panic")?;

        let verifier = super::open(Some(&writer_path), None)?;
        let rows: i64 = verifier.query_row(
            "SELECT COUNT(*) FROM conversations WHERE id IN ('writer-first', 'writer-second')",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(rows, 2, "both concurrent writes must be durable");

        drop(verifier);
        drop(first);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
        Ok(())
    }

    #[test]
    fn wal_reader_does_not_observe_uncommitted_writer_data() -> super::Result<()> {
        let path = std::env::temp_dir().join(format!(
            "chat-gateway-sqlite-isolation-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let mut writer = super::open(Some(&path), None)?;
        let reader = super::open(Some(&path), None)?;
        let transaction =
            writer.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO conversations(id,provider,last_activity_at,owner_key_id)
             VALUES (?1, NULL, 0, 'local')",
            ["isolation-check"],
        )?;

        let before_commit: i64 = reader.query_row(
            "SELECT COUNT(*) FROM conversations WHERE id=?1",
            ["isolation-check"],
            |row| row.get(0),
        )?;
        assert_eq!(before_commit, 0, "reader must not see uncommitted writes");

        transaction.commit()?;
        let after_commit: i64 = reader.query_row(
            "SELECT COUNT(*) FROM conversations WHERE id=?1",
            ["isolation-check"],
            |row| row.get(0),
        )?;
        assert_eq!(after_commit, 1, "committed writes must become visible");

        drop(reader);
        drop(writer);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
        Ok(())
    }
}
