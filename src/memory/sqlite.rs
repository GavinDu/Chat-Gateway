use super::{ChatMessage, Conversation};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::{collections::HashMap, fs, path::Path, time::Duration};

const VERSION: i64 = 3;
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
const SCHEMA: &str = "
    CREATE TABLE conversations (
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
";

pub(super) fn open(path: Option<&Path>, legacy: Option<&Path>) -> Result<Connection> {
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
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        tx.pragma_update(None, "user_version", VERSION)?;
    } else if version == 1 && app == APPLICATION_ID {
        // Version 2 adds provider-thread mappings; version 3 adds activity timestamps.
        tx.execute_batch(PROVIDER_THREADS_SCHEMA)?;
        tx.execute_batch(
            "ALTER TABLE conversations ADD COLUMN last_activity_at INTEGER NOT NULL DEFAULT 0;",
        )?;
        tx.execute_batch(
            "UPDATE conversations SET last_activity_at = unixepoch() WHERE last_activity_at = 0;",
        )?;
        tx.pragma_update(None, "user_version", VERSION)?;
    } else if version == 2 && app == APPLICATION_ID {
        tx.execute_batch(
            "ALTER TABLE conversations ADD COLUMN last_activity_at INTEGER NOT NULL DEFAULT 0;",
        )?;
        tx.execute_batch(
            "UPDATE conversations SET last_activity_at = unixepoch() WHERE last_activity_at = 0;",
        )?;
        tx.pragma_update(None, "user_version", VERSION)?;
    } else if version != VERSION || app != APPLICATION_ID {
        bail!("unsupported conversation schema version {version} (expected {VERSION}) or application id {app}");
    }
    validate_schema(&tx)?;
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
                    for (id, conversation) in snapshot {
                        if id != conversation.id {
                            bail!("legacy conversation key/id mismatch for '{id}'");
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
    // Version 2 is owned by this application. Fail closed if its definitions
    // were changed, including constraints that SELECT alone cannot validate.
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(SCHEMA)?;
    for table in ["conversations", "messages", "metadata", "provider_threads"] {
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
    Ok(())
}

pub(super) fn load(connection: &Connection, id: &str) -> Result<Option<Conversation>> {
    let affinity = connection
        .query_row(
            "SELECT provider, tab_id FROM conversations WHERE id=?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()?;
    let Some((provider, tab_id)) = affinity else {
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
        tab_id,
    }))
}

pub(super) fn append(
    connection: &Connection,
    conversation: &Conversation,
    first: usize,
) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64;
    connection.execute(
        "INSERT INTO conversations(id,provider,tab_id,last_activity_at) VALUES(?1,?2,?3,?4)
         ON CONFLICT(id) DO UPDATE SET provider=excluded.provider, tab_id=excluded.tab_id,
             last_activity_at=excluded.last_activity_at",
        params![conversation.id, conversation.provider, conversation.tab_id, now],
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
    Ok(())
}
