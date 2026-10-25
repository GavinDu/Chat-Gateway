use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::{CStr, CString},
    os::raw::{c_char, c_int, c_void},
    path::{Path, PathBuf},
    ptr,
    sync::Mutex,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage { pub role: String, pub content: String }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub messages: Vec<ChatMessage>,
    pub provider: Option<String>,
    pub tab_id: Option<String>,
}

#[repr(C)] struct Sqlite3 { _private: [u8; 0] }
#[repr(C)] struct Sqlite3Stmt { _private: [u8; 0] }

#[link(name = "sqlite3")]
unsafe extern "C" {
    fn sqlite3_open_v2(filename: *const c_char, db: *mut *mut Sqlite3, flags: c_int, vfs: *const c_char) -> c_int;
    fn sqlite3_close_v2(db: *mut Sqlite3) -> c_int;
    fn sqlite3_errmsg(db: *mut Sqlite3) -> *const c_char;
    fn sqlite3_exec(db: *mut Sqlite3, sql: *const c_char, callback: Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>, arg: *mut c_void, errmsg: *mut *mut c_char) -> c_int;
    fn sqlite3_free(ptr: *mut c_void);
    fn sqlite3_prepare_v2(db: *mut Sqlite3, sql: *const c_char, bytes: c_int, stmt: *mut *mut Sqlite3Stmt, tail: *mut *const c_char) -> c_int;
    fn sqlite3_step(stmt: *mut Sqlite3Stmt) -> c_int;
    fn sqlite3_finalize(stmt: *mut Sqlite3Stmt) -> c_int;
    fn sqlite3_column_text(stmt: *mut Sqlite3Stmt, column: c_int) -> *const u8;
    fn sqlite3_column_int(stmt: *mut Sqlite3Stmt, column: c_int) -> c_int;
    fn sqlite3_busy_timeout(db: *mut Sqlite3, milliseconds: c_int) -> c_int;
}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_OPEN_READWRITE: c_int = 2;
const SQLITE_OPEN_CREATE: c_int = 4;
const SQLITE_OPEN_FULLMUTEX: c_int = 0x00010000;

struct Db(*mut Sqlite3);
impl Db {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        if !path.exists() {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
            drop(options.open(path)?);
        }
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        let filename = CString::new(path.to_string_lossy().as_bytes()).map_err(|_| anyhow::anyhow!("database path contains NUL"))?;
        let mut raw = ptr::null_mut();
        let rc = unsafe { sqlite3_open_v2(filename.as_ptr(), &mut raw, SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_FULLMUTEX, ptr::null()) };
        if rc != SQLITE_OK {
            let message = if raw.is_null() { format!("SQLite open error {rc}") } else { unsafe { CStr::from_ptr(sqlite3_errmsg(raw)).to_string_lossy().into_owned() } };
            if !raw.is_null() { unsafe { sqlite3_close_v2(raw); } }
            anyhow::bail!("opening {}: {message}", path.display());
        }
        let db = Self(raw);
        let rc = unsafe { sqlite3_busy_timeout(db.0, 5000) };
        if rc != SQLITE_OK { anyhow::bail!("setting SQLite busy timeout: {}", db.error()); }
        db.exec("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
        Ok(db)
    }
    fn error(&self) -> String { unsafe { CStr::from_ptr(sqlite3_errmsg(self.0)).to_string_lossy().into_owned() } }
    fn exec(&self, sql: &str) -> anyhow::Result<()> {
        let sql = CString::new(sql).map_err(|_| anyhow::anyhow!("SQL contains NUL"))?;
        let mut error = ptr::null_mut();
        let rc = unsafe { sqlite3_exec(self.0, sql.as_ptr(), None, ptr::null_mut(), &mut error) };
        if rc == SQLITE_OK { return Ok(()); }
        let message = if error.is_null() { self.error() } else {
            let message = unsafe { CStr::from_ptr(error).to_string_lossy().into_owned() };
            unsafe { sqlite3_free(error.cast()); }
            message
        };
        anyhow::bail!("SQLite statement failed: {message}");
    }
    fn prepare(&self, sql: &str) -> anyhow::Result<*mut Sqlite3Stmt> {
        let sql = CString::new(sql).map_err(|_| anyhow::anyhow!("SQL contains NUL"))?;
        let mut stmt = ptr::null_mut();
        let rc = unsafe { sqlite3_prepare_v2(self.0, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()) };
        if rc != SQLITE_OK { anyhow::bail!("preparing SQLite query: {}", self.error()); }
        Ok(stmt)
    }
    fn scalar(&self, sql: &str) -> anyhow::Result<i32> {
        let stmt = self.prepare(sql)?;
        let rc = unsafe { sqlite3_step(stmt) };
        if rc != SQLITE_ROW {
            let error = self.error(); unsafe { sqlite3_finalize(stmt); }
            anyhow::bail!("reading SQLite scalar: {error}");
        }
        let value = unsafe { sqlite3_column_int(stmt, 0) };
        unsafe { sqlite3_finalize(stmt); }
        Ok(value)
    }
}
impl Drop for Db { fn drop(&mut self) { unsafe { sqlite3_close_v2(self.0); } } }
fn quote(value: &str) -> String { format!("'{}'", value.replace('\'', "''")) }
fn column(stmt: *mut Sqlite3Stmt, index: c_int) -> Option<String> {
    let value = unsafe { sqlite3_column_text(stmt, index) };
    if value.is_null() { None } else { Some(unsafe { CStr::from_ptr(value.cast()).to_string_lossy().into_owned() }) }
}

fn read_all(db: &Db) -> anyhow::Result<HashMap<String, Conversation>> {
    let stmt = db.prepare("SELECT id, payload FROM conversations ORDER BY id")?;
    let mut result = HashMap::new();
    loop {
        let rc = unsafe { sqlite3_step(stmt) };
        if rc == SQLITE_DONE { break; }
        if rc != SQLITE_ROW {
            let error = db.error(); unsafe { sqlite3_finalize(stmt); }
            anyhow::bail!("reading SQLite conversations: {error}");
        }
        let id = column(stmt, 0).ok_or_else(|| anyhow::anyhow!("SQLite conversation id is NULL"))?;
        let payload = column(stmt, 1).ok_or_else(|| anyhow::anyhow!("SQLite conversation payload is NULL"))?;
        let conversation: Conversation = serde_json::from_str(&payload)
            .map_err(|e| anyhow::anyhow!("invalid conversation payload for '{id}': {e}"))?;
        if conversation.id != id { unsafe { sqlite3_finalize(stmt); } anyhow::bail!("SQLite key/id mismatch for '{id}'"); }
        result.insert(id, conversation);
    }
    unsafe { sqlite3_finalize(stmt); }
    Ok(result)
}

fn initialize_database(path: &Path, legacy: Option<&Path>) -> anyhow::Result<HashMap<String, Conversation>> {
    let db = Db::open(path).map_err(|e| anyhow::anyhow!("open database {}: {e}", path.display()))?;
    let version = db.scalar("PRAGMA user_version")?;
    if version > 1 { anyhow::bail!("unsupported SQLite schema version {version} in {}", path.display()); }
    if version == 0 {
        db.exec("BEGIN IMMEDIATE;
          CREATE TABLE IF NOT EXISTS conversations(id TEXT PRIMARY KEY NOT NULL, payload TEXT NOT NULL);
          PRAGMA user_version=1;
          COMMIT;")?;
    }
    let mut loaded = read_all(&db)?;
    if loaded.is_empty() {
        if let Some(source) = legacy.filter(|p| p.exists()) {
            let bytes = std::fs::read(source).map_err(|e| anyhow::anyhow!("reading legacy snapshot {}: {e}", source.display()))?;
            let imported: HashMap<String, Conversation> = serde_json::from_slice(&bytes)
                .map_err(|e| anyhow::anyhow!("parsing legacy snapshot {}: {e}", source.display()))?;
            for (key, value) in &imported {
                if key != &value.id { anyhow::bail!("legacy key/id mismatch for '{key}' in {}", source.display()); }
            }
            db.exec("BEGIN IMMEDIATE")?;
            let result = (|| -> anyhow::Result<()> {
                for value in imported.values() { write_one(&db, value)?; }
                db.exec("COMMIT")
            })();
            if let Err(e) = result {
                let _ = db.exec("ROLLBACK");
                return Err(e.context("legacy import rolled back; source JSON was preserved"));
            }
            loaded = read_all(&db)?;
        }
    }
    Ok(loaded)
}

fn write_one(db: &Db, conversation: &Conversation) -> anyhow::Result<()> {
    let payload = serde_json::to_string(conversation)?;
    db.exec(&format!("INSERT INTO conversations(id,payload) VALUES({}, {}) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload",
        quote(&conversation.id), quote(&payload)))
}
fn persist(path: &Path, conversation: &Conversation) -> anyhow::Result<()> {
    let db = Db::open(path)?;
    db.exec("BEGIN IMMEDIATE")?;
    let result = write_one(&db, conversation).and_then(|()| db.exec("COMMIT"));
    if let Err(error) = result { let _ = db.exec("ROLLBACK"); return Err(error.context("SQLite transaction rolled back")); }
    Ok(())
}

/// Unified memory layer: SQLite is the durable source of truth; the DashMap
/// is a process-local read cache. All mutations are serialized to preserve order.
pub struct UnifiedMemory {
    max_tokens: usize,
    conversations: DashMap<String, Conversation>,
    persistence_path: Option<PathBuf>,
    persistence_lock: Mutex<()>,
}

impl UnifiedMemory {
    pub fn new(max_tokens: usize) -> Self {
        Self::with_optional_persistence(max_tokens, None).expect("in-memory store initialization cannot fail")
    }
    pub fn from_env(max_tokens: usize) -> anyhow::Result<Self> {
        let configured = std::env::var_os("CHAT_GATEWAY_MEMORY_PATH").map(PathBuf::from);
        let (database, legacy) = match configured {
            Some(path) if path.extension().and_then(|e| e.to_str()) == Some("json") => (path.with_extension("sqlite3"), Some(path)),
            Some(path) => {
                let legacy = path.parent().unwrap_or(Path::new(".")).join("conversations.json");
                (path, Some(legacy))
            }
            None => (PathBuf::from("data/conversations.sqlite3"), Some(PathBuf::from("data/conversations.json"))),
        };
        Self::with_paths(max_tokens, Some(database), legacy)
    }
    fn with_optional_persistence(max_tokens: usize, path: Option<PathBuf>) -> anyhow::Result<Self> {
        let legacy = path.as_ref().map(|p| p.with_extension("json"));
        Self::with_paths(max_tokens, path, legacy)
    }
    fn with_paths(max_tokens: usize, path: Option<PathBuf>, legacy: Option<PathBuf>) -> anyhow::Result<Self> {
        let loaded = if let Some(path) = path.as_deref() {
            initialize_database(path, legacy.as_deref()).map_err(|e| anyhow::anyhow!("failed to restore conversation memory from {}: {e}", path.display()))?
        } else { HashMap::new() };
        let conversations = DashMap::new();
        for (id, conversation) in loaded {
            if id != conversation.id { anyhow::bail!("conversation key/id mismatch for '{id}'"); }
            conversations.insert(id, conversation);
        }
        Ok(Self { max_tokens, conversations, persistence_path: path, persistence_lock: Mutex::new(()) })
    }
    pub fn build_context(&self, conversation_id: Option<&str>, new_messages: &[ChatMessage]) -> (String, Vec<ChatMessage>) {
        let mut full = vec![];
        if let Some(cid) = conversation_id { if let Some(conv) = self.conversations.get(cid) { full.extend(conv.messages.clone()); } }
        full.extend(messages_after_overlap(&full, new_messages));
        let mut tokens = 0;
        let mut windowed = Vec::new();
        for msg in full.iter().rev() {
            let count = (msg.content.len() as f32 / 4.0) as usize + 8;
            if tokens + count > self.max_tokens && !windowed.is_empty() { break; }
            tokens += count; windowed.push(msg.clone());
        }
        windowed.reverse();
        let prompt = windowed.iter().map(|m| format!("{}: {}", m.role, m.content)).collect::<Vec<_>>().join("\n");
        (prompt, windowed)
    }
    pub fn store_turn(&self, conversation_id: String, user_msgs: Vec<ChatMessage>, assistant_msg: ChatMessage, provider: String, tab_id: String) -> anyhow::Result<()> {
        let _guard = self.persistence_lock.lock().map_err(|e| anyhow::anyhow!("conversation lock poisoned: {e}"))?;
        let previous = self.conversations.get(&conversation_id).map(|v| v.value().clone());
        let mut next = previous.unwrap_or_else(|| Conversation { id: conversation_id.clone(), messages: vec![], provider: None, tab_id: None });
        next.messages.extend(messages_after_overlap(&next.messages, &user_msgs));
        next.messages.push(assistant_msg);
        next.provider = Some(provider); next.tab_id = Some(tab_id);
        if let Some(path) = self.persistence_path.as_deref() { persist(path, &next).context("persisting conversation turn")?; }
        self.conversations.insert(conversation_id, next);
        Ok(())
    }
    pub fn delete_conversation(&self, conversation_id: &str) -> anyhow::Result<bool> {
        let _guard = self.persistence_lock.lock().map_err(|e| anyhow::anyhow!("conversation lock poisoned: {e}"))?;
        if !self.conversations.contains_key(conversation_id) { return Ok(false); }
        if let Some(path) = self.persistence_path.as_deref() {
            let db = Db::open(path)?;
            db.exec("BEGIN IMMEDIATE")?;
            let result = db.exec(&format!("DELETE FROM conversations WHERE id={}", quote(conversation_id))).and_then(|()| db.exec("COMMIT"));
            if let Err(e) = result { let _ = db.exec("ROLLBACK"); return Err(e.context("persisting conversation deletion")); }
        }
        self.conversations.remove(conversation_id);
        Ok(true)
    }
    pub fn has_history(&self, cid: &str) -> bool { self.conversations.get(cid).map(|c| !c.messages.is_empty()).unwrap_or(false) }
    pub fn get_tab(&self, cid: &str) -> Option<String> { self.conversations.get(cid).and_then(|c| c.tab_id.clone()) }
    pub fn get_provider(&self, cid: &str) -> Option<String> { self.conversations.get(cid).and_then(|c| c.provider.clone()) }
}

fn messages_after_overlap(stored: &[ChatMessage], incoming: &[ChatMessage]) -> Vec<ChatMessage> {
    if incoming.is_empty() { return Vec::new(); }
    if stored.is_empty() { return incoming.to_vec(); }
    let prefix = stored.iter().zip(incoming.iter()).take_while(|(a,b)| a.role == b.role && a.content == b.content).count();
    if prefix == stored.len() && incoming.len() >= stored.len() { return incoming[prefix..].to_vec(); }
    for overlap in (1..=stored.len().min(incoming.len())).rev() {
        if stored[stored.len()-overlap..].iter().zip(incoming[..overlap].iter()).all(|(a,b)| a.role == b.role && a.content == b.content) {
            return incoming[overlap..].to_vec();
        }
    }
    incoming.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};
    fn m(role: &str, content: &str) -> ChatMessage { ChatMessage { role: role.into(), content: content.into() } }
    fn dbpath(label: &str) -> PathBuf { std::env::temp_dir().join(format!("gateway-{label}-{}.sqlite3", uuid::Uuid::new_v4())) }
    fn clean(p: &Path) { let _=std::fs::remove_file(p); let _=std::fs::remove_file(p.with_extension("sqlite3-wal")); let _=std::fs::remove_file(p.with_extension("sqlite3-shm")); }

    #[test] fn restart_restores_messages_and_affinity() {
        let p=dbpath("restart");
        { let x=UnifiedMemory::with_optional_persistence(8000,Some(p.clone())).unwrap(); x.store_turn("c".into(),vec![m("user","hi")],m("assistant","hello"),"chatgpt".into(),"tab".into()).unwrap(); }
        let x=UnifiedMemory::with_optional_persistence(8000,Some(p.clone())).unwrap();
        assert!(x.has_history("c")); assert_eq!(x.get_provider("c").as_deref(),Some("chatgpt")); assert_eq!(x.get_tab("c").as_deref(),Some("tab"));
        clean(&p);
    }
    #[test] fn imports_json_and_preserves_source() {
        let p=dbpath("import"); let source=p.with_extension("json");
        let mut values=HashMap::new(); values.insert("old".into(),Conversation{id:"old".into(),messages:vec![m("user","saved")],provider:Some("p".into()),tab_id:Some("t".into())});
        let bytes=serde_json::to_vec(&values).unwrap(); std::fs::write(&source,&bytes).unwrap();
        let x=UnifiedMemory::with_optional_persistence(8000,Some(p.clone())).unwrap(); assert!(x.has_history("old")); assert_eq!(std::fs::read(&source).unwrap(),bytes);
        drop(x); clean(&p); let _=std::fs::remove_file(source);
    }
    #[test] fn deletion_survives_restart() {
        let p=dbpath("delete");
        { let x=UnifiedMemory::with_optional_persistence(8000,Some(p.clone())).unwrap(); x.store_turn("gone".into(),vec![m("user","secret")],m("assistant","answer"),"p".into(),"t".into()).unwrap(); assert!(x.delete_conversation("gone").unwrap()); }
        let x=UnifiedMemory::with_optional_persistence(8000,Some(p.clone())).unwrap(); assert!(!x.has_history("gone")); clean(&p);
    }
    #[test] fn failed_database_open_does_not_mutate_memory() {
        let p=dbpath("rollback"); let x=UnifiedMemory::with_optional_persistence(8000,Some(p.clone())).unwrap();
        std::fs::remove_file(&p).unwrap(); std::fs::create_dir_all(&p).unwrap();
        assert!(x.store_turn("no".into(),vec![m("user","hi")],m("assistant","ok"),"p".into(),"t".into()).is_err()); assert!(!x.has_history("no")); let _=std::fs::remove_dir_all(p);
    }
    #[cfg(unix)] #[test] fn database_permissions_are_owner_only() {
        use std::os::unix::fs::PermissionsExt; let p=dbpath("mode");
        let x=UnifiedMemory::with_optional_persistence(8000,Some(p.clone())).unwrap(); x.store_turn("private".into(),vec![m("user","x")],m("assistant","y"),"p".into(),"t".into()).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode()&0o777,0o600); drop(x); clean(&p);
    }
    #[test] fn corrupt_database_fails_closed() {
        let p=dbpath("corrupt"); std::fs::write(&p,b"not sqlite").unwrap(); assert!(UnifiedMemory::with_optional_persistence(8000,Some(p.clone())).is_err()); clean(&p);
    }
    #[test] fn unsupported_schema_fails_closed() {
        let p=dbpath("version"); let db=Db::open(&p).unwrap(); db.exec("PRAGMA user_version=99").unwrap(); drop(db);
        assert!(UnifiedMemory::with_optional_persistence(8000,Some(p.clone())).is_err()); clean(&p);
    }
    #[test] fn concurrent_writes_preserve_different_conversations() {
        let p=dbpath("parallel"); let x=Arc::new(UnifiedMemory::with_optional_persistence(8000,Some(p.clone())).unwrap());
        let jobs=(0..8).map(|i| {let x=x.clone();thread::spawn(move||x.store_turn(format!("c{i}"),vec![m("user",&format!("u{i}"))],m("assistant",&format!("a{i}")),"p".into(),"t".into()).unwrap())}).collect::<Vec<_>>();
        for j in jobs {j.join().unwrap();} drop(x); let x=UnifiedMemory::with_optional_persistence(8000,Some(p.clone())).unwrap(); for i in 0..8 {assert!(x.has_history(&format!("c{i}")));} clean(&p);
    }
    #[test] fn concurrent_writes_to_same_conversation_preserve_ordered_messages() {
        let p=dbpath("same"); let x=Arc::new(UnifiedMemory::with_optional_persistence(8000,Some(p.clone())).unwrap());
        let jobs=(0..6).map(|i| {let x=x.clone();thread::spawn(move||x.store_turn("same".into(),vec![m("user",&format!("u{i}"))],m("assistant",&format!("a{i}")),"p".into(),"t".into()).unwrap())}).collect::<Vec<_>>();
        for j in jobs {j.join().unwrap();} assert_eq!(x.conversations.get("same").unwrap().messages.len(),12); drop(x);
        let x=UnifiedMemory::with_optional_persistence(8000,Some(p.clone())).unwrap(); assert_eq!(x.conversations.get("same").unwrap().messages.len(),12); clean(&p);
    }
    #[test] fn overlap_deduplicates_full_and_sliding_transcripts() {
        let stored=vec![m("user","hello"),m("assistant","hi")];
        assert_eq!(messages_after_overlap(&stored,&[m("user","hello"),m("assistant","hi"),m("user","next")]),vec![m("user","next")]);
        assert_eq!(messages_after_overlap(&stored,&[m("assistant","hi"),m("user","next")]),vec![m("user","next")]);
    }
}
