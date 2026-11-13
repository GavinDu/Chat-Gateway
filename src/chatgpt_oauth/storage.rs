//! Separate authenticated namespace, sharing vault rotation and backup support.
use super::{failure, Store};
use crate::{chat_backend::GatewayError, session};
use fs2::FileExt;
use std::{
    collections::HashMap,
    fs::OpenOptions,
    path::{Path, PathBuf},
    time::Duration,
};

const FILE_NAME: &str = "chatgpt-oauth.vault";
const AAD_NAME: &str = "chatgpt-oauth";
pub(crate) struct Guard {
    file: std::fs::File,
    root: PathBuf,
}
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}
pub(crate) fn try_lock() -> anyhow::Result<Guard> {
    try_lock_at(Path::new("sessions"))
}
pub(super) fn try_lock_at(root: &Path) -> anyhow::Result<Guard> {
    session::ensure_private_session_dir_sync(root)?;
    let path = root.join("chatgpt-oauth.lock");
    if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
        anyhow::bail!("invalid ChatGPT credential lock");
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    file.try_lock_exclusive()?;
    Ok(Guard {
        file,
        root: root.to_owned(),
    })
}
pub(super) async fn lock() -> Result<Guard, GatewayError> {
    lock_at(Path::new("sessions")).await
}
pub(super) async fn lock_at(root: &Path) -> Result<Guard, GatewayError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match try_lock_at(root) {
            Ok(guard) => return Ok(guard),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock) =>
            {
                if tokio::time::Instant::now() >= deadline {
                    return Err(failure("ChatGPT credential store is busy"));
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            Err(_) => return Err(failure("ChatGPT credential store unavailable")),
        }
    }
}
pub(super) fn load(guard: &Guard) -> Result<Store, GatewayError> {
    if std::env::var("CHAT_GATEWAY_SESSION_STORE").as_deref() != Ok("vault") {
        return Err(failure("ChatGPT OAuth requires encrypted vault storage"));
    }
    let key =
        session::vault_key_from_env().map_err(|_| failure("ChatGPT vault key unavailable"))?;
    load_with_key(guard, &key)
}
pub(super) fn load_with_key(guard: &Guard, key: &[u8; 32]) -> Result<Store, GatewayError> {
    read_with_key(&guard.root, key)
}
fn read_with_key(root: &Path, key: &[u8; 32]) -> Result<Store, GatewayError> {
    let path = root.join(FILE_NAME);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(_) => return Err(failure("ChatGPT credentials unavailable")),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 262_144 {
        return Err(failure("Invalid ChatGPT credential file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(failure("ChatGPT credentials must be private"));
        }
    }
    let bytes = zeroize::Zeroizing::new(
        std::fs::read(&path).map_err(|_| failure("ChatGPT credentials unavailable"))?,
    );
    let file = session::decrypt_session_snapshot(&bytes, AAD_NAME, key)
        .map_err(|_| failure("ChatGPT credentials cannot be authenticated"))?;
    serde_json::from_str(
        file.local_storage
            .get("profile_store")
            .ok_or_else(|| failure("ChatGPT credential record is invalid"))?,
    )
    .map_err(|_| failure("ChatGPT credential record is invalid"))
}
pub(super) fn save(guard: &Guard, store: &Store) -> Result<(), GatewayError> {
    if std::env::var("CHAT_GATEWAY_SESSION_STORE").as_deref() != Ok("vault") {
        return Err(failure("ChatGPT OAuth requires encrypted vault storage"));
    }
    let key =
        session::vault_key_from_env().map_err(|_| failure("ChatGPT vault key unavailable"))?;
    save_with_key(guard, store, &key)
}
pub(super) fn save_with_key(
    guard: &Guard,
    store: &Store,
    key: &[u8; 32],
) -> Result<(), GatewayError> {
    let serialized = serde_json::to_string(store)
        .map_err(|_| failure("ChatGPT credential serialization failed"))?;
    if serialized.len() > 200_000 {
        return Err(failure("Too many ChatGPT account profiles"));
    }
    let file = session::SessionFile {
        cookies: vec![],
        local_storage: HashMap::from([("profile_store".into(), serialized)]),
        ts: chrono::Utc::now().to_rfc3339(),
    };
    let bytes = session::encrypt_session_snapshot(&file, AAD_NAME, key)
        .map_err(|_| failure("ChatGPT credential encryption failed"))?;
    session::write_session_file_atomically(&guard.root.join(FILE_NAME), &bytes)
        .map_err(|_| failure("ChatGPT credentials could not be saved"))
}

pub(super) fn linked_snapshot() -> Result<bool, GatewayError> {
    if std::env::var("CHAT_GATEWAY_SESSION_STORE").as_deref() != Ok("vault") {
        return Ok(false);
    }
    let key =
        session::vault_key_from_env().map_err(|_| failure("ChatGPT vault key unavailable"))?;
    let store = read_with_key(Path::new("sessions"), &key)?;
    Ok(store
        .profiles
        .get(&store.active)
        .and_then(|r| r.credential.as_ref())
        .is_some_and(super::Credential::plan_enabled))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_encrypted_profiles_roundtrip_without_touching_browser_cookies() {
        let root = std::env::temp_dir().join(format!("cg-oauth-store-{}", uuid::Uuid::new_v4()));
        let guard = try_lock_at(&root).unwrap();
        let original = b"browser cookie vault fixture";
        std::fs::write(root.join("chatgpt.vault"), original).unwrap();
        let mut store = Store::default();
        let secret = "fixture-value-not-a-real-access-token";
        store.profiles.insert(
            "work".into(),
            super::super::Registration {
                client_id: "fixture-client".into(),
                subject: "fixture-subject".into(),
                issuer: super::super::AUTH_ORIGIN.into(),
                credential: Some(super::super::Credential {
                    authorization_id: "fixture-grant".into(),
                    issuer: super::super::AUTH_ORIGIN.into(),
                    subject: "fixture-subject".into(),
                    email: "fixture@example.test".into(),
                    access_token: secret.into(),
                    refresh_token: "fixture-refresh".into(),
                    id_token: "fixture-id".into(),
                    scopes: vec![super::super::DIRECT_SCOPE.into()],
                    expires_at: super::super::now() + 300,
                }),
            },
        );
        store.active = "work".into();
        let key = [11u8; 32];
        save_with_key(&guard, &store, &key).unwrap();
        let bytes = std::fs::read(root.join(FILE_NAME)).unwrap();
        assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()));
        assert!(load_with_key(&guard, &[12u8; 32]).is_err());
        let restored = load_with_key(&guard, &key).unwrap();
        assert_eq!(restored.host_id, store.host_id);
        assert_eq!(restored.active, "work");
        assert_eq!(
            restored.profiles["work"]
                .credential
                .as_ref()
                .unwrap()
                .access_token,
            secret
        );
        assert_eq!(std::fs::read(root.join("chatgpt.vault")).unwrap(), original);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(root.join(FILE_NAME))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(try_lock_at(&root).is_err());
        drop(guard);
        assert!(try_lock_at(&root).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_accepted_as_lock_or_credential_files() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("cg-oauth-symlink-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("target"), b"fixture").unwrap();
        symlink(root.join("target"), root.join("chatgpt-oauth.lock")).unwrap();
        assert!(try_lock_at(&root).is_err());
        std::fs::remove_file(root.join("chatgpt-oauth.lock")).unwrap();
        let guard = try_lock_at(&root).unwrap();
        symlink(root.join("target"), root.join(FILE_NAME)).unwrap();
        assert!(load_with_key(&guard, &[11u8; 32]).is_err());
        drop(guard);
        std::fs::remove_dir_all(root).unwrap();
    }
}
